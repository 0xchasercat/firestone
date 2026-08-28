use std::{collections::BTreeMap, env, ffi::OsString, fs, os::unix::fs::DirBuilderExt, path::Path};

use firestone_core::{
    Action, Arch, Catalog, Cmd, DependencyManifest, DispatchFuture, Dispatcher, DoctorContext,
    ErrorKind, Event, EventSink, FirestoneError, GlobalConfig, Level, MachineLock, MachineRecord,
    MachineSpec, MachineSpecPatch, MachineState, MachineStatus, MachineSummary, MachineView, Paths,
    RealValidationHost, SpecResult, SpecWarningPayload, StateImage, StateStore, StateVersion,
    ValidationContext, atomic, read_reconciled_machine_state_live, run_doctor,
};

const SPEC_TEMPLATE: &str = include_str!("../../../templates/firestone.toml");

pub struct LocalDispatcher {
    paths: Paths,
    global: GlobalConfig,
    catalog: Catalog,
}

impl LocalDispatcher {
    pub fn new(paths: Paths, global: GlobalConfig, catalog: Catalog) -> Self {
        Self {
            paths,
            global,
            catalog,
        }
    }

    fn validate_machine_storage(&self) -> Result<(), FirestoneError> {
        validate_owned_directory(self.paths.data_dir(), "data directory", true)?;
        validate_owned_directory(&self.paths.machines_dir(), "machines directory", true)
    }

    pub fn edit(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        let spec_path = self.paths.machine_spec(name)?;
        ensure_machine_exists(name, &machine_dir)?;
        let observed_state = self.read_live_state(name)?;
        let lock_path = self.paths.machine_lock(name)?;
        let _lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(name, &machine_dir)?;
        let original = read_file(&spec_path, "machine spec", ErrorKind::NotFound)?;
        let candidate = spec_path.with_extension("toml.edit");
        atomic::write(&candidate, &original)?;

        let result = self.edit_candidate(name, &machine_dir, &candidate, &spec_path, events);
        let cleanup = remove_candidate(&candidate);
        match (result, cleanup) {
            (Ok(result), Ok(())) => {
                emit_running_spec_warning(&observed_state, events)?;
                emit_spec_warnings(&result.warnings, events)?;
                emit_result(events, "edit", &result)
            }
            (Ok(_), Err(error)) => Err(error),
            (Err(error), _) => Err(error),
        }
    }

