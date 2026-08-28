use std::{
    error::Error as _,
    fmt,
    io::{self, Write},
};

use firestone_core::{
    DoctorCheckId, DoctorReport, DoctorStatus, ErrorInfo, ErrorKind, Event, EventSink,
    FirestoneError, Level, MachineSummary, MachineView, Unit,
};

use unicode_width::UnicodeWidthChar;

/// Selects structured output or the human CLI renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
}

/// Inputs that affect rendering but are not part of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub mode: OutputMode,
    pub quiet: bool,
    pub verbosity: u8,
    pub stderr_is_terminal: bool,
}

impl RenderOptions {
    #[must_use]
    pub const fn human(quiet: bool, stderr_is_terminal: bool) -> Self {
        Self {
            mode: OutputMode::Human,
            quiet,
            verbosity: 0,
            stderr_is_terminal,
        }
    }

    #[must_use]
    pub const fn with_verbosity(mut self, verbosity: u8) -> Self {
        self.verbosity = verbosity;
        self
    }

    #[must_use]
    pub const fn json() -> Self {
        Self {
            mode: OutputMode::Json,
            quiet: false,
            verbosity: 0,
            stderr_is_terminal: false,
        }
    }
}
impl Default for RenderOptions {
    fn default() -> Self {
        Self::human(false, false)
    }
}

/// An `EventSink` whose two streams are supplied by the caller.
///
/// Keeping the writers generic makes stream routing explicit and lets callers
/// choose their own buffering. Each event is flushed before `emit` returns.
pub struct Renderer<Stdout, Stderr> {
    stdout: Stdout,
    stderr: Stderr,
    options: RenderOptions,
    exit_override: Option<u8>,
}

