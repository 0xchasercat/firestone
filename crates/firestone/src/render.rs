use std::{
    collections::HashMap,
    error::Error as _,
    fmt,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use console::measure_text_width;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

use firestone_core::{
    CatalogEntrySummary, CatalogFirmware, DoctorCheckId, DoctorReport, DoctorStatus, ErrorInfo,
    ErrorKind, Event, EventSink, FirestoneError, Level, LogsResult, MachineRecord, MachineStatus,
    MachineSummary, MachineView, MetricsResult, NetMode, RemoveResult, SshConfigResult,
    StartResult, StepId, StopResult, Unit, VersionResult,
};
use owo_colors::OwoColorize as _;
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
    pub color_enabled: bool,
    pub live_progress: bool,
}

impl RenderOptions {
    #[must_use]
    pub const fn human(quiet: bool, stderr_is_terminal: bool) -> Self {
        Self {
            mode: OutputMode::Human,
            quiet,
            verbosity: 0,
            color_enabled: stderr_is_terminal,
            live_progress: stderr_is_terminal && !quiet,
        }
    }

    #[must_use]
    pub const fn with_verbosity(mut self, verbosity: u8) -> Self {
        self.verbosity = verbosity;
        self
    }

    #[must_use]
    pub const fn with_quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        if quiet {
            self.live_progress = false;
        }
        self
    }

    #[must_use]
    pub const fn with_color(mut self, color_enabled: bool) -> Self {
        self.color_enabled = color_enabled;
        self
    }

    #[must_use]
    pub const fn with_live_progress(mut self, live_progress: bool) -> Self {
        self.live_progress = live_progress && !self.quiet;
        self
    }

    #[must_use]
    pub const fn json() -> Self {
        Self {
            mode: OutputMode::Json,
            quiet: false,
            verbosity: 0,
            color_enabled: false,
            live_progress: false,
        }
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self::human(false, false)
    }
}

const STEP_LABEL_WIDTH: usize = 8;
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const SPINNER_TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProgressLineKind {
    Spinner,
    BytesKnown,
    BytesUnknown,
}

#[derive(Debug, Clone, Copy)]
struct ActiveProgressLine {
    index: usize,
    kind: ProgressLineKind,
}

struct TerminalProgress {
    multi: MultiProgress,
    bars: Vec<ProgressBar>,
    active: HashMap<String, ActiveProgressLine>,
    color_enabled: bool,
}

impl TerminalProgress {
    fn stderr(color_enabled: bool) -> Self {
        Self::with_draw_target(ProgressDrawTarget::stderr_with_hz(13), color_enabled)
    }

    fn with_draw_target(target: ProgressDrawTarget, color_enabled: bool) -> Self {
        Self {
            multi: MultiProgress::with_draw_target(target),
            bars: Vec::new(),
            active: HashMap::new(),
            color_enabled,
        }
    }

    fn start(&mut self, id: &StepId, label: &str) -> Result<(), FirestoneError> {
        if let Some(bar) = self.active_bar(id.as_str()) {
            bar.reset_elapsed();
            bar.set_message(terminal_message(label));
            return Ok(());
        }
        self.add_step(id.as_str(), label).map(|_| ())
    }

    fn update(&mut self, id: &StepId, detail: &str) -> Result<(), FirestoneError> {
        let bar = self.bar_for(id.as_str())?;
        bar.set_message(terminal_message(detail));
        Ok(())
    }

    fn progress(
        &mut self,
        id: &StepId,
        done: u64,
        total: Option<u64>,
    ) -> Result<(), FirestoneError> {
        let bar = self.bar_for(id.as_str())?;
        let kind = if total.is_some() {
            ProgressLineKind::BytesKnown
        } else {
            ProgressLineKind::BytesUnknown
        };
        let current_kind = self.active.get(id.as_str()).map(|line| line.kind);
        if current_kind != Some(kind) {
            bar.set_style(progress_style(kind, self.color_enabled)?);
            if let Some(line) = self.active.get_mut(id.as_str()) {
                line.kind = kind;
            }
        }
        match total {
            Some(total) => bar.set_length(total),
            None => bar.unset_length(),
        }
        bar.set_position(done);
        Ok(())
    }

    fn done(
        &mut self,
        id: &StepId,
        detail: Option<&str>,
        elapsed_ms: u64,
    ) -> Result<(), FirestoneError> {
        let marker = success_marker(self.color_enabled);
        self.settle(id, &marker, detail, Some(elapsed_ms))
    }

    fn skip(&mut self, id: &StepId, reason: &str) -> Result<(), FirestoneError> {
        let marker = skip_marker(self.color_enabled);
        self.settle(id, &marker, Some(reason), None)
    }

    fn fail(&mut self, id: &StepId, error: &ErrorInfo) -> Result<(), FirestoneError> {
        let marker = failure_marker(self.color_enabled);
        self.settle(id, &marker, Some(&error.message), None)?;
        if let Some(hint) = &error.hint {
            self.println(format!("hint:  {}", terminal_text(hint)))?;
        }
        Ok(())
    }

    fn fail_last_active(&mut self, message: &str) -> Result<(), FirestoneError> {
        let id = self
            .active
            .iter()
            .max_by_key(|(_, line)| line.index)
            .map(|(id, _)| id.clone());
        if let Some(id) = id {
            let marker = failure_marker(self.color_enabled);
            self.settle(&StepId::from(id), &marker, Some(message), None)?;
        }
        Ok(())
    }

    fn println(&self, line: impl AsRef<str>) -> Result<(), FirestoneError> {
        self.multi.println(line).map_err(write_output_failure)
    }

    fn clear_active(&mut self) {
        let active = self
            .active
            .drain()
            .map(|(_, line)| line.index)
            .collect::<Vec<_>>();
        for index in active {
            if let Some(bar) = self.bars.get(index) {
                bar.disable_steady_tick();
                bar.finish_and_clear();
            }
        }
    }