    fn edit_candidate(
        &self,
        name: &str,
        machine_dir: &Path,
        candidate: &Path,
        spec_path: &Path,
        events: &mut dyn EventSink,
    ) -> Result<SpecResult, FirestoneError> {
        let editor = env::var_os("VISUAL")
            .filter(|value| !value.is_empty())
            .or_else(|| env::var_os("EDITOR").filter(|value| !value.is_empty()))
            .ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "cannot edit the machine spec because VISUAL and EDITOR are unset",
                )
                .with_hint("set VISUAL or EDITOR to an editor executable and retry")
            })?;
        let (editor_program, editor_args) = parse_editor_command(editor)?;
        loop {
            Cmd::new(&editor_program)
                .args(editor_args.iter().map(String::as_str))
                .arg(candidate.as_os_str())
                .stdin_inherit()
                .interactive_stdout_to_stderr()
                .error_kind(ErrorKind::Dependency)
                .run_interactive()?;
            let candidate_bytes = read_file(candidate, "edited machine spec", ErrorKind::Generic)?;
            let candidate_text = std::str::from_utf8(&candidate_bytes).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::InvalidSpec,
                    format!("edited machine spec {} is not UTF-8", candidate.display()),
                )
                .with_hint("save the machine spec as UTF-8 TOML")
                .with_source(source)
            })?;
            match self.load_spec_text(candidate_text, machine_dir, machine_dir) {
                Ok(loaded) => {
                    atomic::write(spec_path, &candidate_bytes)?;
                    return Ok(SpecResult {
                        spec: loaded.spec,
                        warnings: loaded
                            .warnings
                            .iter()
                            .map(SpecWarningPayload::from)
                            .collect(),
                    });
                }
                Err(error) => {
                    let hint = error
                        .hint()
                        .map_or_else(String::new, |hint| format!("; hint: {hint}"));
                    events.emit(Event::Log {
                        level: Level::Warn,
                        message: format!(
                            "machine `{name}` was not saved: {}{hint}; reopening editor",
                            error.message()
                        ),
                    })?;
                }
            }
        }
    }

    fn create(
        &self,
        name: &str,
        spec: MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.create_internal(name, spec, false, events)
    }

    pub fn create_with_edit(
        &self,
        name: &str,
        spec: MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.create_internal(name, spec, true, events)
    }

    fn create_internal(
        &self,
        name: &str,
        spec: MachineSpec,
        edit: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let machine_dir = self.paths.machine_dir(name)?;
        let machines_dir = self.paths.machines_dir();
        ensure_owned_directory(self.paths.data_dir(), "data directory", true)?;
        ensure_owned_directory(&machines_dir, "machines directory", false)?;

        match fs::create_dir(&machine_dir) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(FirestoneError::new(
                    ErrorKind::AlreadyExists,
                    format!("machine `{name}` already exists"),
                )
                .with_hint(format!(
                    "use `firestone show {name}` or choose another name"
                )));
            }
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot create machine directory {}", machine_dir.display()),
                    "check the Firestone data directory permissions",
                    source,
                ));
            }
        }

        let creating_marker = machine_dir.join(".creating");
        if let Err(source) = fs::write(&creating_marker, b"creating\n") {
            let _ = fs::remove_dir_all(&machine_dir);
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot mark machine `{name}` as being created"),
                "check the machine directory permissions",
                source,
            ));
        }
        let lock_path = self.paths.machine_lock(name)?;
        let lock = match MachineLock::acquire(name, &lock_path, events) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = fs::remove_dir_all(&machine_dir);
                return Err(error);
            }
        };
        let mut record = match self.initialize_machine(name, spec, &lock) {
            Ok(record) => record,
            Err(error) => {
                let _ = fs::remove_dir_all(&machine_dir);
                return Err(error);
            }
        };

        if edit {
            let spec_path = self.paths.machine_spec(name)?;
            let candidate = spec_path.with_extension("toml.edit");
            let original = read_file(&spec_path, "machine spec", ErrorKind::Generic)?;
            if let Err(error) = atomic::write(&candidate, &original) {
                let _ = fs::remove_dir_all(&machine_dir);
                return Err(error);
            }
            let edited = self.edit_candidate(name, &machine_dir, &candidate, &spec_path, events);
            let cleanup = remove_candidate(&candidate);
            let edited = match (edited, cleanup) {
                (Ok(edited), Ok(())) => edited,
                (Ok(_), Err(error)) | (Err(error), _) => {
                    let _ = fs::remove_dir_all(&machine_dir);
                    return Err(error);
                }
            };
            if let Err(error) = emit_spec_warnings(&edited.warnings, events) {
                let _ = fs::remove_dir_all(&machine_dir);
                return Err(error);
            }
            let state = self.created_state(name, &edited.spec)?;
            if let Err(error) = StateStore::new(self.paths.machine_state(name)?)
                .write_from_locked_action(&state, &lock)
            {
                let _ = fs::remove_dir_all(&machine_dir);
                return Err(error);
            }
            record.spec = edited.spec;
            record.state = state;
        }

        if let Err(source) = fs::remove_file(&creating_marker) {
            let _ = fs::remove_dir_all(&machine_dir);
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot publish machine `{name}`"),
                "check the machine directory permissions",
                source,
            ));
        }
        emit_result(events, "create", &record)
    }

    fn initialize_machine(
        &self,
        name: &str,
        spec: MachineSpec,
        lock: &MachineLock,
    ) -> Result<MachineRecord, FirestoneError> {
        let state = self.created_state(name, &spec)?;
        let spec_document = render_spec(&spec)?;
        atomic::write(&self.paths.machine_spec(name)?, spec_document.as_bytes())?;
        StateStore::new(self.paths.machine_state(name)?).write_from_locked_action(&state, lock)?;
        Ok(MachineRecord {
            name: name.to_owned(),
            spec,
            state,
        })
    }

    fn created_state(
        &self,
        name: &str,
        spec: &MachineSpec,
    ) -> Result<MachineState, FirestoneError> {
        let architecture = match spec.arch {
            Some(architecture) => architecture,
            None => Arch::current().map_err(|message| {
                FirestoneError::new(ErrorKind::Dependency, message)
                    .with_hint("run Firestone on an x86_64 or aarch64 host")
            })?,
        };
        let image_reference = if self.catalog.contains_reference(spec.image.as_str()) {
            self.catalog
                .resolve(spec.image.as_str(), architecture.as_str())?
                .canonical_reference
        } else {
            spec.image.as_str().to_owned()
        };
        Ok(MachineState {
            version: StateVersion,
            status: MachineStatus::Created,
            image: StateImage {
                r#ref: image_reference,
                id: None,
                sha256: None,
            },
            mac: spec.network.mac.map(|mac| mac.to_string()),
            cid: 3,
            instance_id: None,
            shim_pid: None,
            vmm_pid: None,
            sidecar_pids: BTreeMap::new(),
            runtime_dir: self.paths.machine_runtime_dir(name)?,
            started_at: None,
            forwards: spec
                .network
                .forward
                .iter()
                .map(ToString::to_string)
                .collect(),
            degraded: Vec::new(),
            last_exit: None,
        })
    }

    fn list(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let names = self.machine_names()?;
        let mut machines = Vec::with_capacity(names.len());
        for name in names {
            let (spec, state) = self.load_machine(&name)?;
            machines.push(MachineSummary {
                name,
                status: display_status(&state),
                image: state.image.r#ref.clone(),
                cpus: spec.cpus,
                memory: spec.memory.to_string(),
                uptime: None,
                forwards: state
                    .forwards
                    .iter()
                    .map(|forward| display_forward(forward))
                    .collect(),
            });
        }
        emit_result(events, "list", &machines)
    }

    fn show(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let (spec, state) = self.load_machine(name)?;
        emit_result(events, "show", &MachineView { spec, state })
    }

    fn set_spec(
        &self,
        name: &str,
        spec: MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(name, &machine_dir)?;
        let observed_state = self.read_live_state(name)?;
        let lock_path = self.paths.machine_lock(name)?;
        let _lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(name, &machine_dir)?;
        let document = render_spec(&spec)?;
        atomic::write(&self.paths.machine_spec(name)?, document.as_bytes())?;
        emit_running_spec_warning(&observed_state, events)?;
        emit_result(
            events,
            "edit",
            &SpecResult {
                spec,
                warnings: Vec::new(),
            },
        )
    }

    fn read_live_state(&self, name: &str) -> Result<MachineState, FirestoneError> {
        let state_path = self.paths.machine_state(name)?;
        ensure_owned_regular_file(&state_path, "machine state", ErrorKind::Generic)?;
        let reconciled_at = jiff::Timestamp::now().to_string();
        read_reconciled_machine_state_live(&self.paths, name, &reconciled_at)
    }

    fn load_machine(&self, name: &str) -> Result<(MachineSpec, MachineState), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(name, &machine_dir)?;
        let source = read_file(
            &self.paths.machine_spec(name)?,
            "machine spec",
            ErrorKind::NotFound,
        )?;
        let text = std::str::from_utf8(&source).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine spec for `{name}` is not UTF-8"),
            )
            .with_hint("save firestone.toml as UTF-8 TOML")
            .with_source(source)
        })?;
        let loaded = self.load_spec_text(text, &machine_dir, &machine_dir)?;
        let state = self.read_live_state(name)?;
        Ok((loaded.spec, state))
    }

    fn load_spec_text(
        &self,
        text: &str,
        machine_dir: &Path,
        patch_base_dir: &Path,
    ) -> Result<firestone_core::LoadedMachineSpec, FirestoneError> {
        let host = RealValidationHost::new();
        MachineSpec::load(
            text,
            &self.global,
            &MachineSpecPatch::default(),
            patch_base_dir,
            &ValidationContext {
                host: &host,
                paths: &self.paths,
                machine_dir,
                catalog: &self.catalog,
                base_image_virtual_size: None,
            },
        )
    }

    fn doctor(&self, fix: bool, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let hostname = env::var("HOSTNAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_owned());
        let context = DoctorContext::from_paths(
            self.paths.clone(),
            DependencyManifest::bundled()?,
            hostname,
            jiff::Timestamp::now().to_string(),
        );
        let report = run_doctor(&context, fix)?;
        emit_result(events, "doctor", &report)
    }

    fn machine_names(&self) -> Result<Vec<String>, FirestoneError> {
        self.validate_machine_storage()?;
        let entries = match fs::read_dir(self.paths.machines_dir()) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Generic,
                    format!(
                        "cannot read machines directory {}",
                        self.paths.machines_dir().display()
                    ),
                    "check the Firestone data directory permissions",
                    source,
                ));
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| {
                filesystem_error(
                    ErrorKind::Generic,
                    "cannot read a machine directory entry",
                    "check the Firestone data directory permissions",
                    source,
                )
            })?;
            let file_type = entry.file_type().map_err(|source| {
                filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot inspect machine path {}", entry.path().display()),
                    "check the machine directory permissions",
                    source,
                )
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if self.paths.machine_dir(&name).is_err() || !self.machine_files_ready(&name)? {
                continue;
            }
            names.push(name);
        }
        names.sort();
        Ok(names)
    }

    fn machine_files_ready(&self, name: &str) -> Result<bool, FirestoneError> {
        let marker = self.paths.machine_dir(name)?.join(".creating");
        match fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(false),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot inspect creation marker {}", marker.display()),
                    "check the machine directory permissions",
                    source,
                ));
            }
        }
        Ok(owned_file_ready(&self.paths.machine_spec(name)?)?
            && owned_file_ready(&self.paths.machine_state(name)?)?)
    }
}