impl<Stdout, Stderr> Renderer<Stdout, Stderr>
where
    Stdout: Write,
    Stderr: Write,
{
    pub const fn new(stdout: Stdout, stderr: Stderr, options: RenderOptions) -> Self {
        Self {
            stdout,
            stderr,
            options,
            exit_override: None,
        }
    }

    /// Returns a command-specific process status without turning a result into an error.
    #[must_use]
    pub const fn exit_override(&self) -> Option<u8> {
        self.exit_override
    }

    #[cfg(test)]
    #[must_use]
    pub fn into_writers(self) -> (Stdout, Stderr) {
        (self.stdout, self.stderr)
    }

    /// Writes a terminal action error to the stream selected by the output mode.
    ///
    /// JSON errors use the same object shape as the REST API. Human errors are
    /// never hidden by quiet mode.
    pub fn render_error(&mut self, error: &FirestoneError) -> Result<(), FirestoneError> {
        match self.options.mode {
            OutputMode::Json => self.render_json_error(error),
            OutputMode::Human => self.render_human_error(error),
        }
    }

    fn render_event(&mut self, event: Event) -> Result<(), FirestoneError> {
        self.record_process_outcome(&event)?;
        match self.options.mode {
            OutputMode::Json => self.render_json_event(&event),
            OutputMode::Human => self.render_human_event(event),
        }
    }

    fn record_process_outcome(&mut self, event: &Event) -> Result<(), FirestoneError> {
        let Event::Result { action, payload } = event else {
            return Ok(());
        };
        if action != "doctor" {
            return Ok(());
        }
        let report: DoctorReport = serde_json::from_value(payload.clone())
            .map_err(|error| invalid_result_payload("doctor", error))?;
        self.exit_override = report
            .has_failures()
            .then_some(exit_code(ErrorKind::Dependency));
        Ok(())
    }

    fn render_json_event(&mut self, event: &Event) -> Result<(), FirestoneError> {
        serde_json::to_writer(&mut self.stdout, event).map_err(json_output_failure)?;
        finish_record(&mut self.stdout)
    }

    fn render_json_error(&mut self, error: &FirestoneError) -> Result<(), FirestoneError> {
        self.stdout
            .write_all(b"{\"error\":")
            .map_err(write_output_failure)?;
        serde_json::to_writer(&mut self.stdout, &error.info()).map_err(json_output_failure)?;
        self.stdout.write_all(b"}").map_err(write_output_failure)?;
        finish_record(&mut self.stdout)
    }

    fn render_human_error(&mut self, error: &FirestoneError) -> Result<(), FirestoneError> {
        write_line(&mut self.stderr, format_args!("error: {}", error.message()))?;

        let mut source = error.source();
        while let Some(cause) = source {
            write_line(&mut self.stderr, format_args!("cause: {cause}"))?;
            source = cause.source();
        }

        if let Some(hint) = error.hint() {
            write_line(&mut self.stderr, format_args!("hint:  {hint}"))?;
        }

        Ok(())
    }

    fn render_human_event(&mut self, event: Event) -> Result<(), FirestoneError> {
        match event {
            Event::StepStart { id, label } => {
                if self.options.quiet {
                    return Ok(());
                }
                if self.options.stderr_is_terminal {
                    write_line(&mut self.stderr, format_args!("  ⠋ {id:<8} {label}"))
                } else {
                    write_line(&mut self.stderr, format_args!("[{id}] {label}"))
                }
            }
            Event::StepUpdate { id, detail } => {
                if self.options.quiet {
                    return Ok(());
                }
                if self.options.stderr_is_terminal {
                    write_line(&mut self.stderr, format_args!("  ⠸ {id:<8} {detail}"))
                } else {
                    write_line(&mut self.stderr, format_args!("[{id}] {detail}"))
                }
            }
            Event::Progress {
                id,
                done,
                total,
                unit,
            } => {
                if self.options.quiet {
                    return Ok(());
                }
                self.render_progress(&id, done, total, unit)
            }
            Event::StepDone {
                id,
                detail,
                elapsed_ms,
            } => {
                if self.options.quiet {
                    return Ok(());
                }
                self.render_step_done(&id, detail.as_deref(), elapsed_ms)
            }
            Event::StepSkip { id, reason } => {
                if self.options.quiet {
                    return Ok(());
                }
                if self.options.stderr_is_terminal {
                    write_line(&mut self.stderr, format_args!("  - {id:<8} {reason}"))
                } else {
                    write_line(&mut self.stderr, format_args!("[{id}] {reason}"))
                }
            }
            Event::StepFail { id, error } => self.render_step_failure(&id, &error),
            Event::Log { level, message } => {
                let visible = if self.options.quiet {
                    level == Level::Error
                } else {
                    match level {
                        Level::Trace => self.options.verbosity >= 2,
                        Level::Debug => self.options.verbosity >= 1,
                        Level::Info | Level::Warn | Level::Error => true,
                    }
                };
                if !visible {
                    return Ok(());
                }
                self.render_log(level, &message)
            }
            Event::Result { action, payload } => self.render_result(&action, payload),
        }
    }

    fn render_progress(
        &mut self,
        id: &firestone_core::StepId,
        done: u64,
        total: Option<u64>,
        unit: Unit,
    ) -> Result<(), FirestoneError> {
        let marker = if self.options.stderr_is_terminal {
            "  ⠼ "
        } else {
            "["
        };

        match (self.options.stderr_is_terminal, total) {
            (true, Some(total)) => write_line(
                &mut self.stderr,
                format_args!(
                    "{marker}{id:<8} {} / {}",
                    HumanCount::new(done, unit),
                    HumanCount::new(total, unit)
                ),
            ),
            (true, None) => write_line(
                &mut self.stderr,
                format_args!("{marker}{id:<8} {}", HumanCount::new(done, unit)),
            ),
            (false, Some(total)) => write_line(
                &mut self.stderr,
                format_args!(
                    "{marker}{id}] {} / {}",
                    HumanCount::new(done, unit),
                    HumanCount::new(total, unit)
                ),
            ),
            (false, None) => write_line(
                &mut self.stderr,
                format_args!("{marker}{id}] {}", HumanCount::new(done, unit)),
            ),
        }
    }

    fn render_step_done(
        &mut self,
        id: &firestone_core::StepId,
        detail: Option<&str>,
        elapsed_ms: u64,
    ) -> Result<(), FirestoneError> {
        let elapsed = (elapsed_ms > 1_000).then_some(Elapsed(elapsed_ms));

        match (self.options.stderr_is_terminal, detail, elapsed) {
            (true, Some(detail), Some(elapsed)) => write_line(
                &mut self.stderr,
                format_args!("  ✓ {id:<8} {detail} · {elapsed}"),
            ),
            (true, Some(detail), None) => {
                write_line(&mut self.stderr, format_args!("  ✓ {id:<8} {detail}"))
            }
            (true, None, Some(elapsed)) => {
                write_line(&mut self.stderr, format_args!("  ✓ {id:<8} {elapsed}"))
            }
            (true, None, None) => write_line(&mut self.stderr, format_args!("  ✓ {id}")),
            (false, Some(detail), Some(elapsed)) => write_line(
                &mut self.stderr,
                format_args!("[{id}] {detail} · {elapsed}"),
            ),
            (false, Some(detail), None) => {
                write_line(&mut self.stderr, format_args!("[{id}] {detail}"))
            }
            (false, None, Some(elapsed)) => {
                write_line(&mut self.stderr, format_args!("[{id}] done · {elapsed}"))
            }
            (false, None, None) => write_line(&mut self.stderr, format_args!("[{id}] done")),
        }
    }

    fn render_step_failure(
        &mut self,
        id: &firestone_core::StepId,
        error: &ErrorInfo,
    ) -> Result<(), FirestoneError> {
        if self.options.stderr_is_terminal {
            write_line(
                &mut self.stderr,
                format_args!("  ✗ {id:<8} {}", error.message),
            )?;
        } else {
            write_line(
                &mut self.stderr,
                format_args!("[{id}] error: {}", error.message),
            )?;
        }

        if let Some(hint) = &error.hint {
            write_line(&mut self.stderr, format_args!("hint:  {hint}"))?;
        }

        Ok(())
    }

    fn render_log(&mut self, level: Level, message: &str) -> Result<(), FirestoneError> {
        match (self.options.stderr_is_terminal, level) {
            (true, Level::Warn) => write_line(&mut self.stderr, format_args!("  ! {message}")),
            (true, Level::Error) => write_line(&mut self.stderr, format_args!("  ✗ {message}")),
            (true, _) => write_line(&mut self.stderr, format_args!("  · {message}")),
            (false, Level::Trace) => {
                write_line(&mut self.stderr, format_args!("[trace] {message}"))
            }
            (false, Level::Debug) => {
                write_line(&mut self.stderr, format_args!("[debug] {message}"))
            }
            (false, Level::Info) => write_line(&mut self.stderr, format_args!("{message}")),
            (false, Level::Warn) => {
                write_line(&mut self.stderr, format_args!("warning: {message}"))
            }
            (false, Level::Error) => write_line(&mut self.stderr, format_args!("error: {message}")),
        }
    }

    fn render_result(
        &mut self,
        action: &str,
        payload: serde_json::Value,
    ) -> Result<(), FirestoneError> {
        match action {
            "create" | "edit" => Ok(()),
            "list" => {
                let mut machines: Vec<MachineSummary> = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("list", error))?;
                machines.sort_unstable_by(|left, right| left.name.cmp(&right.name));
                write_machine_table(&mut self.stdout, &machines).map_err(write_output_failure)
            }
            "show" => {
                let view: MachineView = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("show", error))?;
                serde_json::to_writer_pretty(&mut self.stdout, &view)
                    .map_err(json_output_failure)?;
                finish_record(&mut self.stdout)
            }
            "doctor" => {
                let report: DoctorReport = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("doctor", error))?;
                write_doctor_report(&mut self.stdout, &report).map_err(write_output_failure)
            }
            _ if payload.is_null() => Ok(()),
            _ => self.render_other_result(payload),
        }
    }

    fn render_other_result(&mut self, payload: serde_json::Value) -> Result<(), FirestoneError> {
        if let serde_json::Value::String(message) = payload {
            return write_line(&mut self.stdout, format_args!("{message}"));
        }

        serde_json::to_writer_pretty(&mut self.stdout, &payload).map_err(json_output_failure)?;
        finish_record(&mut self.stdout)
    }
}