    fn settle(
        &mut self,
        id: &StepId,
        marker: &str,
        detail: Option<&str>,
        elapsed_ms: Option<u64>,
    ) -> Result<(), FirestoneError> {
        let bar = self.bar_for(id.as_str())?;
        self.active.remove(id.as_str());
        bar.disable_steady_tick();
        let message = settled_message(detail);
        let prefix = if message.is_empty() {
            terminal_text(id.as_str())
        } else {
            padded_step_id(id.as_str())
        };
        bar.set_prefix(prefix);
        bar.set_style(settled_style(marker, elapsed_ms, self.color_enabled)?);
        bar.finish_with_message(message);
        Ok(())
    }

    fn bar_for(&mut self, id: &str) -> Result<ProgressBar, FirestoneError> {
        match self.active_bar(id) {
            Some(bar) => Ok(bar),
            None => self.add_step(id, ""),
        }
    }

    fn active_bar(&self, id: &str) -> Option<ProgressBar> {
        self.active
            .get(id)
            .and_then(|line| self.bars.get(line.index))
            .cloned()
    }

    fn add_step(&mut self, id: &str, message: &str) -> Result<ProgressBar, FirestoneError> {
        let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        bar.set_style(spinner_style(self.color_enabled)?);
        bar.set_prefix(padded_step_id(id));
        bar.set_message(terminal_message(message));
        let bar = self.multi.add(bar);
        bar.enable_steady_tick(SPINNER_INTERVAL);
        bar.tick();
        let index = self.bars.len();
        self.bars.push(bar.clone());
        self.active.insert(
            id.to_owned(),
            ActiveProgressLine {
                index,
                kind: ProgressLineKind::Spinner,
            },
        );
        Ok(bar)
    }
}

impl Drop for TerminalProgress {
    fn drop(&mut self) {
        self.clear_active();
    }
}

fn spinner_style(color_enabled: bool) -> Result<ProgressStyle, FirestoneError> {
    ProgressStyle::with_template("  {spinner} {prefix}{wide_msg}{short_elapsed}")
        .map(|style| with_progress_keys(style.tick_chars(SPINNER_TICKS), color_enabled))
        .map_err(progress_style_failure)
}

fn progress_style(
    kind: ProgressLineKind,
    color_enabled: bool,
) -> Result<ProgressStyle, FirestoneError> {
    let template = match kind {
        ProgressLineKind::Spinner => "  {spinner} {prefix}{wide_msg}{short_elapsed}",
        ProgressLineKind::BytesKnown => {
            "  {spinner} {prefix}{msg} {wide_bar} {count}/{total_count} {rate}{short_elapsed}"
        }
        ProgressLineKind::BytesUnknown => "  {spinner} {prefix}{msg} {count} {rate}{short_elapsed}",
    };
    ProgressStyle::with_template(template)
        .map(|style| {
            with_progress_keys(
                style.tick_chars(SPINNER_TICKS).progress_chars("━╸─"),
                color_enabled,
            )
        })
        .map_err(progress_style_failure)
}

fn settled_style(
    marker: &str,
    elapsed_ms: Option<u64>,
    color_enabled: bool,
) -> Result<ProgressStyle, FirestoneError> {
    ProgressStyle::with_template(&format!(
        "  {marker} {{prefix}}{{wide_msg}}{{final_elapsed}}"
    ))
    .map(|style| {
        style.with_key(
            "final_elapsed",
            move |_state: &ProgressState, writer: &mut dyn fmt::Write| {
                if let Some(elapsed_ms) = elapsed_ms {
                    if color_enabled {
                        let _ = write!(writer, " · {}", Elapsed(elapsed_ms).dimmed());
                    } else {
                        let _ = write!(writer, " · {}", Elapsed(elapsed_ms));
                    }
                }
            },
        )
    })
    .map_err(progress_style_failure)
}

fn with_progress_keys(style: ProgressStyle, color_enabled: bool) -> ProgressStyle {
    style
        .with_key(
            "short_elapsed",
            move |state: &ProgressState, writer: &mut dyn fmt::Write| {
                let elapsed_ms = u64::try_from(state.elapsed().as_millis()).unwrap_or(u64::MAX);
                if elapsed_ms > 1_000 {
                    if color_enabled {
                        let _ = write!(writer, " · {}", Elapsed(elapsed_ms).dimmed());
                    } else {
                        let _ = write!(writer, " · {}", Elapsed(elapsed_ms));
                    }
                }
            },
        )
        .with_key(
            "count",
            |state: &ProgressState, writer: &mut dyn fmt::Write| {
                let _ = write!(writer, "{}", HumanBytes(state.pos()));
            },
        )
        .with_key(
            "total_count",
            |state: &ProgressState, writer: &mut dyn fmt::Write| {
                let _ = write!(writer, "{}", HumanBytes(state.len().unwrap_or(0)));
            },
        )
        .with_key(
            "rate",
            |state: &ProgressState, writer: &mut dyn fmt::Write| {
                let bytes = state.per_sec().max(0.0).round().min(u64::MAX as f64) as u64;
                let _ = write!(writer, "{}/s", HumanBytes(bytes));
            },
        )
}

fn progress_style_failure(source: indicatif::style::TemplateError) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        "cannot initialize terminal progress renderer",
    )
    .with_source(source)
}

fn terminal_message(value: &str) -> String {
    let value = terminal_text(value);
    if value.is_empty() {
        value
    } else {
        format!(" {value}")
    }
}

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn padded_step_id(id: &str) -> String {
    let mut label = terminal_text(id);
    let padding = STEP_LABEL_WIDTH.saturating_sub(measure_text_width(&label));
    label.extend(std::iter::repeat_n(' ', padding));
    label
}

fn settled_message(detail: Option<&str>) -> String {
    terminal_message(&detail.map_or_else(String::new, terminal_text))
}

fn success_marker(color_enabled: bool) -> String {
    if color_enabled {
        "✓".green().to_string()
    } else {
        "✓".to_owned()
    }
}

fn failure_marker(color_enabled: bool) -> String {
    if color_enabled {
        "✗".red().to_string()
    } else {
        "✗".to_owned()
    }
}

fn skip_marker(color_enabled: bool) -> String {
    if color_enabled {
        "-".dimmed().to_string()
    } else {
        "-".to_owned()
    }
}

fn warning_marker(color_enabled: bool) -> String {
    if color_enabled {
        "!".yellow().to_string()
    } else {
        "!".to_owned()
    }
}

