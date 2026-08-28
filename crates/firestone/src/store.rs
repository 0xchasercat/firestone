use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

use firestone_core::{
    Action, Arch, Catalog, Cmd, DependencyManifest, DispatchFuture, Dispatcher, DoctorContext,
    ErrorKind, Event, EventSink, FirestoneError, GlobalConfig, Level, LiveMachineState,
    MachineLock, MachineRecord, MachineSpec, MachineSpecPatch, MachineState, MachineStatus,
    MachineSummary, MachineView, Paths, RealValidationHost, SpecResult, SpecWarningPayload,
    StateImage, StateStore, StateVersion, Supervision, ValidationContext, atomic,
    read_reconciled_machine_state_live, read_reconciled_machine_state_live_locked, run_doctor,
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
        self.paths
            .validate_owned_data_directory(self.paths.data_dir(), "data directory", true)?;
        self.paths.validate_owned_data_directory(
            &self.paths.machines_dir(),
            "machines directory",
            true,
        )
    }

    pub fn edit(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        let spec_path = self.paths.machine_spec(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let lock_path = self.paths.machine_lock(name)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let observed_state = self.read_live_state_locked(name, &lock)?;
        let original = read_file(&spec_path, "machine spec", ErrorKind::NotFound)?;
        let candidate = spec_path.with_extension("toml.edit");
        atomic::write(&candidate, &original)?;

        let pinned_image_ref = if observed_state.state.image.id.is_some()
            && observed_state.state.image.sha256.is_some()
        {
            Some(observed_state.state.image.r#ref.as_str())
        } else {
            None
        };
        let result = self.edit_candidate(
            name,
            &machine_dir,
            &candidate,
            &spec_path,
            pinned_image_ref,
            events,
        );
        let cleanup = remove_candidate(&candidate);
        match (result, cleanup) {
            (Ok(result), Ok(())) => {
                emit_running_spec_warning(&observed_state.state, events)?;
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
        pinned_image_ref: Option<&str>,
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
            match self.load_spec_text_with_pinned(
                candidate_text,
                machine_dir,
                machine_dir,
                pinned_image_ref,
            ) {
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
        self.paths
            .ensure_owned_data_directory(self.paths.data_dir(), "data directory", true)?;
        self.paths
            .ensure_owned_data_directory(&machines_dir, "machines directory", false)?;

        let (lock, creating_marker) = self.prepare_machine_creation(name, &machine_dir, events)?;
        let mut record = self.initialize_machine(name, spec, &lock)?;

        if edit {
            let spec_path = self.paths.machine_spec(name)?;
            let candidate = spec_path.with_extension("toml.edit");
            let original = read_file(&spec_path, "machine spec", ErrorKind::Generic)?;
            atomic::write(&candidate, &original)?;
            let edited =
                self.edit_candidate(name, &machine_dir, &candidate, &spec_path, None, events);
            let cleanup = remove_candidate(&candidate);
            let edited = match (edited, cleanup) {
                (Ok(edited), Ok(())) => edited,
                (Ok(_), Err(error)) | (Err(error), _) => return Err(error),
            };
            emit_spec_warnings(&edited.warnings, events)?;
            let state = self.created_state(name, &edited.spec)?;
            StateStore::new(self.paths.machine_state(name)?)
                .write_from_locked_action(&state, &lock)?;
            record.spec = edited.spec;
            record.state = state;
        }

        fs::remove_file(&creating_marker).map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot publish machine `{name}`"),
                "check the machine directory permissions",
                source,
            )
        })?;
        emit_result(events, "create", &record)
    }

    fn prepare_machine_creation(
        &self,
        name: &str,
        machine_dir: &Path,
        events: &mut dyn EventSink,
    ) -> Result<(MachineLock, PathBuf), FirestoneError> {
        self.paths
            .ensure_owned_data_directory(machine_dir, "machine directory", false)?;
        let creating_marker = machine_dir.join(".creating");
        let lock_path = self.paths.machine_lock(name)?;
        validate_creation_lock_file(&lock_path, name, true)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        self.paths
            .validate_owned_data_directory(machine_dir, "machine directory", false)?;
        validate_creation_lock_file(&lock_path, name, false)?;

        let marker_exists = creation_marker_exists(&creating_marker, name)?;
        if self.machine_publication_complete(name, machine_dir)? {
            if marker_exists {
                fs::remove_file(&creating_marker).map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot finalize machine `{name}` after interrupted creation"),
                        "check the machine directory permissions",
                        source,
                    )
                })?;
            }
            return Err(machine_already_exists_error(name));
        }

        if marker_exists {
            clear_incomplete_machine(machine_dir, name)?;
        } else if machine_has_non_lock_entries(machine_dir, name)? {
            return Err(machine_already_exists_error(name));
        }

        if !marker_exists {
            fs::write(&creating_marker, b"creating\n").map_err(|source| {
                filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot mark machine `{name}` as being created"),
                    "check the machine directory permissions",
                    source,
                )
            })?;
        }

        Ok((lock, creating_marker))
    }

    fn machine_publication_complete(
        &self,
        name: &str,
        machine_dir: &Path,
    ) -> Result<bool, FirestoneError> {
        let spec_path = self.paths.machine_spec(name)?;
        let state_path = self.paths.machine_state(name)?;
        if !owned_file_ready(&spec_path)? || !owned_file_ready(&state_path)? {
            return Ok(false);
        }

        let source = read_file(&spec_path, "machine spec", ErrorKind::InvalidSpec)?;
        let text = std::str::from_utf8(&source).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine spec for `{name}` is not UTF-8"),
            )
            .with_hint("save firestone.toml as UTF-8 TOML")
            .with_source(source)
        })?;
        let state = StateStore::new(state_path).read()?;
        let pinned_image_ref = if state.image.id.is_some() && state.image.sha256.is_some() {
            Some(state.image.r#ref.as_str())
        } else {
            None
        };
        self.load_spec_text_with_pinned(text, machine_dir, machine_dir, pinned_image_ref)?;
        Ok(true)
    }
    fn initialize_machine(
        &self,
        name: &str,
        mut spec: MachineSpec,
        lock: &MachineLock,
    ) -> Result<MachineRecord, FirestoneError> {
        let state = self.created_state(name, &spec)?;
        spec.image = state.image.r#ref.clone().into();
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
        let now = jiff::Timestamp::now();
        let mut machines = Vec::with_capacity(names.len());
        for name in names {
            let (spec, live) = self.load_machine(&name)?;
            let state = &live.state;
            machines.push(MachineSummary {
                name,
                status: display_status(state.status, !state.degraded.is_empty(), live.supervision),
                image: state.image.r#ref.clone(),
                cpus: spec.cpus,
                memory: spec.memory.to_string(),
                uptime: display_uptime(state, &now),
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
        let (spec, live) = self.load_machine(name)?;
        emit_result(
            events,
            "show",
            &MachineView {
                spec,
                state: live.state,
                supervision: live.supervision,
            },
        )
    }

    fn set_spec(
        &self,
        name: &str,
        spec: MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let lock_path = self.paths.machine_lock(name)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let observed_state = self.read_live_state_locked(name, &lock)?;
        let document = render_spec(&spec)?;
        atomic::write(&self.paths.machine_spec(name)?, document.as_bytes())?;
        emit_running_spec_warning(&observed_state.state, events)?;
        emit_result(
            events,
            "edit",
            &SpecResult {
                spec,
                warnings: Vec::new(),
            },
        )
    }

    fn read_live_state(&self, name: &str) -> Result<LiveMachineState, FirestoneError> {
        let state_path = self.paths.machine_state(name)?;
        ensure_owned_regular_file(&state_path, "machine state", ErrorKind::Generic)?;
        let reconciled_at = jiff::Timestamp::now().to_string();
        read_reconciled_machine_state_live(&self.paths, name, &reconciled_at)
    }
    fn read_live_state_locked(
        &self,
        name: &str,
        lock: &MachineLock,
    ) -> Result<LiveMachineState, FirestoneError> {
        let state_path = self.paths.machine_state(name)?;
        ensure_owned_regular_file(&state_path, "machine state", ErrorKind::Generic)?;
        let reconciled_at = jiff::Timestamp::now().to_string();
        read_reconciled_machine_state_live_locked(&self.paths, name, &reconciled_at, lock)
    }

    fn load_machine(&self, name: &str) -> Result<(MachineSpec, LiveMachineState), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let state = self.read_live_state(name)?;
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
        let pinned_image_ref =
            if state.state.image.id.is_some() && state.state.image.sha256.is_some() {
                Some(state.state.image.r#ref.as_str())
            } else {
                None
            };
        let loaded =
            self.load_spec_text_with_pinned(text, &machine_dir, &machine_dir, pinned_image_ref)?;
        Ok((loaded.spec, state))
    }

    fn load_spec_text_with_pinned(
        &self,
        text: &str,
        machine_dir: &Path,
        patch_base_dir: &Path,
        pinned_image_ref: Option<&str>,
    ) -> Result<firestone_core::LoadedMachineSpec, FirestoneError> {
        let host = RealValidationHost::new();
        let context = ValidationContext::new(&host, &self.paths, machine_dir, &self.catalog);
        let context = match pinned_image_ref {
            Some(reference) => context.with_pinned_image_ref(reference),
            None => context,
        };
        MachineSpec::load(
            text,
            &self.global,
            &MachineSpecPatch::default(),
            patch_base_dir,
            &context,
        )
    }
    fn doctor(&self, fix: bool, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
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
        let machine_dir = self.paths.machine_dir(name)?;
        self.paths
            .validate_owned_data_directory(&machine_dir, "machine directory", false)?;
        let marker = machine_dir.join(".creating");
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

fn display_status(
    status: MachineStatus,
    degraded: bool,
    supervision: Option<Supervision>,
) -> String {
    if status == MachineStatus::Running {
        let base = if degraded { "running!" } else { "running" };
        return if supervision == Some(Supervision::Unsupervised) {
            format!("{base} (unsupervised)")
        } else {
            base.to_owned()
        };
    }

    match status {
        MachineStatus::Created => "created",
        MachineStatus::Starting => "starting",
        MachineStatus::Stopping => "stopping",
        MachineStatus::Stopped => "stopped",
        MachineStatus::Failed => "failed",
        MachineStatus::Running => unreachable!("running status returned above"),
    }
    .to_owned()
}

fn display_uptime(state: &MachineState, now: &jiff::Timestamp) -> Option<String> {
    if !state.status.is_active() {
        return None;
    }
    let started_at = state
        .started_at
        .as_deref()?
        .parse::<jiff::Timestamp>()
        .ok()?;
    let seconds = now.as_second().checked_sub(started_at.as_second())?;
    let seconds = u64::try_from(seconds).ok()?;
    Some(format_uptime_seconds(seconds))
}

fn format_uptime_seconds(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (24 * 60 * 60))
    }
}

fn display_forward(forward: &str) -> String {
    match forward.rsplit_once(':') {
        Some((host, guest)) => format!("{host}→{guest}"),
        None => forward.to_owned(),
    }
}

fn creation_marker_exists(path: &Path, name: &str) -> Result<bool, FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("creation marker for machine `{name}` is not a regular file"),
        )
        .with_hint("move the invalid machine directory aside and retry")),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect creation marker for machine `{name}`"),
            "check the machine directory permissions",
            source,
        )),
    }
}