impl<Stdout, Stderr> EventSink for Renderer<Stdout, Stderr>
where
    Stdout: Write + Send,
    Stderr: Write + Send,
{
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
        self.render_event(event)
    }
}

/// Maps a stable core error category to the CLI status from SPEC §15.5.
#[must_use]
pub const fn exit_code(kind: ErrorKind) -> u8 {
    match kind {
        ErrorKind::Generic | ErrorKind::NotRunning => 1,
        ErrorKind::Usage | ErrorKind::InvalidSpec => 2,
        ErrorKind::NotFound => 3,
        ErrorKind::Conflict
        | ErrorKind::AlreadyExists
        | ErrorKind::AlreadyRunning
        | ErrorKind::Busy => 4,
        ErrorKind::Dependency => 5,
        ErrorKind::Timeout => 6,
        ErrorKind::Checksum => 7,
        ErrorKind::Interrupted => 130,
    }
}

/// Convenience form of [`exit_code`] for an operation error.
#[must_use]
pub const fn error_exit_code(error: &FirestoneError) -> u8 {
    exit_code(error.kind())
}

fn write_doctor_report<W: Write>(writer: &mut W, report: &DoctorReport) -> io::Result<()> {
    for check in &report.checks {
        writeln!(
            writer,
            "{} {}: {}",
            doctor_status_label(check.status),
            doctor_check_id_label(check.id),
            check.reason
        )?;
        if let Some(fix) = &check.fix {
            writeln!(writer, "  fix: {fix}")?;
        }
        if let Some(hint) = &check.hint {
            writeln!(writer, "  hint: {hint}")?;
        }
    }
    Ok(())
}