impl Dispatcher for LocalDispatcher {
    fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            match action {
                Action::Create { name, spec } => self.create(&name, spec, events),
                Action::List => self.list(events),
                Action::Show { name } => self.show(&name, events),
                Action::SetSpec { name, spec } => self.set_spec(&name, spec, events),
                Action::Doctor { fix } => self.doctor(fix, events),
                _ => Err(FirestoneError::new(
                    ErrorKind::Usage,
                    "this action is not implemented in the M0 CLI",
                )
                .with_hint("use create, ls, show, edit, or doctor")),
            }
        })
    }
}

fn parse_editor_command(value: OsString) -> Result<(String, Vec<String>), FirestoneError> {
    let value = value.into_string().map_err(|_| {
        FirestoneError::new(ErrorKind::Dependency, "VISUAL or EDITOR is not valid UTF-8")
            .with_hint("set VISUAL or EDITOR to a UTF-8 executable command")
    })?;
    let mut words = shlex::split(&value).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "VISUAL or EDITOR contains an unterminated quoted argument",
        )
        .with_hint("quote editor arguments using shell-style single or double quotes")
    })?;
    if words.first().is_none_or(String::is_empty) {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            "VISUAL or EDITOR does not name an executable",
        )
        .with_hint("set VISUAL or EDITOR to an executable optionally followed by arguments"));
    }
    let program = words.remove(0);
    Ok((program, words))
}