fn validate_creation_lock_file(
    path: &Path,
    name: &str,
    allow_missing: bool,
) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("machine lock for `{name}` is not a regular file"),
        )
        .with_hint("move the invalid machine directory aside and retry")),
        Err(source) if allow_missing && source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect machine lock for `{name}`"),
            "check the machine directory permissions",
            source,
        )),
    }
}

fn clear_incomplete_machine(machine_dir: &Path, name: &str) -> Result<(), FirestoneError> {
    let entries = fs::read_dir(machine_dir).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect incomplete machine `{name}`"),
            "check the machine directory permissions",
            source,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect incomplete machine `{name}`"),
                "check the machine directory permissions",
                source,
            )
        })?;
        let file_name = entry.file_name();
        if file_name.as_os_str() == OsStr::new("lock")
            || file_name.as_os_str() == OsStr::new(".creating")
        {
            continue;
        }

        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect stale machine path {}", path.display()),
                "check the machine directory permissions",
                source,
            )
        })?;
        let removal = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removal.map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot remove stale machine path {}", path.display()),
                "check the machine directory permissions",
                source,
            )
        })?;
    }
    Ok(())
}
fn machine_has_non_lock_entries(machine_dir: &Path, name: &str) -> Result<bool, FirestoneError> {
    let entries = fs::read_dir(machine_dir).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect incomplete machine `{name}`"),
            "check the machine directory permissions",
            source,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect incomplete machine `{name}`"),
                "check the machine directory permissions",
                source,
            )
        })?;
        if entry.file_name().as_os_str() != OsStr::new("lock") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn machine_already_exists_error(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::AlreadyExists,
        format!("machine `{name}` already exists"),
    )
    .with_hint(format!(
        "use `firestone show {name}` or choose another name"
    ))
}
fn ensure_machine_exists(
    paths: &Paths,
    name: &str,
    machine_dir: &Path,
) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(machine_dir) {
        Ok(_) => {
            paths.validate_owned_data_directory(machine_dir, "machine directory", false)?;
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
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use firestone_core::{
        Action, Catalog, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig,
        MachineLock, MachineSpec, MachineSpecPatch, MachineStatus, PathInputs, Paths,
        RealValidationHost, StateStore, Supervision, ValidationContext,
    };

    use super::{
        LocalDispatcher, display_forward, display_status, format_uptime_seconds,
        parse_editor_command,
    };
    struct WaitingSink(mpsc::Sender<()>);

    impl EventSink for WaitingSink {
        fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
            if matches!(
                event,
                Event::Log { ref message, .. }
                    if message.starts_with("waiting for another firestone operation")
            ) {
                let _ = self.0.send(());
            }
            Ok(())
        }
    }
    fn fixture() -> Result<(tempfile::TempDir, LocalDispatcher, Paths), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let root = fs::canonicalize(directory.path())?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let firestone_home = root.join("home");
        fs::create_dir(&firestone_home)?;
        fs::set_permissions(&firestone_home, fs::Permissions::from_mode(0o700))?;
        let paths = Paths::from_inputs(&PathInputs {
            current_dir: root.clone(),
            home_dir: Some(root.clone()),
            firestone_home: Some(firestone_home),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: fs::metadata(&root)?.uid(),
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
    async fn create_catalog_alias_persists_canonical_image_reference()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        let spec = firestone_core::MachineSpec {
            image: "ubuntu".into(),
            ..firestone_core::MachineSpec::default()
        };

        dispatcher
            .run(
                Action::Create {
                    name: "canonical".to_owned(),
                    spec,
                },
                &mut events,
            )
            .await?;

        let persisted = fs::read_to_string(paths.machine_spec("canonical")?)?;
        let patch = firestone_core::MachineSpecPatch::from_toml(&persisted)?;
        assert_eq!(
            patch.image.as_ref().map(|image| image.as_str()),
            Some("ubuntu:24.04")
        );
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
    async fn pinned_relative_local_image_remains_listable_and_showable_after_source_deletion()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, paths) = fixture()?;
        let root = fs::canonicalize(directory.path())?;
        let source = root.join("relative.qcow2");
        fs::write(&source, b"QFI\xFBLOCAL")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        let catalog = Catalog::built_in()?;
        let host = RealValidationHost::new();
        let machine_dir = paths.machine_dir("local-pin")?;
        let patch = MachineSpecPatch {
            image: Some("relative.qcow2".into()),
            ..MachineSpecPatch::default()
        };
        let loaded = MachineSpec::load(
            "",
            &GlobalConfig::default(),
            &patch,
            &root,
            &ValidationContext::new(&host, &paths, &machine_dir, &catalog),
        )?;
        assert_eq!(loaded.spec.image.as_str(), source.to_string_lossy());

        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Create {
                    name: "local-pin".to_owned(),
                    spec: loaded.spec,
                },
                &mut events,
            )
            .await?;
        let state_path = paths.machine_state("local-pin")?;
        let mut state = StateStore::new(state_path.clone()).read()?;
        assert_eq!(state.image.r#ref, source.to_string_lossy());
        state.image.id = Some(format!("image-{}", "a".repeat(64)));
        state.image.sha256 = Some("b".repeat(64));
        StateStore::new(state_path).write_from_shim(&state)?;
        fs::remove_file(&source)?;

        dispatcher.run(Action::List, &mut events).await?;
        dispatcher
            .run(
                Action::Show {
                    name: "local-pin".to_owned(),
                },
                &mut events,
            )
            .await?;
        let duplicate = dispatcher
            .run(
                Action::Create {
                    name: "local-pin".to_owned(),
                    spec: MachineSpec::default(),
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("expected duplicate machine rejection")?;
        assert_eq!(duplicate.kind(), ErrorKind::AlreadyExists);
        assert!(!source.exists());
        Ok(())
    }

    #[tokio::test]
    async fn removed_catalog_machine_requires_complete_pin_before_spec_reload()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        let spec = MachineSpec {
            image: "retired:1".into(),
            ..MachineSpec::default()
        };
        dispatcher
            .run(
                Action::Create {
                    name: "retired".to_owned(),
                    spec,
                },
                &mut events,
            )
            .await?;

        let unpinned = dispatcher
            .run(Action::List, &mut events)
            .await
            .err()
            .ok_or("expected unresolved unpinned catalog rejection")?;
        assert_eq!(unpinned.kind(), ErrorKind::InvalidSpec);

        let state_path = paths.machine_state("retired")?;
        let mut state = StateStore::new(state_path.clone()).read()?;
        state.image.id = Some(format!("image-{}", "c".repeat(64)));
        state.image.sha256 = Some("d".repeat(64));
        StateStore::new(state_path).write_from_shim(&state)?;

        dispatcher.run(Action::List, &mut events).await?;
        dispatcher
            .run(
                Action::Show {
                    name: "retired".to_owned(),
                },
                &mut events,
            )
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn create_missing_storage_creates_mode_0700() -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Create {
                    name: "secure".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;

        for path in [
            paths.data_dir().to_path_buf(),
            paths.machines_dir(),
            paths.machine_dir("secure")?,
        ] {
            assert_eq!(
                fs::symlink_metadata(path)?.permissions().mode() & 0o7777,
                0o700
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn create_permissive_data_directory_returns_dependency_without_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.data_dir())?;
        fs::set_permissions(paths.data_dir(), fs::Permissions::from_mode(0o777))?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Create {
                    name: "blocked".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await
            .err();

        assert_eq!(
            error.as_ref().map(FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!paths.machines_dir().exists());
        Ok(())
    }

    #[tokio::test]
    async fn create_permissive_machines_directory_returns_dependency_without_mutation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.machines_dir())?;
        fs::set_permissions(paths.data_dir(), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(paths.machines_dir(), fs::Permissions::from_mode(0o777))?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Create {
                    name: "blocked".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await
            .err();

        assert_eq!(
            error.as_ref().map(FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        assert!(!paths.machine_dir("blocked")?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn create_stale_incomplete_publication_recovers_and_retries()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.machines_dir())?;
        fs::set_permissions(paths.data_dir(), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(paths.machines_dir(), fs::Permissions::from_mode(0o700))?;
        let machine_dir = paths.machine_dir("stale-create")?;
        fs::create_dir(&machine_dir)?;
        fs::set_permissions(&machine_dir, fs::Permissions::from_mode(0o700))?;
        fs::write(machine_dir.join(".creating"), b"creating\n")?;
        fs::write(machine_dir.join("partial"), b"stale")?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Create {
                    name: "stale-create".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;

        assert!(!machine_dir.join(".creating").exists());
        assert!(!machine_dir.join("partial").exists());
        assert!(paths.machine_spec("stale-create")?.exists());
        assert!(paths.machine_state("stale-create")?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn create_complete_publication_with_stale_marker_preserves_machine()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Create {
                    name: "complete".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;
        let marker = paths.machine_dir("complete")?.join(".creating");
        fs::write(&marker, b"creating\n")?;
        let replacement = firestone_core::MachineSpec {
            cpus: 8,
            ..firestone_core::MachineSpec::default()
        };

        let error = dispatcher
            .run(
                Action::Create {
                    name: "complete".to_owned(),
                    spec: replacement,
                },
                &mut events,
            )
            .await
            .err();

        assert_eq!(
            error.as_ref().map(FirestoneError::kind),
            Some(ErrorKind::AlreadyExists)
        );
        assert!(!marker.exists());
        let (spec, _) = dispatcher.load_machine("complete")?;
        assert_eq!(spec.cpus, 2);
        Ok(())
    }

    #[tokio::test]
    async fn create_empty_markerless_directory_recovers_and_publishes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.machines_dir())?;
        fs::set_permissions(paths.data_dir(), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(paths.machines_dir(), fs::Permissions::from_mode(0o700))?;
        let machine_dir = paths.machine_dir("pre-marker")?;
        fs::create_dir(&machine_dir)?;
        fs::set_permissions(&machine_dir, fs::Permissions::from_mode(0o700))?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Create {
                    name: "pre-marker".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;

        assert!(paths.machine_spec("pre-marker")?.is_file());
        assert!(paths.machine_state("pre-marker")?.is_file());
        assert!(!machine_dir.join(".creating").exists());
        Ok(())
    }

    #[tokio::test]
    async fn list_permissive_machine_directory_returns_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Create {
                    name: "unsafe-machine".to_owned(),
                    spec: firestone_core::MachineSpec::default(),
                },
                &mut events,
            )
            .await?;
        fs::set_permissions(
            paths.machine_dir("unsafe-machine")?,
            fs::Permissions::from_mode(0o777),
        )?;
        events.clear();

        let error = dispatcher.run(Action::List, &mut events).await.err();

        assert_eq!(
            error.as_ref().map(FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );
        Ok(())
    }
    #[test]
    fn create_active_publication_becomes_already_exists_without_deletion()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        fs::create_dir_all(paths.machines_dir())?;
        fs::set_permissions(paths.data_dir(), fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(paths.machines_dir(), fs::Permissions::from_mode(0o700))?;
        let machine_dir = paths.machine_dir("active-create")?;
        fs::create_dir(&machine_dir)?;
        fs::set_permissions(&machine_dir, fs::Permissions::from_mode(0o700))?;
        let marker = machine_dir.join(".creating");
        fs::write(&marker, b"creating\n")?;
        let mut lock_events = Vec::new();
        let active_lock = MachineLock::acquire(
            "active-create",
            &paths.machine_lock("active-create")?,
            &mut lock_events,
        )?;
        let (sender, receiver) = mpsc::channel();

        let handle = thread::spawn(move || {
            let mut events = WaitingSink(sender);
            dispatcher
                .create_internal(
                    "active-create",
                    firestone_core::MachineSpec::default(),
                    false,
                    &mut events,
                )
                .err()
                .map(|error| error.kind())
        });
        receiver.recv_timeout(Duration::from_secs(3))?;
        let sentinel = machine_dir.join("published");
        fs::write(&sentinel, b"complete")?;
        fs::remove_file(&marker)?;
        drop(active_lock);

        let error_kind = handle
            .join()
            .map_err(|_| std::io::Error::other("create thread panicked"))?;
        assert_eq!(error_kind, Some(ErrorKind::AlreadyExists));
        assert_eq!(fs::read(sentinel)?, b"complete");
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
            Some(ErrorKind::Dependency)
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
        ] {
            let error = dispatcher.run(action, &mut events).await.err();
            assert_eq!(
                error.as_ref().map(firestone_core::FirestoneError::kind),
                Some(ErrorKind::Dependency)
            );
        }
        let edit_error = dispatcher.edit("redirected", &mut events).err();
        assert_eq!(
            edit_error
                .as_ref()
                .map(firestone_core::FirestoneError::kind),
            Some(ErrorKind::Dependency)
        );

        events.clear();
        dispatcher
            .run(Action::Doctor { fix: false }, &mut events)
            .await?;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Result { action, .. } if action == "doctor"))
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
            Some(ErrorKind::Dependency)
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
        fs::set_permissions(
            paths.machine_dir("pending")?,
            fs::Permissions::from_mode(0o700),
        )?;
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
        for path in [
            paths.data_dir().to_path_buf(),
            paths.machines_dir(),
            machine_dir.clone(),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
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
    #[test]
    fn display_status_running_without_shim_reports_unsupervised() {
        assert_eq!(
            display_status(
                MachineStatus::Running,
                false,
                Some(Supervision::Unsupervised),
            ),
            "running (unsupervised)"
        );
        assert_eq!(
            display_status(
                MachineStatus::Running,
                true,
                Some(Supervision::Unsupervised),
            ),
            "running! (unsupervised)"
        );
    }

    #[test]
    fn format_uptime_seconds_boundaries_use_short_units() {
        assert_eq!(format_uptime_seconds(41), "41s");
        assert_eq!(format_uptime_seconds(60), "1m");
        assert_eq!(format_uptime_seconds(3_600), "1h");
        assert_eq!(format_uptime_seconds(172_800), "2d");
    }
}