const fn doctor_status_label(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Ok => "ok",
        DoctorStatus::Warn => "warn",
        DoctorStatus::Fail => "fail",
    }
}

const fn doctor_check_id_label(id: DoctorCheckId) -> &'static str {
    match id {
        DoctorCheckId::HostArch => "host_arch",
        DoctorCheckId::Kvm => "kvm",
        DoctorCheckId::NestedVirtualization => "nested_virtualization",
        DoctorCheckId::RuntimeDir => "runtime_dir",
        DoctorCheckId::VendoredBinaries => "vendored_binaries",
        DoctorCheckId::Virtiofsd => "virtiofsd",
        DoctorCheckId::Passt => "passt",
        DoctorCheckId::QemuImg => "qemu_img",
        DoctorCheckId::Ssh => "ssh",
        DoctorCheckId::UserNamespaces => "user_namespaces",
        DoctorCheckId::SshKey => "ssh_key",
        DoctorCheckId::DataSpace => "data_space",
        DoctorCheckId::StaleState => "stale_state",
    }
}

fn write_machine_table<W: Write>(writer: &mut W, machines: &[MachineSummary]) -> io::Result<()> {
    let widths = TableWidths::for_machines(machines);

    writeln!(
        writer,
        "{:<name_width$}  {:<status_width$}  {:<image_width$}  {:<cpus_width$}  {:<memory_width$}  {:<uptime_width$}  FORWARDS",
        "NAME",
        "STATUS",
        "IMAGE",
        "CPUS",
        "MEM",
        "UPTIME",
        name_width = widths.name,
        status_width = widths.status,
        image_width = widths.image,
        cpus_width = widths.cpus,
        memory_width = widths.memory,
        uptime_width = widths.uptime,
    )?;

    for machine in machines {
        let uptime = machine.uptime.as_deref().unwrap_or("-");
        write_safe_cell(writer, &machine.name, widths.name)?;
        writer.write_all(b"  ")?;
        write_safe_cell(writer, &machine.status, widths.status)?;
        writer.write_all(b"  ")?;
        write_safe_cell(writer, &machine.image, widths.image)?;
        writer.write_all(b"  ")?;
        write!(writer, "{:<width$}  ", machine.cpus, width = widths.cpus)?;
        write_safe_cell(writer, &machine.memory, widths.memory)?;
        writer.write_all(b"  ")?;
        write_safe_cell(writer, uptime, widths.uptime)?;
        writer.write_all(b"  ")?;

        if machine.forwards.is_empty() {
            writer.write_all(b"-")?;
        } else {
            for (index, forward) in machine.forwards.iter().enumerate() {
                if index != 0 {
                    writer.write_all(b", ")?;
                }
                write_safe_text(writer, forward)?;
            }
        }
        writer.write_all(b"\n")?;
    }

    writer.flush()
}

fn write_safe_cell<W: Write>(writer: &mut W, value: &str, width: usize) -> io::Result<()> {
    write_safe_text(writer, value)?;
    for _ in display_width(value)..width {
        writer.write_all(b" ")?;
    }
    Ok(())
}