fn info_marker(color_enabled: bool) -> String {
    if color_enabled {
        "·".dimmed().to_string()
    } else {
        "·".to_owned()
    }
}
/// Human-only details that do not belong in the shared create payload.
struct CreateResultContext {
    config_path: PathBuf,
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
    progress: Option<TerminalProgress>,
    create_result: Option<CreateResultContext>,
}

impl Renderer<io::Stdout, io::Stderr> {
    pub fn stdio(options: RenderOptions) -> Self {
        let progress = options
            .live_progress
            .then(|| TerminalProgress::stderr(options.color_enabled));
        Self {
            stdout: io::stdout(),
            stderr: io::stderr(),
            options,
            exit_override: None,
            progress,
            create_result: None,
        }
    }
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
            progress: None,
            create_result: None,
        }
    }

    /// Returns a command-specific process status without turning a result into an error.
    #[must_use]
    pub const fn exit_override(&self) -> Option<u8> {
        self.exit_override
    }

    pub fn set_exit_override(&mut self, exit: u8) {
        self.exit_override = Some(exit);
    }

    pub(crate) fn set_create_result_context(&mut self, config_path: PathBuf) {
        self.create_result = Some(CreateResultContext { config_path });
    }

    /// Writes one line of interactive guidance to stderr.
    pub(crate) fn interactive_line(&mut self, message: &str) -> Result<(), FirestoneError> {
        self.write_feedback_line(format_args!("{message}"))
    }

    #[cfg(test)]
    #[must_use]
    pub fn into_writers(mut self) -> (Stdout, Stderr) {
        self.progress.take();
        (self.stdout, self.stderr)
    }

    /// Writes one interactive confirmation prompt to stderr.
    pub fn prompt(&mut self, message: &str) -> Result<(), FirestoneError> {
        if let Some(progress) = &mut self.progress {
            progress.clear_active();
        }
        write_safe_arguments(&mut self.stderr, format_args!("{message}"))
            .and_then(|()| self.stderr.flush())
            .map_err(write_output_failure)
    }

    fn write_feedback_line(&mut self, arguments: fmt::Arguments<'_>) -> Result<(), FirestoneError> {
        if let Some(progress) = &self.progress {
            return progress.println(terminal_text(&arguments.to_string()));
        }
        write_line(&mut self.stderr, arguments)
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
        if self.options.quiet && !matches!(event, Event::Result { .. }) {
            return Ok(());
        }
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
        self.render_human_error_inner(error, false)
    }

    pub fn render_hidden_error(&mut self, error: &FirestoneError) -> Result<(), FirestoneError> {
        self.render_human_error_inner(error, true)
    }

    fn render_human_error_inner(
        &mut self,
        error: &FirestoneError,
        include_kind: bool,
    ) -> Result<(), FirestoneError> {
        if let Some(progress) = &mut self.progress {
            progress.fail_last_active(error.message())?;
            progress.clear_active();
        }
        if include_kind {
            self.write_feedback_line(format_args!("error: {}: {}", error.kind(), error.message()))?;
        } else {
            self.write_feedback_line(format_args!("error: {}", error.message()))?;
        }

        let mut source = error.source();
        while let Some(cause) = source {
            self.write_feedback_line(format_args!("cause: {cause}"))?;
            source = cause.source();
        }

        if let Some(hint) = error.hint() {
            self.write_feedback_line(format_args!("hint:  {hint}"))?;
        }

        Ok(())
    }
    fn render_human_event(&mut self, event: Event) -> Result<(), FirestoneError> {
        match event {
            Event::StepStart { id, label } => {
                if self.options.quiet {
                    return Ok(());
                }
                if let Some(progress) = &mut self.progress {
                    progress.start(&id, &label)
                } else {
                    write_line(&mut self.stderr, format_args!("[{id}] {label}"))
                }
            }
            Event::StepUpdate { id, detail } => {
                if self.options.quiet {
                    return Ok(());
                }
                if let Some(progress) = &mut self.progress {
                    progress.update(&id, &detail)
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
                if let Some(progress) = &mut self.progress {
                    progress.skip(&id, &reason)
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
            Event::Output { data } => {
                if self.options.quiet {
                    return Ok(());
                }
                if let Some(progress) = &self.progress {
                    let stdout = &mut self.stdout;
                    progress
                        .multi
                        .suspend(|| write_safe_output(stdout, &data))
                        .map_err(write_output_failure)
                } else {
                    write_safe_output(&mut self.stdout, &data).map_err(write_output_failure)
                }
            }
            Event::Result { action, payload } => {
                if let Some(progress) = &mut self.progress {
                    progress.clear_active();
                }
                self.render_result(&action, payload)
            }
        }
    }

    fn render_progress(
        &mut self,
        id: &StepId,
        done: u64,
        total: Option<u64>,
        unit: Unit,
    ) -> Result<(), FirestoneError> {
        if let Some(progress) = &mut self.progress {
            return match unit {
                Unit::Bytes => progress.progress(id, done, total),
            };
        }

        match total {
            Some(total) => write_line(
                &mut self.stderr,
                format_args!(
                    "[{id}] {} / {}",
                    HumanCount::new(done, unit),
                    HumanCount::new(total, unit)
                ),
            ),
            None => write_line(
                &mut self.stderr,
                format_args!("[{id}] {}", HumanCount::new(done, unit)),
            ),
        }
    }

    fn render_step_done(
        &mut self,
        id: &StepId,
        detail: Option<&str>,
        elapsed_ms: u64,
    ) -> Result<(), FirestoneError> {
        if let Some(progress) = &mut self.progress {
            return progress.done(id, detail, elapsed_ms);
        }
        let elapsed = (elapsed_ms > 1_000).then_some(Elapsed(elapsed_ms));

        match (detail, elapsed) {
            (Some(detail), Some(elapsed)) => write_line(
                &mut self.stderr,
                format_args!("[{id}] {detail} · {elapsed}"),
            ),
            (Some(detail), None) => write_line(&mut self.stderr, format_args!("[{id}] {detail}")),
            (None, Some(elapsed)) => {
                write_line(&mut self.stderr, format_args!("[{id}] done · {elapsed}"))
            }
            (None, None) => write_line(&mut self.stderr, format_args!("[{id}] done")),
        }
    }

    fn render_step_failure(
        &mut self,
        id: &StepId,
        error: &ErrorInfo,
    ) -> Result<(), FirestoneError> {
        if let Some(progress) = &mut self.progress {
            return progress.fail(id, error);
        }
        write_line(
            &mut self.stderr,
            format_args!("[{id}] error: {}", error.message),
        )?;

        if let Some(hint) = &error.hint {
            write_line(&mut self.stderr, format_args!("hint:  {hint}"))?;
        }

        Ok(())
    }

    fn render_log(&mut self, level: Level, message: &str) -> Result<(), FirestoneError> {
        if let Some(progress) = &self.progress {
            let marker = match level {
                Level::Warn => warning_marker(self.options.color_enabled),
                Level::Error => failure_marker(self.options.color_enabled),
                Level::Trace | Level::Debug | Level::Info => {
                    info_marker(self.options.color_enabled)
                }
            };
            return progress.println(format!("  {marker} {}", terminal_text(message)));
        }
        match level {
            Level::Trace => write_line(&mut self.stderr, format_args!("[trace] {message}")),
            Level::Debug => write_line(&mut self.stderr, format_args!("[debug] {message}")),
            Level::Info => write_line(&mut self.stderr, format_args!("{message}")),
            Level::Warn => write_line(&mut self.stderr, format_args!("warning: {message}")),
            Level::Error => write_line(&mut self.stderr, format_args!("error: {message}")),
        }
    }
    fn render_result(
        &mut self,
        action: &str,
        payload: serde_json::Value,
    ) -> Result<(), FirestoneError> {
        match action {
            "create" => {
                let result: MachineRecord = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("create", error))?;
                let context = self.create_result.take().ok_or_else(|| {
                    invalid_result_value("create", "missing human result context")
                })?;
                self.render_create_result(&result, &context)
            }
            "edit" => Ok(()),
            "start" | "restart" => {
                let result: StartResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload(action, error))?;
                write_line(&mut self.stdout, format_args!("{} is running", result.name))
            }
            "stop" => {
                let result: StopResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("stop", error))?;
                write_line(
                    &mut self.stdout,
                    format_args!("{} is {}", result.name, machine_status_label(result.status)),
                )
            }
            "rm" => {
                let result: RemoveResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("rm", error))?;
                for name in result.removed {
                    write_line(&mut self.stdout, format_args!("removed {name}"))?;
                }
                Ok(())
            }
            "ssh-config" => {
                let result: SshConfigResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("ssh-config", error))?;
                write_safe_output(&mut self.stdout, &result.config).map_err(write_output_failure)
            }
            "logs" => {
                let _: LogsResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("logs", error))?;
                Ok(())
            }
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
            "show-vmconfig" => {
                serde_json::to_writer(&mut self.stdout, &payload).map_err(json_output_failure)?;
                finish_record(&mut self.stdout)
            }
            "catalog" => {
                let entries: Vec<CatalogEntrySummary> = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("catalog", error))?;
                write_catalog_table(&mut self.stdout, &entries).map_err(write_output_failure)
            }
            "images-pull" => self.render_image_pull_result(&payload),
            "images-rm" => self.render_image_remove_result(&payload),
            "images-prune" => self.render_image_prune_result(&payload),
            "version" => {
                let result: VersionResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("version", error))?;
                self.render_version_result(&result)
            }
            "doctor" => {
                let report: DoctorReport = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("doctor", error))?;
                write_doctor_report(&mut self.stdout, &report).map_err(write_output_failure)
            }
            "metrics" => {
                let result: MetricsResult = serde_json::from_value(payload)
                    .map_err(|error| invalid_result_payload("metrics", error))?;
                write_metrics_report(&mut self.stdout, &result).map_err(write_output_failure)
            }
            _ if payload.is_null() => Ok(()),
            _ => self.render_other_result(payload),
        }
    }

    fn render_create_result(
        &mut self,
        result: &MachineRecord,
        context: &CreateResultContext,
    ) -> Result<(), FirestoneError> {
        write_line(&mut self.stdout, format_args!("Created machine"))?;
        write_line(&mut self.stdout, format_args!("  Name: {}", result.name))?;
        write_line(
            &mut self.stdout,
            format_args!("  Image: {}", result.spec.image),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("  CPUs: {}", result.spec.cpus),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("  Memory: {}", result.spec.memory),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("  Disk: {}", result.spec.disk),
        )?;
        match (result.spec.network.mode, result.spec.network.tap.as_deref()) {
            (NetMode::Tap, Some(tap)) => {
                write_line(&mut self.stdout, format_args!("  Network: tap ({tap})"))?;
            }
            (NetMode::Tap, None) => {
                write_line(&mut self.stdout, format_args!("  Network: tap"))?;
            }
            (NetMode::Passt, _) => {
                write_line(&mut self.stdout, format_args!("  Network: passt"))?;
            }
            (NetMode::None, _) => {
                write_line(&mut self.stdout, format_args!("  Network: none"))?;
            }
        }

        if result.spec.network.forward.is_empty() {
            write_line(&mut self.stdout, format_args!("  Forwards: none"))?;
        } else {
            write_line(&mut self.stdout, format_args!("  Forwards:"))?;
            for forward in &result.spec.network.forward {
                write_line(&mut self.stdout, format_args!("    {forward}"))?;
            }
        }

        if result.spec.mounts.is_empty() {
            write_line(&mut self.stdout, format_args!("  Mounts: none"))?;
        } else {
            write_line(&mut self.stdout, format_args!("  Mounts:"))?;
            for (index, mount) in result.spec.mounts.iter().enumerate() {
                let access = if mount.readonly {
                    "read-only"
                } else {
                    "read-write"
                };
                match mount.tag.as_deref() {
                    Some(tag) => write_line(
                        &mut self.stdout,
                        format_args!(
                            "    {} -> {} ({access}, tag {tag})",
                            mount.host.display(),
                            mount.guest.display()
                        ),
                    )?,
                    None => write_line(
                        &mut self.stdout,
                        format_args!(
                            "    {} -> {} ({access}, tag share{index})",
                            mount.host.display(),
                            mount.guest.display()
                        ),
                    )?,
                }
            }
        }

        write_line(
            &mut self.stdout,
            format_args!("  Config: {}", context.config_path.display()),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("Edit: firestone edit {}", result.name),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("Start: firestone start {}", result.name),
        )
    }

    fn render_version_result(&mut self, result: &VersionResult) -> Result<(), FirestoneError> {
        write_line(
            &mut self.stdout,
            format_args!("firestone {}", result.version),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("release: {}", result.identity.release),
        )?;
        write_line(
            &mut self.stdout,
            format_args!(
                "git commit: {}",
                result
                    .identity
                    .git_commit
                    .as_deref()
                    .unwrap_or("not embedded")
            ),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("architecture: {}", result.architecture),
        )?;
        write_line(&mut self.stdout, format_args!("dependencies:"))?;
        for (name, dependency) in &result.dependencies {
            write_line(
                &mut self.stdout,
                format_args!(
                    "  {name}: {} (sha256 {})",
                    dependency.version, dependency.sha256
                ),
            )?;
        }
        write_line(&mut self.stdout, format_args!("paths:"))?;
        write_line(
            &mut self.stdout,
            format_args!("  config: {}", result.paths.config),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("  data: {}", result.paths.data),
        )?;
        write_line(
            &mut self.stdout,
            format_args!("  runtime: {}", result.paths.runtime),
        )
    }

    fn render_image_pull_result(
        &mut self,
        payload: &serde_json::Value,
    ) -> Result<(), FirestoneError> {
        let metadata = payload
            .get("metadata")
            .and_then(serde_json::Value::as_object);
        let source = metadata
            .and_then(|metadata| metadata.get("source_ref"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_result_value("images-pull", "missing metadata.source_ref"))?;
        let architecture = metadata
            .and_then(|metadata| metadata.get("architecture"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_result_value("images-pull", "missing metadata.architecture"))?;
        let size = metadata
            .and_then(|metadata| metadata.get("size"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid_result_value("images-pull", "missing metadata.size"))?;
        let cached = payload
            .get("cached")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| invalid_result_value("images-pull", "missing cached"))?;
        let disposition = if cached { "cached" } else { "pulled" };
        write_line(
            &mut self.stdout,
            format_args!(
                "{source} · {architecture} · {} · {disposition}",
                HumanBytes(size)
            ),
        )
    }

    fn render_image_remove_result(
        &mut self,
        payload: &serde_json::Value,
    ) -> Result<(), FirestoneError> {
        let id = payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| invalid_result_value("images-rm", "missing id"))?;
        let bytes = payload
            .get("bytes_freed")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid_result_value("images-rm", "missing bytes_freed"))?;
        write_line(
            &mut self.stdout,
            format_args!("removed {id} · {} freed", HumanBytes(bytes)),
        )
    }

    fn render_image_prune_result(
        &mut self,
        payload: &serde_json::Value,
    ) -> Result<(), FirestoneError> {
        let removed = payload
            .get("removed")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| invalid_result_value("images-prune", "missing removed"))?;
        let bytes = payload
            .get("bytes_freed")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| invalid_result_value("images-prune", "missing bytes_freed"))?;
        write_line(
            &mut self.stdout,
            format_args!(
                "pruned {} image(s) · {} freed",
                removed.len(),
                HumanBytes(bytes)
            ),
        )
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
        write_safe_text(writer, doctor_status_label(check.status))?;
        writer.write_all(b" ")?;
        write_safe_text(writer, doctor_check_id_label(check.id))?;
        writer.write_all(b": ")?;
        write_safe_text(writer, &check.reason)?;
        writer.write_all(b"\n")?;

        if let Some(fix) = &check.fix {
            writer.write_all(b"  fix: ")?;
            write_safe_text(writer, fix)?;
            writer.write_all(b"\n")?;
        }
        if let Some(hint) = &check.hint {
            writer.write_all(b"  hint: ")?;
            write_safe_text(writer, hint)?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()
}