fn render_spec(spec: &MachineSpec) -> Result<String, FirestoneError> {
    let effective = spec.to_toml()?;
    Ok(format!("{SPEC_TEMPLATE}\n{effective}"))
}

fn display_status(state: &MachineState) -> String {
    match state.status {
        MachineStatus::Created => "created",
        MachineStatus::Starting => "starting",
        MachineStatus::Running if state.degraded.is_empty() => "running",
        MachineStatus::Running => "running!",
        MachineStatus::Stopping => "stopping",
        MachineStatus::Stopped => "stopped",
        MachineStatus::Failed => "failed",
    }
    .to_owned()
}

fn display_forward(forward: &str) -> String {
    match forward.rsplit_once(':') {
        Some((host, guest)) => format!("{host}→{guest}"),
        None => forward.to_owned(),
    }
}

fn ensure_owned_directory(path: &Path, label: &str, recursive: bool) -> Result<(), FirestoneError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive).mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot create {label} {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            ));
        }
    }
    validate_owned_directory(path, label, false)
}

fn validate_owned_directory(
    path: &Path,
    label: &str,
    allow_missing: bool,
) -> Result<(), FirestoneError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if allow_missing && source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect {label} {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            ));
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Generic,
        format!(
            "{label} {} is not a regular owned directory",
            path.display()
        ),
    )
    .with_hint("replace the symlink or special file with a regular Firestone-owned directory"))
}