fn write_safe_text<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    let mut encoded = [0_u8; 4];
    for character in value.chars() {
        let character = if character.is_control() {
            '\u{fffd}'
        } else {
            character
        };
        writer.write_all(character.encode_utf8(&mut encoded).as_bytes())?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct TableWidths {
    name: usize,
    status: usize,
    image: usize,
    cpus: usize,
    memory: usize,
    uptime: usize,
}

impl TableWidths {
    fn for_machines(machines: &[MachineSummary]) -> Self {
        let mut widths = Self {
            name: "NAME".len(),
            status: "STATUS".len(),
            image: "IMAGE".len(),
            cpus: "CPUS".len(),
            memory: "MEM".len(),
            uptime: "UPTIME".len(),
        };

        for machine in machines {
            widths.name = widths.name.max(display_width(&machine.name));
            widths.status = widths.status.max(display_width(&machine.status));
            widths.image = widths.image.max(display_width(&machine.image));
            widths.cpus = widths.cpus.max(decimal_width(machine.cpus));
            widths.memory = widths.memory.max(display_width(&machine.memory));
            widths.uptime = widths
                .uptime
                .max(machine.uptime.as_deref().map_or(1, display_width));
        }

        widths
    }
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                1
            } else {
                UnicodeWidthChar::width(character).unwrap_or(0)
            }
        })
        .sum()
}

fn decimal_width(value: u8) -> usize {
    match value {
        0..=9 => 1,
        10..=99 => 2,
        100..=u8::MAX => 3,
    }
}

#[derive(Debug, Clone, Copy)]
struct HumanCount {
    value: u64,
    unit: Unit,
}

impl HumanCount {
    const fn new(value: u64, unit: Unit) -> Self {
        Self { value, unit }
    }
}

impl fmt::Display for HumanCount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.unit {
            Unit::Bytes => fmt::Display::fmt(&HumanBytes(self.value), formatter),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct HumanBytes(u64);

impl fmt::Display for HumanBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(&str, u64); 7] = [
            ("EB", 1_000_000_000_000_000_000),
            ("PB", 1_000_000_000_000_000),
            ("TB", 1_000_000_000_000),
            ("GB", 1_000_000_000),
            ("MB", 1_000_000),
            ("kB", 1_000),
            ("B", 1),
        ];

        for (unit, scale) in UNITS {
            if self.0 >= scale {
                let whole = self.0 / scale;
                let remainder = self.0 % scale;
                if scale != 1 && whole < 10 && remainder != 0 {
                    let mut tenths = (remainder * 10 + scale / 2) / scale;
                    let mut rounded_whole = whole;
                    if tenths == 10 {
                        rounded_whole += 1;
                        tenths = 0;
                    }
                    return write!(formatter, "{rounded_whole}.{tenths} {unit}");
                }
                return write!(formatter, "{whole} {unit}");
            }
        }

        formatter.write_str("0 B")
    }
}

#[derive(Debug, Clone, Copy)]
struct Elapsed(u64);

impl fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = (self.0 + 50) / 100;
        write!(formatter, "{}.{:01}s", tenths / 10, tenths % 10)
    }
}

fn write_line<W: Write>(
    writer: &mut W,
    arguments: fmt::Arguments<'_>,
) -> Result<(), FirestoneError> {
    writer
        .write_fmt(arguments)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(write_output_failure)
}

fn finish_record<W: Write>(writer: &mut W) -> Result<(), FirestoneError> {
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(write_output_failure)
}

fn write_output_failure(error: io::Error) -> FirestoneError {
    FirestoneError::new(ErrorKind::Generic, "failed to write command output").with_source(error)
}

fn json_output_failure(error: serde_json::Error) -> FirestoneError {
    FirestoneError::new(ErrorKind::Generic, "failed to encode command output").with_source(error)
}