const fn doctor_status_label(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Ok => "ok",
        DoctorStatus::Warn => "warn",
        DoctorStatus::Fail => "fail",
    }
}

const fn machine_status_label(status: MachineStatus) -> &'static str {
    match status {
        MachineStatus::Created => "created",
        MachineStatus::Starting => "starting",
        MachineStatus::Running => "running",
        MachineStatus::Stopping => "stopping",
        MachineStatus::Stopped => "stopped",
        MachineStatus::Failed => "failed",
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

/// Renders one metrics sample as a deterministic human table.
///
/// Counters are cumulative, so the table states raw totals and never a rate.
/// An absent figure prints `-` rather than a fabricated zero.
fn write_metrics_report<W: Write>(writer: &mut W, result: &MetricsResult) -> io::Result<()> {
    write!(writer, "sampled at ")?;
    write_safe_text(writer, &result.sampled_at)?;
    writer.write_all(b"\n")?;
    writeln!(
        writer,
        "cpu       {} vcpus, {} ns cpu time",
        result.cpu.vcpus,
        optional_count(result.cpu.cpu_time_ns)
    )?;
    writeln!(
        writer,
        "memory    {} bytes rss, {} bytes allocated, {} bytes guest actual",
        optional_count(result.memory.rss_bytes),
        result.memory.allocated_bytes,
        optional_count(result.memory.guest_actual_bytes)
    )?;

    if result.block.is_empty() {
        writeln!(writer, "block     none")?;
    } else {
        let device_width = result
            .block
            .iter()
            .map(|device| display_width(&device.device))
            .chain(std::iter::once("DEVICE".len()))
            .max()
            .unwrap_or("DEVICE".len());
        writeln!(
            writer,
            "{:<device_width$}  {:>14}  {:>14}  {:>10}  {:>10}",
            "DEVICE",
            "READ BYTES",
            "WRITTEN BYTES",
            "READ OPS",
            "WRITE OPS",
            device_width = device_width,
        )?;
        for device in &result.block {
            write_safe_cell(writer, &device.device, device_width)?;
            writeln!(
                writer,
                "  {:>14}  {:>14}  {:>10}  {:>10}",
                optional_count(device.read_bytes),
                optional_count(device.written_bytes),
                optional_count(device.read_ops),
                optional_count(device.write_ops),
            )?;
        }
    }

    match result.net.as_deref() {
        None | Some([]) => writeln!(writer, "net       none reported"),
        Some(devices) => {
            for device in devices {
                write!(writer, "net       ")?;
                write_safe_text(writer, &device.device)?;
                for (key, value) in &device.counters {
                    writer.write_all(b" ")?;
                    write_safe_text(writer, key)?;
                    write!(writer, "={value}")?;
                }
                writer.write_all(b"\n")?;
            }
            Ok(())
        }
    }
}

fn optional_count(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn write_catalog_table<W: Write>(
    writer: &mut W,
    entries: &[CatalogEntrySummary],
) -> io::Result<()> {
    let rows = entries
        .iter()
        .map(|entry| {
            let aliases = if entry.aliases.is_empty() {
                String::new()
            } else {
                entry.aliases.join(", ")
            };
            let architectures = entry
                .architectures
                .iter()
                .map(|architecture| architecture.architecture.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let firmware = entry.architectures.first().map_or_else(
                || "-".to_owned(),
                |first| {
                    if entry
                        .architectures
                        .iter()
                        .all(|architecture| architecture.firmware == first.firmware)
                    {
                        catalog_firmware_label(first.firmware).to_owned()
                    } else {
                        entry
                            .architectures
                            .iter()
                            .map(|architecture| {
                                format!(
                                    "{}={}",
                                    architecture.architecture,
                                    catalog_firmware_label(architecture.firmware)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                },
            );
            (entry.reference.as_str(), aliases, firmware, architectures)
        })
        .collect::<Vec<_>>();

    let reference_width = rows
        .iter()
        .map(|row| display_width(row.0))
        .fold("REFERENCE".len(), usize::max);
    let aliases_width = rows
        .iter()
        .map(|row| display_width(&row.1))
        .fold("ALIASES".len(), usize::max);
    let firmware_width = rows
        .iter()
        .map(|row| display_width(&row.2))
        .fold("FIRMWARE".len(), usize::max);

    writeln!(
        writer,
        "{:<reference_width$}  {:<aliases_width$}  {:<firmware_width$}  ARCHITECTURES",
        "REFERENCE", "ALIASES", "FIRMWARE"
    )?;
    for (reference, aliases, firmware, architectures) in rows {
        write_safe_cell(writer, reference, reference_width)?;
        writer.write_all(b"  ")?;
        write_safe_cell(writer, &aliases, aliases_width)?;
        writer.write_all(b"  ")?;
        write_safe_cell(writer, &firmware, firmware_width)?;
        writer.write_all(b"  ")?;
        write_safe_text(writer, &architectures)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

const fn catalog_firmware_label(firmware: CatalogFirmware) -> &'static str {
    match firmware {
        CatalogFirmware::Rhf => "rhf",
        CatalogFirmware::Edk2 => "edk2",
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
pub(crate) fn write_safe_output<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    let mut encoded = [0_u8; 4];
    for character in value.chars() {
        if character == '\n' {
            writer.write_all(b"\n")?;
            continue;
        }
        let character = if character.is_control() {
            '\u{fffd}'
        } else {
            character
        };
        writer.write_all(character.encode_utf8(&mut encoded).as_bytes())?;
    }
    writer.flush()
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

struct SafeTextFormatter<'writer, W> {
    writer: &'writer mut W,
    error: Option<io::Error>,
}

impl<W: Write> fmt::Write for SafeTextFormatter<'_, W> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        write_safe_text(self.writer, value).map_err(|error| {
            self.error = Some(error);
            fmt::Error
        })
    }
}

fn write_safe_arguments<W: Write>(writer: &mut W, arguments: fmt::Arguments<'_>) -> io::Result<()> {
    let mut formatter = SafeTextFormatter {
        writer,
        error: None,
    };
    if fmt::write(&mut formatter, arguments).is_ok() {
        return Ok(());
    }
    match formatter.error {
        Some(error) => Err(error),
        None => Err(io::Error::other("failed to format command output")),
    }
}

fn write_line<W: Write>(
    writer: &mut W,
    arguments: fmt::Arguments<'_>,
) -> Result<(), FirestoneError> {
    write_safe_arguments(writer, arguments)
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
fn invalid_result_value(action: &str, detail: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("invalid {action} result payload: {detail}"),
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, error::Error, io, path::PathBuf};

    use firestone_core::{
        DoctorCheck, DoctorCheckId, DoctorReport, DoctorStatus, EventSink, ImageRef, MachineRecord,
        MachineSpec, MachineState, MachineStatus, MountSpec, StateImage, StateVersion, StepId,
    };
    use indicatif::{InMemoryTerm, ProgressDrawTarget};
    use serde_json::json;

    use super::{
        ErrorInfo, ErrorKind, Event, FirestoneError, Level, MachineSummary, MachineView,
        RenderOptions, Renderer, TerminalProgress, Unit, error_exit_code, exit_code,
    };

    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn terminal_progress_settles_ordered_unicode_rows_without_duplicates() -> TestResult {
        let terminal = InMemoryTerm::new(20, 80);
        let target = ProgressDrawTarget::term_like(Box::new(terminal.clone()));
        let mut progress = TerminalProgress::with_draw_target(target, false);

        let image = StepId::from("image");
        progress.start(&image, "download image")?;
        progress.update(&image, "verifying")?;
        progress.progress(&image, 512, Some(1_024))?;
        progress.done(&image, Some("cached"), 1_251)?;

        let disk = StepId::from("磁盘");
        progress.done(&disk, Some("20G overlay"), 348)?;

        let filesystem = StepId::from("fs");
        progress.start(&filesystem, "first mount")?;
        progress.done(&filesystem, Some("one"), 0)?;
        progress.skip(&filesystem, "cached")?;

        progress.fail(
            &StepId::from("ssh"),
            &ErrorInfo {
                kind: ErrorKind::Timeout,
                message: "timed out".to_owned(),
                hint: None,
            },
        )?;
        drop(progress);

        let contents = terminal.contents();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("  ✓ image    cached"));
        assert!(lines[0].ends_with("· 1.3s"));
        assert!(lines[1].starts_with("  ✓ 磁盘     20G overlay"));
        assert!(lines[1].ends_with("· 0.3s"));
        assert!(lines[2].starts_with("  ✓ fs       one"));
        assert!(lines[2].ends_with("· 0.0s"));
        assert_eq!(lines[3], "  - fs       cached");
        assert_eq!(lines[4], "  ✗ ssh      timed out");
        Ok(())
    }

    #[test]
    fn lifecycle_and_log_results_render_exact_human_output() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        renderer.emit(Event::Output {
            data: "first\nbad\u{1b}[31m\n".to_owned(),
        })?;
        renderer.emit(Event::Result {
            action: "start".to_owned(),
            payload: json!({
                "name": "dev",
                "status": "running",
                "elapsed_ms": 12,
                "forwards": [],
                "mounts": []
            }),
        })?;
        renderer.emit(Event::Result {
            action: "stop".to_owned(),
            payload: json!({"name": "dev", "status": "stopped", "elapsed_ms": 20}),
        })?;
        renderer.emit(Event::Result {
            action: "rm".to_owned(),
            payload: json!({"removed": ["dev"]}),
        })?;
        renderer.emit(Event::Result {
            action: "logs".to_owned(),
            payload: json!({"name": "dev", "source": "console", "lines": 2, "follow": false}),
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            String::from_utf8(stdout)?,
            "first\nbad�[31m\ndev is running\ndev is stopped\nremoved dev\n"
        );
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn output_event_json_is_exact_and_quiet_suppresses_data_only() -> TestResult {
        let mut json_renderer = Renderer::new(Vec::new(), Vec::new(), RenderOptions::json());
        json_renderer.emit(Event::Output {
            data: "line\n".to_owned(),
        })?;
        let (stdout, stderr) = json_renderer.into_writers();
        assert_eq!(stdout, b"{\"type\":\"Output\",\"data\":\"line\\n\"}\n");
        assert!(stderr.is_empty());

        let mut quiet = Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(true, false));
        quiet.emit(Event::Output {
            data: "hidden\n".to_owned(),
        })?;
        quiet.emit(Event::Result {
            action: "start".to_owned(),
            payload: json!({
                "name": "dev",
                "status": "running",
                "elapsed_ms": 0,
                "forwards": [],
                "mounts": []
            }),
        })?;
        let (stdout, stderr) = quiet.into_writers();
        assert_eq!(stdout, b"dev is running\n");
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn vmconfig_result_writes_canonical_object_bytes() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        renderer.emit(Event::Result {
            action: "show-vmconfig".to_owned(),
            payload: json!({"a": 1, "nested": {"b": 2}}),
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            stdout,
            br#"{"a":1,"nested":{"b":2}}
"#
        );
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn image_mutation_results_report_bytes_freed() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        renderer.emit(Event::Result {
            action: "images-pull".to_owned(),
            payload: json!({
                "metadata": {
                    "source_ref": "ubuntu:24.04",
                    "architecture": "x86_64",
                    "size": 1000
                },
                "cached": true
            }),
        })?;
        renderer.emit(Event::Result {
            action: "images-rm".to_owned(),
            payload: json!({"id": "image-id", "bytes_freed": 10, "referenced_by": []}),
        })?;
        renderer.emit(Event::Result {
            action: "images-prune".to_owned(),
            payload: json!({"removed": ["one", "two"], "bytes_freed": 2000}),
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            String::from_utf8(stdout)?,
            "ubuntu:24.04 · x86_64 · 1 kB · cached\nremoved image-id · 10 B freed\npruned 2 image(s) · 2 kB freed\n"
        );
        assert!(stderr.is_empty());
        Ok(())
    }

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
    fn json_quiet_with_non_result_events_emits_only_result_and_terminal_error() -> TestResult {
        let options = RenderOptions::json().with_quiet(true);
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), options);
        let events = vec![
            Event::StepStart {
                id: StepId::from("start"),
                label: "starting".to_owned(),
            },
            Event::StepUpdate {
                id: StepId::from("update"),
                detail: "updating".to_owned(),
            },
            Event::Progress {
                id: StepId::from("progress"),
                done: 1,
                total: Some(2),
                unit: Unit::Bytes,
            },
            Event::StepDone {
                id: StepId::from("done"),
                detail: None,
                elapsed_ms: 1,
            },
            Event::StepSkip {
                id: StepId::from("skip"),
                reason: "not needed".to_owned(),
            },
            Event::StepFail {
                id: StepId::from("fail"),
                error: ErrorInfo {
                    kind: ErrorKind::Generic,
                    message: "failed".to_owned(),
                    hint: Some("retry".to_owned()),
                },
            },
            Event::Log {
                level: Level::Error,
                message: "event error".to_owned(),
            },
        ];
        for event in events {
            renderer.emit(event)?;
        }
        renderer.emit(Event::Result {
            action: "version".to_owned(),
            payload: json!({"version": "0.1.0"}),
        })?;
        renderer.render_error(&FirestoneError::new(
            ErrorKind::Dependency,
            "terminal error",
        ))?;

        let (stdout, stderr) = renderer.into_writers();
        let records = stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice)
            .collect::<Result<Vec<serde_json::Value>, _>>()?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["type"], "Result");
        assert_eq!(records[0]["action"], "version");
        assert_eq!(records[1]["error"]["message"], "terminal error");
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
        let options = RenderOptions::human(false, false).with_verbosity(2);
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), options);
        renderer.emit(Event::Log {
            level: Level::Trace,
            message: "trace detail".to_owned(),
        })?;
        renderer.emit(Event::Log {
            level: Level::Debug,
            message: "debug detail".to_owned(),
        })?;
        let (stdout, stderr) = renderer.into_writers();
        assert!(stdout.is_empty());
        assert_eq!(stderr, b"[trace] trace detail\n[debug] debug detail\n");
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
    fn human_doctor_report_with_control_bearing_text_emits_sanitized_lines() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                id: DoctorCheckId::Kvm,
                status: DoctorStatus::Fail,
                reason: "cannot open /dev/kvm\u{1b}[31m\n拒绝".to_owned(),
                fix: Some("run \u{7}doctor --fix".to_owned()),
                hint: Some("réessayer\u{1b}[0m".to_owned()),
            }],
        };

        renderer.emit(Event::Result {
            action: "doctor".to_owned(),
            payload: serde_json::to_value(report)?,
        })?;

        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(
            String::from_utf8(stdout)?,
            "fail kvm: cannot open /dev/kvm�[31m�拒绝\n  fix: run �doctor --fix\n  hint: réessayer�[0m\n"
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
            action: "probe".to_owned(),
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
    fn human_error_with_control_bearing_path_and_hint_emits_sanitized_lines() -> TestResult {
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        let error =
            FirestoneError::new(ErrorKind::InvalidSpec, "cannot read /tmp/雪\u{1b}[31m\nvm")
                .with_source(io::Error::other("permission\u{7} denied"))
                .with_hint("inspect /tmp/雪\u{1b}[0m");

        renderer.render_error(&error)?;

        let (stdout, stderr) = renderer.into_writers();
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr)?,
            "error: cannot read /tmp/雪�[31m�vm\ncause: permission� denied\nhint:  inspect /tmp/雪�[0m\n"
        );
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
            action: "probe".to_owned(),
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
    fn create_result_renders_effective_summary_and_commands() -> TestResult {
        let mut spec = MachineSpec {
            image: ImageRef::new("ubuntu:24.04"),
            ..MachineSpec::default()
        };
        spec.network.forward.push("8080:80".parse()?);
        spec.mounts.push(MountSpec {
            host: PathBuf::from("/srv/project"),
            guest: PathBuf::from("/wo\u{1b}rk"),
            readonly: true,
            tag: None,
        });
        let record = MachineRecord {
            name: "dev".to_owned(),
            spec,
            state: MachineState {
                version: StateVersion,
                status: MachineStatus::Created,
                image: StateImage {
                    r#ref: "ubuntu".to_owned(),
                    id: None,
                    sha256: None,
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
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        renderer.set_create_result_context(PathBuf::from(
            "/home/alice/.local/share/firestone/machines/dev/firestone.toml",
        ));

        renderer.emit(Event::Result {
            action: "create".to_owned(),
            payload: serde_json::to_value(record)?,
        })?;

        let (stdout, stderr) = renderer.into_writers();
        let output = String::from_utf8(stdout)?;
        assert_eq!(
            output,
            "Created machine\n  Name: dev\n  Image: ubuntu:24.04\n  CPUs: 2\n  Memory: 2G\n  Disk: 20G\n  Network: passt\n  Forwards:\n    8080:80\n  Mounts:\n    /srv/project -> /wo�rk (read-only, tag share0)\n  Config: /home/alice/.local/share/firestone/machines/dev/firestone.toml\nEdit: firestone edit dev\nStart: firestone start dev\n"
        );
        assert!(!output.contains('\u{1b}'));
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
            supervision: None,
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
        assert!(String::from_utf8_lossy(&stdout).contains("\"supervision\": null"));
        Ok(())
    }

    #[test]
    fn every_error_kind_renders_stable_context_and_hint() -> TestResult {
        let kinds = [
            ErrorKind::Generic,
            ErrorKind::Usage,
            ErrorKind::InvalidSpec,
            ErrorKind::NotFound,
            ErrorKind::NotRunning,
            ErrorKind::Conflict,
            ErrorKind::AlreadyExists,
            ErrorKind::AlreadyRunning,
            ErrorKind::Busy,
            ErrorKind::Dependency,
            ErrorKind::Timeout,
            ErrorKind::Checksum,
            ErrorKind::Interrupted,
        ];

        for kind in kinds {
            let message = format!("{kind} operation failed for machine demo");
            let hint = format!("correct the {kind} condition and retry");
            let error = FirestoneError::new(kind, &message).with_hint(&hint);

            let mut human =
                Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
            human.render_error(&error)?;
            let (stdout, stderr) = human.into_writers();
            assert!(stdout.is_empty());
            assert_eq!(
                String::from_utf8(stderr)?,
                format!("error: {message}\nhint:  {hint}\n")
            );

            let mut json = Renderer::new(Vec::new(), Vec::new(), RenderOptions::json());
            json.render_error(&error)?;
            let (stdout, stderr) = json.into_writers();
            assert!(stderr.is_empty());
            let record: serde_json::Value = serde_json::from_slice(&stdout)?;
            assert_eq!(record["error"]["kind"], kind.as_str());
            assert_eq!(record["error"]["message"], message);
            assert_eq!(record["error"]["hint"], hint);
        }
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

    #[test]
    fn metrics_result_human_table_prints_totals_and_dashes_for_absent_figures() -> TestResult {
        let result = firestone_core::MetricsResult {
            sampled_at: "2026-09-02T12:00:00Z".to_owned(),
            cpu: firestone_core::MetricsCpu {
                vcpus: 2,
                cpu_time_ns: Some(9_500_000_000),
            },
            memory: firestone_core::MetricsMemory {
                rss_bytes: None,
                allocated_bytes: 2_147_483_648,
                guest_actual_bytes: Some(1_073_741_824),
            },
            block: vec![firestone_core::MetricsBlockDevice {
                device: "_disk0".to_owned(),
                read_bytes: Some(4096),
                written_bytes: Some(8192),
                read_ops: Some(2),
                write_ops: None,
            }],
            net: None,
        };
        let mut renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        renderer.emit(Event::Result {
            action: "metrics".to_owned(),
            payload: serde_json::to_value(&result)?,
        })?;

        let (stdout, stderr) = renderer.into_writers();
        let text = String::from_utf8(stdout)?;
        assert_eq!(
            text,
            concat!(
                "sampled at 2026-09-02T12:00:00Z\n",
                "cpu       2 vcpus, 9500000000 ns cpu time\n",
                "memory    - bytes rss, 2147483648 bytes allocated, 1073741824 bytes guest actual\n",
                "DEVICE      READ BYTES   WRITTEN BYTES    READ OPS   WRITE OPS\n",
                "_disk0            4096            8192           2           -\n",
                "net       none reported\n",
            )
        );
        assert!(stderr.is_empty());
        Ok(())
    }
}