fn ensure_machine_exists(name: &str, machine_dir: &Path) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(machine_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(FirestoneError::new(
            ErrorKind::NotFound,
            format!("machine path for `{name}` is a symbolic link"),
        )
        .with_hint("remove the symbolic link and recreate the machine")),
        Ok(metadata) if metadata.is_dir() => {
            match fs::symlink_metadata(machine_dir.join(".creating")) {
                Ok(_) => Err(FirestoneError::new(
                    ErrorKind::Busy,
                    format!("machine `{name}` is still being created"),
                )
                .with_hint("wait for create to finish and retry")),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot inspect creation marker for machine `{name}`"),
                    "check the machine directory permissions",
                    source,
                )),
            }
        }
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::NotFound,
            format!("machine path for `{name}` is not a directory"),
        )
        .with_hint("remove the invalid machine path and recreate the machine")),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Err(FirestoneError::new(
            ErrorKind::NotFound,
            format!("no machine named `{name}`"),
        )
        .with_hint("run `firestone ls` to list machines")),
        Err(source) => Err(filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect machine directory {}", machine_dir.display()),
            "check the machine directory permissions",
            source,
        )),
    }
}

fn owned_file_ready(path: &Path) -> Result<bool, FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect owned file {}", path.display()),
            "check the machine directory permissions",
            source,
        )),
    }
}

fn ensure_owned_regular_file(
    path: &Path,
    label: &str,
    kind: ErrorKind,
) -> Result<(), FirestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        filesystem_error(
            kind,
            format!("cannot inspect {label} {}", path.display()),
            "check that the file exists and is readable",
            source,
        )
    })?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        return Ok(());
    }
    Err(FirestoneError::new(
        kind,
        format!("{label} {} is not a regular owned file", path.display()),
    )
    .with_hint("replace the symlink or special file with a regular Firestone-owned file"))
}

fn read_file(path: &Path, label: &str, kind: ErrorKind) -> Result<Vec<u8>, FirestoneError> {
    ensure_owned_regular_file(path, label, kind)?;
    fs::read(path).map_err(|source| {
        filesystem_error(
            kind,
            format!("cannot read {label} {}", path.display()),
            "check that the file exists and is readable",
            source,
        )
    })
}

fn remove_candidate(path: &Path) -> Result<(), FirestoneError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(filesystem_error(
            ErrorKind::Generic,
            format!("cannot remove edit candidate {}", path.display()),
            "remove the stale edit file before retrying",
            source,
        )),
    }
}

fn emit_running_spec_warning(
    state: &MachineState,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    if state.status == MachineStatus::Running {
        events.emit(Event::Log {
            level: Level::Warn,
            message: "machine is running; spec changes take effect on next start".to_owned(),
        })?;
    }
    Ok(())
}

fn emit_spec_warnings(
    warnings: &[SpecWarningPayload],
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    for warning in warnings {
        events.emit(Event::Log {
            level: Level::Warn,
            message: format!("{}: {}", warning.key, warning.message),
        })?;
    }
    Ok(())
}

fn emit_result<T: serde::Serialize>(
    events: &mut dyn EventSink,
    action: &str,
    payload: &T,
) -> Result<(), FirestoneError> {
    let payload = serde_json::to_value(payload).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot serialize `{action}` result"),
        )
        .with_hint("report this Firestone serialization bug")
        .with_source(source)
    })?;
    events.emit(Event::Result {
        action: action.to_owned(),
        payload,
    })
}