fn invalid_result_payload(action: &str, error: serde_json::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("invalid {action} result payload"),
    )
    .with_source(error)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, error::Error, io, path::PathBuf};

    use firestone_core::{
        DoctorCheck, DoctorCheckId, DoctorReport, DoctorStatus, EventSink, ImageRef, MachineSpec,
        MachineState, MachineStatus, StateImage, StateVersion, StepId,
    };
    use serde_json::json;

    use super::{
        ErrorInfo, ErrorKind, Event, FirestoneError, Level, MachineSummary, MachineView,
        RenderOptions, Renderer, error_exit_code, exit_code,
    };

    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn json_mode_is_byte_exact_ndjson_on_stdout() -> TestResult {
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), RenderOptions::json());

        renderer.emit(Event::StepDone {
            id: StepId::from("image"),
            detail: Some("cached".to_owned()),
            elapsed_ms: 25,
        })?;
        renderer.emit(Event::Log {
            level: Level::Info,
            message: "ready".to_owned(),
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            stdout,
            b"{\"type\":\"StepDone\",\"id\":\"image\",\"detail\":\"cached\",\"elapsed_ms\":25}\n{\"type\":\"Log\",\"level\":\"info\",\"message\":\"ready\"}\n"
        );
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn human_verbosity_reveals_debug_before_trace() -> TestResult {
        let options = RenderOptions::human(false, false).with_verbosity(1);
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), options);

        for (level, message) in [
            (Level::Trace, "trace detail"),
            (Level::Debug, "debug detail"),
            (Level::Info, "normal detail"),
        ] {
            renderer.emit(Event::Log {
                level,
                message: message.to_owned(),
            })?;
        }

        let (stdout, stderr) = renderer.into_writers();
        assert!(stdout.is_empty());
        assert_eq!(stderr, b"[debug] debug detail\nnormal detail\n");
        Ok(())
    }

    #[test]
    fn human_doctor_result_prints_status_reason_fix_and_hint() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                id: DoctorCheckId::Kvm,
                status: DoctorStatus::Fail,
                reason: "cannot open /dev/kvm".to_owned(),
                fix: Some("sudo usermod -aG kvm $USER".to_owned()),
                hint: Some("log in again".to_owned()),
            }],
        };

        renderer.emit(Event::Result {
            action: "doctor".to_owned(),
            payload: serde_json::to_value(report)?,
        })?;
        assert_eq!(renderer.exit_override(), Some(5));

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            stdout,
            b"fail kvm: cannot open /dev/kvm\n  fix: sudo usermod -aG kvm $USER\n  hint: log in again\n"
        );
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn human_mode_separates_feedback_results_and_errors() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));

        renderer.emit(Event::StepStart {
            id: StepId::from("image"),
            label: "fetching".to_owned(),
        })?;
        renderer.emit(Event::Log {
            level: Level::Info,
            message: "checking cache".to_owned(),
        })?;
        renderer.emit(Event::Result {
            action: "version".to_owned(),
            payload: json!("0.1.0"),
        })?;

        let error = FirestoneError::new(ErrorKind::Dependency, "cannot start ubuntu")
            .with_source(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission denied",
            ))
            .with_hint("run firestone doctor");
        renderer.render_error(&error)?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(stdout, b"0.1.0\n");
        assert_eq!(
            stderr,
            b"[image] fetching\nchecking cache\nerror: cannot start ubuntu\ncause: permission denied\nhint:  run firestone doctor\n"
        );
        assert!(!stderr.contains(&b'\r'));
        assert!(!stderr.contains(&0x1b));
        Ok(())
    }

    #[test]
    fn quiet_keeps_results_and_errors_only() -> TestResult {
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(true, false));

        renderer.emit(Event::StepStart {
            id: StepId::from("disk"),
            label: "creating overlay".to_owned(),
        })?;
        renderer.emit(Event::Log {
            level: Level::Warn,
            message: "slow filesystem".to_owned(),
        })?;
        renderer.emit(Event::StepFail {
            id: StepId::from("disk"),
            error: ErrorInfo {
                kind: ErrorKind::Generic,
                message: "disk failed".to_owned(),
                hint: None,
            },
        })?;
        renderer.emit(Event::Result {
            action: "version".to_owned(),
            payload: json!("0.1.0"),
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(stdout, b"0.1.0\n");
        assert_eq!(stderr, b"[disk] error: disk failed\n");
        Ok(())
    }

    #[test]
    fn list_table_is_stable_sorted_and_never_truncates_names() -> TestResult {
        let machines = vec![
            MachineSummary {
                name: "z".to_owned(),
                status: "stopped".to_owned(),
                image: "debian:12".to_owned(),
                cpus: 4,
                memory: "8G".to_owned(),
                uptime: None,
                forwards: Vec::new(),
            },
            MachineSummary {
                name: "development-machine".to_owned(),
                status: "running!".to_owned(),
                image: "ubuntu:24.04".to_owned(),
                cpus: 2,
                memory: "2G".to_owned(),
                uptime: Some("41s".to_owned()),
                forwards: vec!["8080→80".to_owned(), "8443→443".to_owned()],
            },
        ];
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));

        renderer.emit(Event::Result {
            action: "list".to_owned(),
            payload: serde_json::to_value(machines)?,
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            stdout,
            concat!(
                "NAME                 STATUS    IMAGE         CPUS  MEM  UPTIME  FORWARDS\n",
                "development-machine  running!  ubuntu:24.04  2     2G   41s     8080→80, 8443→443\n",
                "z                    stopped   debian:12     4     8G   -       -\n",
            )
            .as_bytes()
        );
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn list_table_sanitizes_controls_and_measures_terminal_width() -> TestResult {
        let machines = vec![MachineSummary {
            name: "开发".to_owned(),
            status: "run\nning".to_owned(),
            image: "image".to_owned(),
            cpus: 2,
            memory: "2G".to_owned(),
            uptime: None,
            forwards: vec!["1\n2".to_owned()],
        }];
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));

        renderer.emit(Event::Result {
            action: "list".to_owned(),
            payload: serde_json::to_value(machines)?,
        })?;

        let (stdout, stderr) = renderer.into_writers();
        let output = String::from_utf8(stdout)?;
        assert_eq!(output.matches('\n').count(), 2);
        assert!(output.contains("开发  run�ning"));
        assert!(output.contains("1�2"));
        assert_eq!(super::display_width("开发"), 4);
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn create_result_is_silent_in_human_mode() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));

        renderer.emit(Event::Result {
            action: "create".to_owned(),
            payload: json!({"name": "dev"}),
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn show_result_is_deterministic_pretty_json() -> TestResult {
        let spec = MachineSpec {
            image: ImageRef::new("ubuntu:24.04"),
            ..MachineSpec::default()
        };
        let view = MachineView {
            spec,
            state: MachineState {
                version: StateVersion,
                status: MachineStatus::Created,
                image: StateImage {
                    r#ref: "ubuntu:24.04".to_owned(),
                    id: Some("ubuntu-24.04-amd64".to_owned()),
                    sha256: Some("abc123".to_owned()),
                },
                mac: None,
                cid: 3,
                instance_id: None,
                shim_pid: None,
                vmm_pid: None,
                sidecar_pids: BTreeMap::new(),
                runtime_dir: PathBuf::from("/run/user/1000/firestone/dev"),
                started_at: None,
                forwards: Vec::new(),
                degraded: Vec::new(),
                last_exit: None,
            },
        };
        let mut expected = serde_json::to_vec_pretty(&view)?;
        expected.push(b'\n');
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));

        renderer.emit(Event::Result {
            action: "show".to_owned(),
            payload: serde_json::to_value(view)?,
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(stdout, expected);
        assert!(stdout.starts_with(b"{\n  \"spec\": {\n"));
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn exit_codes_match_spec_section_15_5() {
        let cases = [
            (ErrorKind::Generic, 1),
            (ErrorKind::NotRunning, 1),
            (ErrorKind::Usage, 2),
            (ErrorKind::InvalidSpec, 2),
            (ErrorKind::NotFound, 3),
            (ErrorKind::Conflict, 4),
            (ErrorKind::AlreadyExists, 4),
            (ErrorKind::AlreadyRunning, 4),
            (ErrorKind::Busy, 4),
            (ErrorKind::Dependency, 5),
            (ErrorKind::Timeout, 6),
            (ErrorKind::Checksum, 7),
            (ErrorKind::Interrupted, 130),
        ];

        for (kind, expected) in cases {
            assert_eq!(exit_code(kind), expected, "{kind}");
        }

        let error = FirestoneError::new(ErrorKind::Timeout, "timed out");
        assert_eq!(error_exit_code(&error), 6);
    }
}