fn filesystem_error(
    kind: ErrorKind,
    message: impl Into<String>,
    hint: impl Into<String>,
    source: std::io::Error,
) -> FirestoneError {
    FirestoneError::new(kind, message)
        .with_hint(hint)
        .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use firestone_core::{
        Action, Catalog, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig,
        PathInputs, Paths,
    };

    use super::{LocalDispatcher, display_forward, parse_editor_command};

    fn fixture() -> Result<(tempfile::TempDir, LocalDispatcher, Paths), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let root = fs::canonicalize(directory.path())?;
        let paths = Paths::from_inputs(&PathInputs {
            current_dir: root.clone(),
            home_dir: Some(root.clone()),
            firestone_home: Some(root.join("home")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: 1000,
        })?;
        let dispatcher =
            LocalDispatcher::new(paths.clone(), GlobalConfig::default(), Catalog::built_in()?);
        Ok((directory, dispatcher, paths))
    }

    #[test]
    fn display_forward_replaces_only_guest_separator() {
        assert_eq!(
            display_forward("udp:127.0.0.1:5353:53"),
            "udp:127.0.0.1:5353→53"
        );
    }

    #[test]
    fn editor_command_parses_quoted_arguments_without_a_shell()
    -> Result<(), Box<dyn std::error::Error>> {
        let (program, arguments) =
            parse_editor_command(std::ffi::OsString::from("code --wait 'profile name'"))?;

        assert_eq!(program, "code");
        assert_eq!(arguments, ["--wait", "profile name"]);
        assert!(parse_editor_command(std::ffi::OsString::from("code 'unterminated")).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn create_then_list_persists_effective_spec_and_created_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Create {
                    name: "ubuntu".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;
        dispatcher.run(Action::List, &mut events).await?;

        assert!(paths.machine_spec("ubuntu")?.exists());
        assert!(paths.machine_state("ubuntu")?.exists());
        let state = firestone_core::StateStore::new(paths.machine_state("ubuntu")?).read()?;
        assert_eq!(state.status, firestone_core::MachineStatus::Created);
        assert_eq!(state.image.r#ref, "ubuntu:24.04");
        assert_eq!(state.image.id, None);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Result { .. }))
                .count(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_reconciles_stale_running_state_before_rendering()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Create {
                    name: "stale".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;

        let store = firestone_core::StateStore::new(paths.machine_state("stale")?);
        let mut state = store.read()?;
        state.status = firestone_core::MachineStatus::Running;
        state.shim_pid = Some(u32::MAX);
        state.vmm_pid = Some(u32::MAX - 1);
        state.runtime_dir = paths.machine_runtime_dir("stale")?;
        state.started_at = Some("2026-08-28T09:12:44Z".to_owned());
        let lock = firestone_core::MachineLock::acquire(
            "stale",
            &paths.machine_lock("stale")?,
            &mut events,
        )?;
        store.write_from_locked_action(&state, &lock)?;
        drop(lock);

        events.clear();
        dispatcher.run(Action::List, &mut events).await?;

        let reconciled = store.read()?;
        assert_eq!(reconciled.status, firestone_core::MachineStatus::Stopped);
        assert_eq!(reconciled.shim_pid, None);
        assert_eq!(reconciled.vmm_pid, None);
        Ok(())
    }

    #[tokio::test]
    async fn doctor_action_emits_all_check_results() -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        let mut events = Vec::new();

        dispatcher
            .run(Action::Doctor { fix: false }, &mut events)
            .await?;

        let result = events.into_iter().find_map(|event| match event {
            Event::Result { action, payload } if action == "doctor" => Some(payload),
            _ => None,
        });
        let Some(result) = result else {
            panic!("doctor result event missing");
        };
        assert_eq!(
            result
                .get("checks")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(13)
        );
        Ok(())
    }

    #[tokio::test]
    async fn create_existing_name_returns_already_exists_without_replacing_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        let action = Action::Create {
            name: "demo".to_owned(),
            spec: firestone_core::MachineSpec::default(),
        };
        dispatcher.run(action.clone(), &mut events).await?;
        let before = fs::read(paths.machine_spec("demo")?)?;

        let error = dispatcher.run(action, &mut events).await.err();

        assert_eq!(
            error.as_ref().map(firestone_core::FirestoneError::kind),
            Some(firestone_core::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(paths.machine_spec("demo")?)?, before);
        Ok(())
    }

    #[tokio::test]
    async fn create_invalid_name_does_not_create_data_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Create {
                    name: "../invalid".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await
            .err();

        assert_eq!(
            error.as_ref().map(FirestoneError::kind),
            Some(ErrorKind::InvalidSpec)
        );
        assert!(!paths.machines_dir().exists());
        Ok(())
    }

    #[tokio::test]
    async fn machine_operations_reject_symlinked_machines_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.data_dir())?;
        let outside = directory.path().join("outside-machines");
        fs::create_dir(&outside)?;
        symlink(&outside, paths.machines_dir())?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Create {
                    name: "redirected".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await
            .err();

        assert_eq!(
            error.as_ref().map(firestone_core::FirestoneError::kind),
            Some(ErrorKind::Generic)
        );
        for action in [
            Action::List,
            Action::Show {
                name: "redirected".to_owned(),
            },
            Action::SetSpec {
                name: "redirected".to_owned(),
                spec: firestone_core::MachineSpec::default(),
            },
            Action::Doctor { fix: false },
        ] {
            let error = dispatcher.run(action, &mut events).await.err();
            assert_eq!(
                error.as_ref().map(firestone_core::FirestoneError::kind),
                Some(ErrorKind::Generic)
            );
        }
        let edit_error = dispatcher.edit("redirected", &mut events).err();
        assert_eq!(
            edit_error
                .as_ref()
                .map(firestone_core::FirestoneError::kind),
            Some(ErrorKind::Generic)
        );

        assert!(!outside.join("redirected").exists());
        Ok(())
    }

    #[tokio::test]
    async fn show_rejects_symlinked_machine_directory() -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.machines_dir())?;
        let outside = directory.path().join("outside");
        fs::create_dir(&outside)?;
        std::os::unix::fs::symlink(&outside, paths.machine_dir("linked")?)?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Show {
                    name: "linked".to_owned(),
                },
                &mut events,
            )
            .await
            .err();

        assert_eq!(
            error.as_ref().map(FirestoneError::kind),
            Some(ErrorKind::NotFound)
        );
        assert!(outside.exists());
        Ok(())
    }

    struct FailingSink;

    impl EventSink for FailingSink {
        fn emit(&mut self, _event: Event) -> Result<(), FirestoneError> {
            Err(
                FirestoneError::new(ErrorKind::Generic, "injected result sink failure")
                    .with_hint("test failure"),
            )
        }
    }

    #[tokio::test]
    async fn create_result_sink_failure_preserves_committed_machine()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = FailingSink;

        let error = dispatcher
            .run(
                Action::Create {
                    name: "partial".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await
            .err();

        assert!(error.is_some());
        assert!(paths.machine_dir("partial")?.exists());
        assert!(paths.machine_spec("partial")?.exists());
        assert!(paths.machine_state("partial")?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn machine_names_ignore_incomplete_and_non_directory_entries_and_sort()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        for name in ["zeta", "alpha"] {
            dispatcher
                .run(
                    Action::Create {
                        name: name.to_owned(),
                        spec: firestone_core::MachineSpec::default(),
                    },
                    &mut events,
                )
                .await?;
        }
        fs::create_dir(paths.machine_dir("pending")?)?;
        fs::copy(paths.machine_spec("alpha")?, paths.machine_spec("pending")?)?;
        fs::copy(
            paths.machine_state("alpha")?,
            paths.machine_state("pending")?,
        )?;
        fs::write(
            paths.machine_dir("pending")?.join(".creating"),
            b"creating\n",
        )?;
        fs::write(paths.machines_dir().join("README"), b"ignored")?;

        assert_eq!(dispatcher.machine_names()?, vec!["alpha", "zeta"]);
        Ok(())
    }

    #[test]
    fn load_machine_incomplete_publication_returns_busy() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_directory, dispatcher, paths) = fixture()?;
        let machine_dir = paths.machine_dir("pending")?;
        fs::create_dir_all(&machine_dir)?;
        fs::write(machine_dir.join(".creating"), b"creating\n")?;

        let error = dispatcher.load_machine("pending").err();

        assert_eq!(
            error.as_ref().map(firestone_core::FirestoneError::kind),
            Some(ErrorKind::Busy)
        );
        Ok(())
    }

    #[tokio::test]
    async fn load_machine_rejects_symlinked_owned_files() -> Result<(), Box<dyn std::error::Error>>
    {
        let (directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Create {
                    name: "demo".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;

        let spec_path = paths.machine_spec("demo")?;
        let external_spec = directory.path().join("external-spec.toml");
        fs::copy(&spec_path, &external_spec)?;
        fs::remove_file(&spec_path)?;
        symlink(&external_spec, &spec_path)?;
        let spec_error = dispatcher.load_machine("demo").err();
        assert_eq!(
            spec_error
                .as_ref()
                .map(firestone_core::FirestoneError::kind),
            Some(ErrorKind::NotFound)
        );

        fs::remove_file(&spec_path)?;
        fs::copy(&external_spec, &spec_path)?;
        let state_path = paths.machine_state("demo")?;
        let external_state = directory.path().join("external-state.json");
        fs::copy(&state_path, &external_state)?;
        fs::remove_file(&state_path)?;
        symlink(&external_state, &state_path)?;
        let state_error = dispatcher.load_machine("demo").err();
        assert_eq!(
            state_error
                .as_ref()
                .map(firestone_core::FirestoneError::kind),
            Some(ErrorKind::Generic)
        );
        Ok(())
    }
}
