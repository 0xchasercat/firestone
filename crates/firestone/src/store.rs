use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use firestone_core::{
    Action, Arch, ByteSize, Catalog, CatalogArchitectureSummary, CatalogEntry, CatalogEntrySummary,
    Cmd, CpResult, DependencyManifest, DispatchFuture, Dispatcher, DoctorContext, DoctorOptions,
    ErrorKind, Event, EventSink, ExtractedPasstHelper, FirestoneError, GlobalConfig,
    ImagePullRequest, ImageStore, InternalHelper, Level, LiveMachineState, LogSource, LogsResult,
    MachineLock, MachineRecord, MachineSpec, MachineSpecPatch, MachineState, MachineStatus,
    MachineSummary, MachineView, MetricsCpu, MetricsMemory, MetricsResult, Paths, PruneItem,
    PruneKind, PruneResult, ReadinessOptions, RealValidationHost, RemoveResult, ResizeResult,
    ShimClient, ShimTimeouts, SpecResult, SpecWarningPayload, StartResult, StateImage, StateStore,
    StateVersion, StopResult, Supervision, ValidationContext, VersionDependency, VersionIdentity,
    VersionPaths, VersionResult, VmmApi, atomic, cancel_prepared, classify_cp_operands,
    disk_shrink_error, forwards_differ, launch_prepared_cancellable, overlay_virtual_size,
    prepare_start, project_device_counters, read_reconciled_machine_state_live,
    read_reconciled_machine_state_live_locked, run_doctor, sample_vmm_process, scp_command_plan,
    stop_unsupervised, validate_machine_spec, wait_for_ssh_ready,
};
use firestone_core::{CloneResult, StepId};
use firestone_core::{
    RestoreRequest, SNAPSHOT_FILE_MODE, SNAPSHOT_SCHEMA_VERSION, SnapshotKind, SnapshotListResult,
    SnapshotMetadata, SnapshotRemoveResult, SnapshotRestoreResult, SnapshotResult, SnapshotSummary,
    allocated_bytes, auto_snapshot_name, available_bytes, create_snapshot_directory,
    ensure_snapshot_directory, read_snapshot_metadata, snapshot_document_digest as sha256_hex,
    snapshot_file_url, sparse_copy_file, validate_snapshot_name,
};

const SPEC_TEMPLATE: &str = include_str!("../../../templates/firestone.toml");
/// `firestone.toml` may hold a plaintext guest password (§10.5), so it is
/// published owner-read/write only rather than at the process umask.
const MACHINE_SPEC_FILE_MODE: u32 = 0o600;
const MAX_VMCONFIG_BYTES: u64 = 51_200;
const MAX_LOG_LINES: u32 = 100_000;
const MAX_LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const LOG_FOLLOW_CHUNK_BYTES: u64 = 256 * 1024;
const LOG_FOLLOW_INTERVAL: Duration = Duration::from_millis(100);

pub struct LocalDispatcher {
    paths: Paths,
    global: GlobalConfig,
    catalog: Catalog,
    source_base: PathBuf,
    qemu_img: PathBuf,
    shim_program: Option<PathBuf>,
    automatic_start_timeout: bool,
    start_cancellation: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct TerminalMachine {
    pub spec: MachineSpec,
    pub state: MachineState,
}

impl LocalDispatcher {
    pub fn new(paths: Paths, global: GlobalConfig, catalog: Catalog) -> Self {
        let source_base = match env::current_dir() {
            Ok(path) => path,
            Err(_) => paths.data_dir().to_path_buf(),
        };
        Self {
            paths,
            global,
            catalog,
            source_base,
            qemu_img: PathBuf::from("qemu-img"),
            shim_program: None,
            automatic_start_timeout: false,
            start_cancellation: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn with_source_base(mut self, source_base: PathBuf) -> Self {
        self.source_base = source_base;
        self
    }
    #[must_use]
    pub const fn with_automatic_start_timeout(mut self, automatic: bool) -> Self {
        self.automatic_start_timeout = automatic;
        self
    }
    #[must_use]
    pub fn with_start_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.start_cancellation = cancellation;
        self
    }

    #[cfg(test)]
    fn with_programs(mut self, qemu_img: PathBuf, shim_program: PathBuf) -> Self {
        self.qemu_img = qemu_img;
        self.shim_program = Some(shim_program);
        self
    }

    pub fn find_terminal_machine(
        &self,
        name: &str,
    ) -> Result<Option<TerminalMachine>, FirestoneError> {
        match fs::symlink_metadata(self.paths.machines_dir()) {
            Ok(_) => self.validate_machine_storage()?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Dependency,
                    "cannot inspect the machine storage directory",
                    "check the Firestone data directory permissions",
                    source,
                ));
            }
        }
        let machine_dir = match self.paths.machine_dir(name) {
            Ok(machine_dir) => machine_dir,
            Err(error) if error.kind() == ErrorKind::InvalidSpec => return Ok(None),
            Err(error) => return Err(error),
        };
        match fs::symlink_metadata(&machine_dir) {
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot inspect machine {name}"),
                    "check the machine directory permissions",
                    source,
                ));
            }
        }
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let live = self.read_live_state(name)?;
        let spec = self.load_machine_spec(name, &live.state)?;
        Ok(Some(TerminalMachine {
            spec,
            state: live.state,
        }))
    }

    pub fn terminal_machine(&self, name: &str) -> Result<TerminalMachine, FirestoneError> {
        self.find_terminal_machine(name)?.ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::NotFound,
                format!("machine {name} does not exist"),
            )
            .with_hint("run firestone ls to list machines")
        })
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
        atomic::write_with_mode(&candidate, &original, MACHINE_SPEC_FILE_MODE)?;

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
                emit_forward_restart_warning(&result.spec, &observed_state.state, events)?;
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
            .unwrap_or_else(|| OsString::from("nano"));
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
                    atomic::write_with_mode(spec_path, &candidate_bytes, MACHINE_SPEC_FILE_MODE)?;
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
        mut spec: MachineSpec,
        edit: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let machine_dir = self.paths.machine_dir(name)?;
        let warnings = self.validate_action_spec(&mut spec, None)?;
        emit_spec_warnings(&warnings, events)?;
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
            atomic::write_with_mode(&candidate, &original, MACHINE_SPEC_FILE_MODE)?;
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
        atomic::write_with_mode(
            &self.paths.machine_spec(name)?,
            spec_document.as_bytes(),
            MACHINE_SPEC_FILE_MODE,
        )?;
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

    /// Copies a stopped or created machine's spec and overlay to a new machine (SPEC section 24).
    fn clone_machine(
        &self,
        source: &str,
        dest: &str,
        fresh_disk: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let source_dir = self.paths.machine_dir(source)?;
        let dest_dir = self.paths.machine_dir(dest)?;
        if source == dest {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                format!("cannot clone machine `{source}` onto itself"),
            )
            .with_hint("choose a destination name that differs from the source"));
        }
        ensure_machine_exists(&self.paths, source, &source_dir)?;
        // A live machine holds its own machine lock, so refuse before waiting on it.
        ensure_clonable(source, self.read_live_state(source)?.state.status)?;

        // Stable lock order: the source machine first, then the destination
        // under the same creation path `create` uses.
        let source_lock = MachineLock::acquire(source, &self.paths.machine_lock(source)?, events)?;
        ensure_machine_exists(&self.paths, source, &source_dir)?;
        let live = self.read_live_state_locked(source, &source_lock)?;
        ensure_clonable(source, live.state.status)?;

        let spec_document = read_file(
            &self.paths.machine_spec(source)?,
            "machine spec",
            ErrorKind::NotFound,
        )?;
        let text = std::str::from_utf8(&spec_document).map_err(|source_error| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine spec for `{source}` is not UTF-8"),
            )
            .with_hint("save firestone.toml as UTF-8 TOML")
            .with_source(source_error)
        })?;
        // The copied document becomes the destination spec verbatim, so it must
        // already resolve against the destination machine directory.
        let loaded = self.load_spec_text_with_pinned(
            text,
            &dest_dir,
            &dest_dir,
            pinned_image_reference(&live.state),
        )?;
        if loaded.spec.network.mac.is_some() {
            events.emit(Event::Log {
                level: Level::Warn,
                message: format!(
                    "machine `{source}` pins network.mac; `{dest}` inherits the same address"
                ),
            })?;
        }

        self.paths
            .ensure_owned_data_directory(self.paths.data_dir(), "data directory", true)?;
        self.paths.ensure_owned_data_directory(
            &self.paths.machines_dir(),
            "machines directory",
            false,
        )?;
        let (dest_lock, creating_marker) =
            self.prepare_machine_creation(dest, &dest_dir, events)?;

        let spec_started = Instant::now();
        events.emit(Event::StepStart {
            id: StepId::from("spec"),
            label: format!("copying spec to {dest}"),
        })?;
        atomic::write(&self.paths.machine_spec(dest)?, &spec_document)?;
        let mut state = self.created_state(dest, &loaded.spec)?;
        state.image = live.state.image.clone();
        StateStore::new(self.paths.machine_state(dest)?)
            .write_from_locked_action(&state, &dest_lock)?;
        events.emit(Event::StepDone {
            id: StepId::from("spec"),
            detail: None,
            elapsed_ms: elapsed_millis(spec_started.elapsed()),
        })?;

        let disk_bytes = self.clone_disk(
            source,
            dest,
            fresh_disk,
            &state,
            &loaded.spec,
            &dest_lock,
            events,
        )?;

        fs::remove_file(&creating_marker).map_err(|source_error| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot publish machine `{dest}`"),
                "check the machine directory permissions",
                source_error,
            )
        })?;
        emit_result(
            events,
            "clone",
            &CloneResult {
                source: source.to_owned(),
                dest: dest.to_owned(),
                disk_bytes,
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn clone_disk(
        &self,
        source: &str,
        dest: &str,
        fresh_disk: bool,
        state: &MachineState,
        spec: &MachineSpec,
        dest_lock: &MachineLock,
        events: &mut dyn EventSink,
    ) -> Result<u64, FirestoneError> {
        let started = Instant::now();
        events.emit(Event::StepStart {
            id: StepId::from("disk"),
            label: format!("preparing {dest} disk"),
        })?;
        let source_disk = self.paths.machine_disk(source)?;
        let source_disk_ready = owned_file_ready(&source_disk)?;
        let (Some(id), Some(_)) = (&state.image.id, &state.image.sha256) else {
            if source_disk_ready {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("machine `{source}` has a disk but no pinned image identity"),
                )
                .with_hint("repair state.json so image id and sha256 are both present"));
            }
            events.emit(Event::StepSkip {
                id: StepId::from("disk"),
                reason: "no disk yet".to_owned(),
            })?;
            return Ok(0);
        };
        let store = self.image_store()?;
        if fresh_disk || !source_disk_ready {
            let overlay = store.create_overlay(dest, &state.image, spec.disk, dest_lock)?;
            events.emit(Event::StepDone {
                id: StepId::from("disk"),
                detail: Some("fresh overlay".to_owned()),
                elapsed_ms: elapsed_millis(started.elapsed()),
            })?;
            return Ok(overlay.virtual_size);
        }

        let overlay = store.copy_overlay(
            &source_disk,
            &self.paths.machine_disk_partial(dest)?,
            &self.paths.image_base(id)?,
        )?;
        events.emit(Event::StepDone {
            id: StepId::from("disk"),
            detail: Some("copied overlay".to_owned()),
            elapsed_ms: elapsed_millis(started.elapsed()),
        })?;
        Ok(overlay.virtual_size)
    }

    /// Publishes one immutable snapshot of a machine (SPEC section 23).
    ///
    /// The snapshot is assembled inside `.partial-<snapshot>` and published
    /// with a single rename, so a partial snapshot is never listed, restored,
    /// or counted as an image reference.
    fn snapshot_create(
        &self,
        name: &str,
        snapshot: Option<String>,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let snapshot = match snapshot {
            Some(value) => {
                validate_snapshot_name(&value)?;
                value
            }
            None => auto_snapshot_name(jiff::Timestamp::now()),
        };

        let snapshots_dir = self.paths.machine_snapshots_dir(name)?;
        ensure_snapshot_directory(&snapshots_dir)?;
        let snapshot_lock =
            MachineLock::acquire(name, &self.paths.machine_snapshot_lock(name)?, events)?;

        let published = self.paths.machine_snapshot_dir(name, &snapshot)?;
        if published.exists() {
            return Err(FirestoneError::new(
                ErrorKind::AlreadyExists,
                format!("machine `{name}` already has a snapshot named `{snapshot}`"),
            )
            .with_hint(format!(
                "choose another name or run `firestone snapshot rm {name} {snapshot}`"
            )));
        }

        // A running machine's shim owns the machine lock for its whole
        // lifetime, so the warm path deliberately takes no machine lock: it
        // writes only inside `snapshots/`, which nothing else touches, and the
        // snapshot lock above serializes it against other snapshot work.
        let preliminary = self.read_live_state(name)?;
        let kind = snapshot_kind_for(name, preliminary.state.status)?;
        let machine_lock = if kind == SnapshotKind::Cold {
            let lock = MachineLock::acquire(name, &self.paths.machine_lock(name)?, events)?;
            ensure_machine_exists(&self.paths, name, &machine_dir)?;
            Some(lock)
        } else {
            None
        };
        let live = match &machine_lock {
            Some(lock) => self.read_live_state_locked(name, lock)?,
            None => preliminary,
        };
        if snapshot_kind_for(name, live.state.status)? != kind {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` changed lifecycle state during snapshot"),
            )
            .with_hint(format!("retry `firestone snapshot create {name}`")));
        }
        let spec = self.load_machine_spec(name, &live.state)?;

        let partial = self.paths.machine_snapshot_partial_dir(name, &snapshot)?;
        self.clear_snapshot_directory(&partial)?;
        let outcome =
            self.build_snapshot(name, &snapshot, kind, &partial, &live.state, &spec, events);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = self.clear_snapshot_directory(&partial);
                return Err(error);
            }
        };

        fs::rename(&partial, &published).map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot publish snapshot `{snapshot}` of machine `{name}`"),
                "check the machine snapshots directory permissions",
                source,
            )
        })?;
        sync_directory(&snapshots_dir)?;
        drop(machine_lock);
        drop(snapshot_lock);
        emit_result(
            events,
            "snapshot-create",
            &SnapshotResult {
                name: name.to_owned(),
                snapshot,
                kind,
                disk_bytes: outcome.disk_bytes,
                memory_bytes: outcome.memory_bytes,
            },
        )
    }

    /// Fills one partial snapshot directory with everything it must carry.
    #[allow(clippy::too_many_arguments)]
    fn build_snapshot(
        &self,
        name: &str,
        snapshot: &str,
        kind: SnapshotKind,
        partial: &Path,
        state: &MachineState,
        spec: &MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<SnapshotOutcome, FirestoneError> {
        create_snapshot_directory(partial)?;
        let started = Instant::now();
        events.emit(Event::StepStart {
            id: StepId::from("snapshot"),
            label: format!("capturing {kind} snapshot {snapshot} of {name}"),
        })?;

        let spec_document = read_file(
            &self.paths.machine_spec(name)?,
            "machine spec",
            ErrorKind::NotFound,
        )?;
        atomic::write_with_mode(
            &Paths::snapshot_spec(partial),
            &spec_document,
            SNAPSHOT_FILE_MODE,
        )?;
        let vmconfig_path = self.paths.machine_vmconfig(name)?;
        if owned_file_ready(&vmconfig_path)? {
            let vmconfig = read_file(&vmconfig_path, "machine VmConfig", ErrorKind::NotFound)?;
            atomic::write_with_mode(
                &Paths::snapshot_vmconfig(partial),
                &vmconfig,
                SNAPSHOT_FILE_MODE,
            )?;
        } else if kind == SnapshotKind::Warm {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("running machine `{name}` has no published VmConfig"),
            )
            .with_hint(format!("restart machine {name} and retry the snapshot")));
        }

        let outcome = match kind {
            SnapshotKind::Cold => self.capture_cold_snapshot(name, partial, state)?,
            SnapshotKind::Warm => self.capture_warm_snapshot(name, partial, spec, events)?,
        };

        atomic::write_json_with_mode(
            &Paths::snapshot_metadata(partial),
            &SnapshotMetadata {
                schema_version: SNAPSHOT_SCHEMA_VERSION,
                kind,
                created_at: jiff::Timestamp::now().to_string(),
                image_id: state.image.id.clone(),
                firestone_version: env!("CARGO_PKG_VERSION").to_owned(),
                disk_bytes: outcome.disk_bytes,
                memory_bytes: outcome.memory_bytes,
            },
            SNAPSHOT_FILE_MODE,
        )?;
        sync_directory(partial)?;
        events.emit(Event::StepDone {
            id: StepId::from("snapshot"),
            detail: Some(format!("{kind} snapshot {snapshot}")),
            elapsed_ms: elapsed_millis(started.elapsed()),
        })?;
        Ok(outcome)
    }

    /// Copies a stopped machine's overlay onto the same base image.
    fn capture_cold_snapshot(
        &self,
        name: &str,
        partial: &Path,
        state: &MachineState,
    ) -> Result<SnapshotOutcome, FirestoneError> {
        let disk = self.paths.machine_disk(name)?;
        if !owned_file_ready(&disk)? {
            return Ok(SnapshotOutcome {
                disk_bytes: 0,
                memory_bytes: None,
            });
        }
        let (Some(id), Some(_)) = (&state.image.id, &state.image.sha256) else {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has a disk but no pinned image identity"),
            )
            .with_hint("repair state.json so image id and sha256 are both present"));
        };
        let overlay = self.image_store()?.copy_overlay(
            &disk,
            &Paths::snapshot_disk_partial(partial),
            &self.paths.image_base(id)?,
        )?;
        Ok(SnapshotOutcome {
            disk_bytes: overlay.virtual_size,
            memory_bytes: None,
        })
    }

    /// Pauses a running machine, captures VM state and the overlay, resumes it.
    ///
    /// Resume is attempted on every failure after the pause. A machine that
    /// cannot be resumed is marked degraded rather than silently left paused.
    fn capture_warm_snapshot(
        &self,
        name: &str,
        partial: &Path,
        spec: &MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<SnapshotOutcome, FirestoneError> {
        let disk = self.paths.machine_disk(name)?;
        if !owned_file_ready(&disk)? {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("running machine `{name}` has no overlay to snapshot"),
            )
            .with_hint("restart the machine and retry the snapshot"));
        }
        let memory_bytes = spec.memory.as_bytes();
        self.check_warm_snapshot_space(name, partial, &disk, memory_bytes)?;

        let vmstate = Paths::snapshot_vmstate_dir(partial);
        // Cloud Hypervisor v53 refuses `vm.snapshot` with HTTP 500 and
        // "Destination is not a directory" unless the destination already
        // exists, so Firestone pre-creates it.
        create_snapshot_directory(&vmstate)?;
        let destination = snapshot_file_url(&vmstate)?;
        let api_socket = self.paths.machine_api_socket(name)?;
        let api = VmmApi::new(&api_socket, ShimTimeouts::default().api);

        api.vm_pause()?;
        let captured = (|| -> Result<u64, FirestoneError> {
            api.vm_snapshot(&destination)?;
            sparse_copy_file(&disk, &Paths::snapshot_disk(partial), SNAPSHOT_FILE_MODE)?;
            overlay_virtual_size(&self.qemu_img_program()?, &Paths::snapshot_disk(partial))?
                .ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        format!("snapshot overlay copy for machine `{name}` disappeared"),
                    )
                    .with_hint("retry the snapshot")
                })
        })();
        let resumed = api.vm_resume();
        match (captured, resumed) {
            (Ok(disk_bytes), Ok(())) => Ok(SnapshotOutcome {
                disk_bytes,
                memory_bytes: Some(memory_bytes),
            }),
            (captured, Err(resume_error)) => {
                self.mark_resume_degraded(name, events)?;
                let message = match captured {
                    Ok(_) => format!(
                        "machine `{name}` stayed paused after its snapshot: {}",
                        resume_error.message()
                    ),
                    Err(error) => format!(
                        "machine `{name}` snapshot failed ({}) and it stayed paused: {}",
                        error.message(),
                        resume_error.message()
                    ),
                };
                Err(FirestoneError::new(ErrorKind::Conflict, message)
                    .with_hint(format!(
                        "the machine is degraded and its vCPUs are stopped; run `firestone restart {name}`"
                    ))
                    .with_source(resume_error))
            }
            (Err(error), Ok(())) => Err(error),
        }
    }

    /// Refuses a warm snapshot that cannot fit guest memory plus the overlay.
    fn check_warm_snapshot_space(
        &self,
        name: &str,
        partial: &Path,
        disk: &Path,
        memory_bytes: u64,
    ) -> Result<(), FirestoneError> {
        let required = memory_bytes.saturating_add(allocated_bytes(disk)?);
        let available = available_bytes(partial)?;
        if available >= required {
            return Ok(());
        }
        Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "a warm snapshot of machine `{name}` needs about {required} bytes but only {available} bytes are free"
            ),
        )
        .with_hint("free disk space, or take a cold snapshot after stopping the machine"))
    }

    /// Records that a machine's vCPUs stayed paused after a failed resume.
    fn mark_resume_degraded(
        &self,
        name: &str,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let store = StateStore::new(self.paths.machine_state(name)?);
        let mut state = store.read()?;
        let marker = "vmm paused after a failed snapshot resume".to_owned();
        if !state.degraded.contains(&marker) {
            state.degraded.push(marker);
        }
        store.write_from_shim(&state)?;
        events.emit(Event::Log {
            level: Level::Error,
            message: format!("machine `{name}` could not be resumed and is degraded"),
        })
    }

    /// Lists every published snapshot of one machine, ordered by identifier.
    fn snapshot_list(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let mut snapshots = Vec::new();
        for snapshot in self.published_snapshot_names(name)? {
            let directory = self.paths.machine_snapshot_dir(name, &snapshot)?;
            let metadata = read_snapshot_metadata(&Paths::snapshot_metadata(&directory))?;
            snapshots.push(SnapshotSummary {
                snapshot,
                kind: metadata.kind,
                created_at: metadata.created_at,
                image_id: metadata.image_id,
                disk_bytes: metadata.disk_bytes,
                memory_bytes: metadata.memory_bytes,
            });
        }
        emit_result(events, "snapshot-list", &SnapshotListResult { snapshots })
    }

    /// Rolls one machine back to a snapshot (SPEC section 23).
    fn snapshot_restore(
        &self,
        name: &str,
        snapshot: &str,
        force: bool,
        start: bool,
        timeout: Duration,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        validate_snapshot_name(snapshot)?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let snapshots_dir = self.paths.machine_snapshots_dir(name)?;
        ensure_snapshot_directory(&snapshots_dir)?;
        let snapshot_lock =
            MachineLock::acquire(name, &self.paths.machine_snapshot_lock(name)?, events)?;
        let directory = self.paths.machine_snapshot_dir(name, snapshot)?;
        let metadata = self.read_published_snapshot(name, snapshot, &directory)?;

        // A running machine holds its own machine lock through its shim, so the
        // stop must happen before this action takes it.
        if self.read_live_state(name)?.state.status.is_active() {
            if !force {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("machine `{name}` is running and cannot be restored"),
                )
                .with_hint(format!(
                    "stop {name} first, or retry `firestone snapshot restore {name} {snapshot} --force`"
                )));
            }
            self.stop_machine(name, timeout, false, events)?;
        }

        let machine_lock = MachineLock::acquire(name, &self.paths.machine_lock(name)?, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let live = self.read_live_state_locked(name, &machine_lock)?;
        if live.state.status.is_active() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` became active during restore"),
            )
            .with_hint(format!("stop {name} and retry the restore")));
        }

        let started = Instant::now();
        events.emit(Event::StepStart {
            id: StepId::from("restore"),
            label: format!("restoring {name} from {snapshot}"),
        })?;
        // Any earlier pending restore is replaced: exactly one marker can be
        // waiting for the next launch.
        remove_candidate(&self.paths.machine_restore_request(name)?)?;
        self.restore_machine_disk(name, &directory)?;
        atomic::write_with_mode(
            &self.paths.machine_spec(name)?,
            &read_file(
                &Paths::snapshot_spec(&directory),
                "snapshot spec",
                ErrorKind::NotFound,
            )?,
            MACHINE_SPEC_FILE_MODE,
        )?;
        let vmconfig = Paths::snapshot_vmconfig(&directory);
        let vmconfig_bytes = if owned_file_ready(&vmconfig)? {
            let bytes = read_file(&vmconfig, "snapshot VmConfig", ErrorKind::NotFound)?;
            atomic::write(&self.paths.machine_vmconfig(name)?, &bytes)?;
            Some(bytes)
        } else {
            remove_candidate(&self.paths.machine_vmconfig(name)?)?;
            None
        };

        let mut state = live.state;
        if state.status == MachineStatus::Failed {
            state.status = MachineStatus::Stopped;
        }
        state.degraded.clear();
        StateStore::new(self.paths.machine_state(name)?)
            .write_from_locked_action(&state, &machine_lock)?;

        let warm = metadata.kind == SnapshotKind::Warm;
        if warm {
            let bytes = vmconfig_bytes.ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("warm snapshot `{snapshot}` of machine `{name}` has no VmConfig"),
                )
                .with_hint("remove the damaged snapshot and take a new one")
            })?;
            atomic::write_json_with_mode(
                &self.paths.machine_restore_request(name)?,
                &RestoreRequest {
                    schema_version: SNAPSHOT_SCHEMA_VERSION,
                    snapshot: snapshot.to_owned(),
                    snapshot_dir: directory.clone(),
                    vmstate_dir: Paths::snapshot_vmstate_dir(&directory),
                    vmconfig_sha256: sha256_hex(&bytes),
                    created_at: jiff::Timestamp::now().to_string(),
                },
                SNAPSHOT_FILE_MODE,
            )?;
        }
        sync_directory(&machine_dir)?;
        events.emit(Event::StepDone {
            id: StepId::from("restore"),
            detail: Some(format!("{} snapshot {snapshot}", metadata.kind)),
            elapsed_ms: elapsed_millis(started.elapsed()),
        })?;
        drop(machine_lock);
        drop(snapshot_lock);

        // A warm snapshot only means something resumed: its restore always
        // starts the machine, and the shim consumes the marker on that launch.
        if warm && !start {
            events.emit(Event::Log {
                level: Level::Warn,
                message: format!(
                    "warm snapshot `{snapshot}` restores by resuming machine `{name}`; starting it"
                ),
            })?;
        }
        let started_machine = if warm || start {
            self.start(name, false, self.global.start.timeout.get(), events)?;
            true
        } else {
            false
        };
        emit_result(
            events,
            "snapshot-restore",
            &SnapshotRestoreResult {
                name: name.to_owned(),
                snapshot: snapshot.to_owned(),
                started: started_machine,
            },
        )
    }

    /// Copies one snapshot's overlay back over the machine's live overlay.
    fn restore_machine_disk(&self, name: &str, directory: &Path) -> Result<(), FirestoneError> {
        let disk = self.paths.machine_disk(name)?;
        let source = Paths::snapshot_disk(directory);
        if !owned_file_ready(&source)? {
            // The snapshot was taken before the machine ever had an overlay, so
            // the rollback removes the one it grew since.
            return remove_candidate(&disk);
        }
        let partial = self.paths.machine_disk_partial(name)?;
        remove_candidate(&partial)?;
        sparse_copy_file(&source, &partial, 0o600)?;
        fs::rename(&partial, &disk).map_err(|error| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot restore the overlay of machine `{name}`"),
                "check the machine directory permissions",
                error,
            )
        })
    }

    /// Deletes one published snapshot.
    fn snapshot_remove(
        &self,
        name: &str,
        snapshot: &str,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        validate_snapshot_name(snapshot)?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let snapshots_dir = self.paths.machine_snapshots_dir(name)?;
        ensure_snapshot_directory(&snapshots_dir)?;
        let snapshot_lock =
            MachineLock::acquire(name, &self.paths.machine_snapshot_lock(name)?, events)?;
        let directory = self.paths.machine_snapshot_dir(name, snapshot)?;
        let _ = self.read_published_snapshot(name, snapshot, &directory)?;

        let marker = self.paths.machine_restore_request(name)?;
        if owned_file_ready(&marker)?
            && firestone_core::read_restore_request(&marker)?.snapshot == snapshot
        {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has a pending restore of snapshot `{snapshot}`"),
            )
            .with_hint(format!(
                "start {name} to finish the restore, or restore another snapshot first"
            )));
        }

        validate_removal_tree(&self.paths, &directory, "snapshot directory")?;
        let tombstone = self.paths.machine_snapshot_removal_dir(name, snapshot)?;
        self.clear_snapshot_directory(&tombstone)?;
        fs::rename(&directory, &tombstone).map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot unpublish snapshot `{snapshot}` of machine `{name}`"),
                "check the machine snapshots directory permissions",
                source,
            )
        })?;
        sync_directory(&snapshots_dir)?;
        self.clear_snapshot_directory(&tombstone)?;
        drop(snapshot_lock);
        emit_result(
            events,
            "snapshot-rm",
            &SnapshotRemoveResult {
                name: name.to_owned(),
                snapshot: snapshot.to_owned(),
            },
        )
    }

    /// Reads one published snapshot's metadata, or explains that it is absent.
    fn read_published_snapshot(
        &self,
        name: &str,
        snapshot: &str,
        directory: &Path,
    ) -> Result<SnapshotMetadata, FirestoneError> {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                read_snapshot_metadata(&Paths::snapshot_metadata(directory))
            }
            Ok(_) => Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "snapshot path {} is not a snapshot directory",
                    directory.display()
                ),
            )
            .with_hint("move the conflicting path aside and retry")),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(FirestoneError::new(
                    ErrorKind::NotFound,
                    format!("machine `{name}` has no snapshot named `{snapshot}`"),
                )
                .with_hint(format!("run `firestone snapshot list {name}`")))
            }
            Err(source) => Err(filesystem_error(
                ErrorKind::Dependency,
                format!("cannot inspect snapshot {}", directory.display()),
                "check the machine snapshots directory permissions",
                source,
            )),
        }
    }

    /// Sorted identifiers of every published snapshot of one machine.
    ///
    /// Names beginning with a dot are Firestone's own partial, removal, and
    /// lock entries; a snapshot identifier can never start with one.
    fn published_snapshot_names(&self, name: &str) -> Result<Vec<String>, FirestoneError> {
        let snapshots_dir = self.paths.machine_snapshots_dir(name)?;
        let entries = match fs::read_dir(&snapshots_dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot read snapshots of machine `{name}`"),
                    "check the machine directory permissions",
                    source,
                ));
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| {
                filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot read snapshots of machine `{name}`"),
                    "check the machine directory permissions",
                    source,
                )
            })?;
            let Some(snapshot) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if snapshot.starts_with('.') {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|source| {
                filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot inspect snapshot {}", entry.path().display()),
                    "check the machine snapshots directory permissions",
                    source,
                )
            })?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                names.push(snapshot);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Removes one owned snapshot working directory if it exists.
    fn clear_snapshot_directory(&self, directory: &Path) -> Result<(), FirestoneError> {
        match fs::symlink_metadata(directory) {
            Ok(_) => {
                validate_removal_tree(&self.paths, directory, "snapshot working directory")?;
                fs::remove_dir_all(directory).map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot clear snapshot directory {}", directory.display()),
                        "remove the owned directory and retry",
                        source,
                    )
                })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(filesystem_error(
                ErrorKind::Dependency,
                format!("cannot inspect snapshot directory {}", directory.display()),
                "check the machine snapshots directory permissions",
                source,
            )),
        }
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
                forwards_pending: forwards_pending(&spec, state),
            });
        }
        emit_result(events, "list", &machines)
    }

    fn show(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let (spec, live) = self.load_machine(name)?;
        let pending = forwards_pending(&spec, &live.state);
        let image = image_pending(&self.catalog, &spec, &live.state);
        emit_result(
            events,
            "show",
            &MachineView {
                spec,
                state: live.state,
                supervision: live.supervision,
                forwards_pending: pending,
                image_pending: image,
            },
        )
    }

    fn metrics(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let (spec, live) = self.load_machine(name)?;
        if !live.state.status.is_active() {
            return Err(machine_not_running(name));
        }
        let sampled_at = jiff::Timestamp::now().to_string();
        let process = live
            .state
            .vmm_pid
            .map(sample_vmm_process)
            .unwrap_or_default();
        let api_socket = self.paths.machine_api_socket(name)?;
        let api = VmmApi::new(&api_socket, ShimTimeouts::default().api);
        let counters = api.vm_counters()?;
        let info = api.vm_info()?;
        let (block, net) = project_device_counters(&counters);
        emit_result(
            events,
            "metrics",
            &MetricsResult {
                sampled_at,
                cpu: MetricsCpu {
                    vcpus: spec.cpus,
                    cpu_time_ns: process.cpu_time_ns,
                },
                memory: MetricsMemory {
                    rss_bytes: process.rss_bytes,
                    allocated_bytes: spec.memory.as_bytes(),
                    guest_actual_bytes: (info.memory_actual_size != 0)
                        .then_some(info.memory_actual_size),
                },
                block,
                net,
            },
        )
    }

    fn set_spec(
        &self,
        name: &str,
        mut spec: MachineSpec,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let lock_path = self.paths.machine_lock(name)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let observed_state = self.read_live_state_locked(name, &lock)?;
        let pinned_image_ref = pinned_image_reference(&observed_state.state);
        let warnings = self.validate_action_spec(&mut spec, pinned_image_ref)?;
        self.refuse_disk_shrink(name, &spec)?;
        let previous = self.previous_spec_when_running(name, &observed_state.state)?;
        let document = render_spec(&spec)?;
        atomic::write_with_mode(
            &self.paths.machine_spec(name)?,
            document.as_bytes(),
            MACHINE_SPEC_FILE_MODE,
        )?;
        emit_running_spec_warning(&observed_state.state, events)?;
        emit_forward_restart_warning(&spec, &observed_state.state, events)?;
        emit_credential_change_warning(previous.as_ref(), &spec, events)?;
        emit_spec_warnings(&warnings, events)?;
        emit_result(events, "edit", &SpecResult { spec, warnings })
    }

    fn patch_spec(
        &self,
        name: &str,
        patch: &MachineSpecPatch,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let lock_path = self.paths.machine_lock(name)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let observed_state = self.read_live_state_locked(name, &lock)?;
        let source = read_file(
            &self.paths.machine_spec(name)?,
            "machine spec",
            ErrorKind::NotFound,
        )?;
        let text = std::str::from_utf8(&source).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine spec for {name:?} is not UTF-8"),
            )
            .with_hint("save firestone.toml as UTF-8 TOML")
            .with_source(source)
        })?;
        let loaded = self.load_spec_text_with_patch(
            text,
            &machine_dir,
            &self.source_base,
            pinned_image_reference(&observed_state.state),
            patch,
        )?;
        let warnings = loaded
            .warnings
            .iter()
            .map(SpecWarningPayload::from)
            .collect::<Vec<_>>();
        self.refuse_disk_shrink(name, &loaded.spec)?;
        let previous = self.previous_spec_when_running(name, &observed_state.state)?;
        let document = render_spec(&loaded.spec)?;
        atomic::write_with_mode(
            &self.paths.machine_spec(name)?,
            document.as_bytes(),
            MACHINE_SPEC_FILE_MODE,
        )?;
        emit_running_spec_warning(&observed_state.state, events)?;
        emit_forward_restart_warning(&loaded.spec, &observed_state.state, events)?;
        emit_credential_change_warning(previous.as_ref(), &loaded.spec, events)?;
        emit_spec_warnings(&warnings, events)?;
        emit_result(
            events,
            "edit",
            &SpecResult {
                spec: loaded.spec,
                warnings,
            },
        )
    }

    /// Refuses a `disk` value below the virtual size of an existing overlay.
    ///
    /// qcow2 shrink truncates the guest filesystem. A machine that has never
    /// been started owns no overlay yet, so any `disk` remains free to change.
    fn refuse_disk_shrink(&self, name: &str, spec: &MachineSpec) -> Result<(), FirestoneError> {
        let overlay = self.paths.machine_disk(name)?;
        let exists = overlay.try_exists().map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect machine overlay {}", overlay.display()),
                "check the machine directory permissions",
                source,
            )
        })?;
        if !exists {
            return Ok(());
        }
        let Some(current) = overlay_virtual_size(&self.qemu_img_program()?, &overlay)? else {
            return Ok(());
        };
        if spec.disk.as_bytes() < current {
            return Err(disk_shrink_error(name, current, spec.disk.as_bytes()));
        }
        Ok(())
    }

    /// Loads the spec a running machine actually booted with, for change
    /// comparison. Stopped machines need no comparison and pay no cost.
    fn previous_spec_when_running(
        &self,
        name: &str,
        state: &MachineState,
    ) -> Result<Option<MachineSpec>, FirestoneError> {
        if state.status != MachineStatus::Running {
            return Ok(None);
        }
        self.load_machine_spec(name, state).map(Some)
    }

    /// Resizes one machine's CPU and memory, live when the VMM has headroom.
    fn resize(
        &self,
        name: &str,
        cpus: Option<u8>,
        memory: Option<ByteSize>,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        if cpus.is_none() && memory.is_none() {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                format!("resize of machine `{name}` requested no change"),
            )
            .with_hint("pass --cpus N, --memory SIZE, or both"));
        }
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;

        // A running machine's shim owns the machine lock for its whole
        // lifetime, so the live path deliberately takes no lock: it changes
        // only the VMM's live sizing and the spec file, which the shim never
        // writes. Every other status takes the lock like any spec change.
        let preliminary = self.read_live_state(name)?;
        let status = preliminary.state.status;
        if matches!(status, MachineStatus::Starting | MachineStatus::Stopping) {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` is {status:?} and cannot be resized"),
            )
            .with_hint(format!(
                "wait for machine {name:?} to settle and retry resize"
            )));
        }
        let running = status == MachineStatus::Running;
        let lock = if running {
            None
        } else {
            let lock_path = self.paths.machine_lock(name)?;
            let lock = MachineLock::acquire(name, &lock_path, events)?;
            ensure_machine_exists(&self.paths, name, &machine_dir)?;
            Some(lock)
        };
        let observed_state = match &lock {
            Some(lock) => self.read_live_state_locked(name, lock)?,
            None => preliminary,
        };
        if (observed_state.state.status == MachineStatus::Running) != running {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` changed lifecycle state during resize"),
            )
            .with_hint(format!("retry `firestone resize {name}`")));
        }

        let source = read_file(
            &self.paths.machine_spec(name)?,
            "machine spec",
            ErrorKind::NotFound,
        )?;
        let text = std::str::from_utf8(&source).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine spec for {name:?} is not UTF-8"),
            )
            .with_hint("save firestone.toml as UTF-8 TOML")
            .with_source(source)
        })?;
        let patch = MachineSpecPatch {
            cpus,
            memory,
            ..MachineSpecPatch::default()
        };
        let loaded = self.load_spec_text_with_patch(
            text,
            &machine_dir,
            &self.source_base,
            pinned_image_reference(&observed_state.state),
            &patch,
        )?;
        let warnings = loaded
            .warnings
            .iter()
            .map(SpecWarningPayload::from)
            .collect::<Vec<_>>();

        self.refuse_disk_shrink(name, &loaded.spec)?;

        let applied_live = if running {
            let capacity = published_vmconfig_capacity(name, &self.read_published_vmconfig(name)?)?;
            capacity.check(name, loaded.spec.cpus, loaded.spec.memory)?;
            VmmApi::new(
                &self.paths.machine_api_socket(name)?,
                ShimTimeouts::default().api,
            )
            .vm_resize(cpus, memory.map(ByteSize::as_bytes))?;
            true
        } else {
            false
        };

        let document = render_spec(&loaded.spec)?;
        atomic::write(&self.paths.machine_spec(name)?, document.as_bytes())?;
        drop(lock);
        emit_spec_warnings(&warnings, events)?;
        emit_result(
            events,
            "resize",
            &ResizeResult {
                name: name.to_owned(),
                applied_live,
                cpus: loaded.spec.cpus,
                memory: loaded.spec.memory,
            },
        )
    }

    /// Reads and validates the canonical VmConfig published by the last start.
    fn read_published_vmconfig(&self, name: &str) -> Result<serde_json::Value, FirestoneError> {
        let path = self.paths.machine_vmconfig(name)?;
        let mut file = open_owned_data_file(
            &self.paths,
            &path,
            "machine VmConfig",
            ErrorKind::NotFound,
            Some(format!(
                "start machine {name:?} before requesting --vmconfig"
            )),
        )?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_VMCONFIG_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| {
                filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot read machine VmConfig {}", path.display()),
                    "check the machine directory permissions",
                    source,
                )
            })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_VMCONFIG_BYTES {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "machine VmConfig {} exceeds the {MAX_VMCONFIG_BYTES} byte limit",
                    path.display()
                ),
            )
            .with_hint(format!(
                "restart machine {name:?} to republish canonical VmConfig"
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!("persisted VmConfig {} is invalid JSON", path.display()),
            )
            .with_hint(format!(
                "restart machine {name:?} to republish canonical VmConfig"
            ))
            .with_source(source)
        })?;
        let canonical = serde_json::to_vec(&value).map_err(|source| {
            FirestoneError::new(ErrorKind::Generic, "cannot encode persisted VmConfig")
                .with_source(source)
        })?;
        if canonical != bytes {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("persisted VmConfig {} is not canonical", path.display()),
            )
            .with_hint(format!(
                "restart machine {name:?} to republish canonical VmConfig"
            )));
        }
        Ok(value)
    }

    fn validate_action_spec(
        &self,
        spec: &mut MachineSpec,
        pinned_image_ref: Option<&str>,
    ) -> Result<Vec<SpecWarningPayload>, FirestoneError> {
        let host = RealValidationHost::new();
        let context = ValidationContext::new(&host, &self.paths, &self.source_base, &self.catalog);
        let context = match pinned_image_ref {
            Some(reference) => context.with_pinned_image_ref(reference),
            None => context,
        };
        Ok(validate_machine_spec(spec, &context)?
            .iter()
            .map(SpecWarningPayload::from)
            .collect())
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
        self.load_spec_text_with_patch(
            text,
            machine_dir,
            patch_base_dir,
            pinned_image_ref,
            &MachineSpecPatch::default(),
        )
    }

    fn load_spec_text_with_patch(
        &self,
        text: &str,
        machine_dir: &Path,
        patch_base_dir: &Path,
        pinned_image_ref: Option<&str>,
        patch: &MachineSpecPatch,
    ) -> Result<firestone_core::LoadedMachineSpec, FirestoneError> {
        let host = RealValidationHost::new();
        let context = ValidationContext::new(&host, &self.paths, machine_dir, &self.catalog);
        let context = match pinned_image_ref {
            Some(reference) => context.with_pinned_image_ref(reference),
            None => context,
        };
        MachineSpec::load(text, &self.global, patch, patch_base_dir, &context)
    }
    fn doctor(
        &self,
        fix: bool,
        elevation_confirmed: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let hostname = env::var("HOSTNAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_owned());
        let manifest = DependencyManifest::bundled()?;
        let extracted_passt = if fix {
            let _ = firestone_core::materialize_embedded_helper(
                &self.paths,
                InternalHelper::CloudHypervisor,
            )?;

            let _ =
                firestone_core::materialize_embedded_helper(&self.paths, InternalHelper::QemuImg)?;
            match firestone_core::materialize_embedded_helper(&self.paths, InternalHelper::Passt)? {
                Some(path) => Some(ExtractedPasstHelper::new(
                    path,
                    manifest.embedded_passt("x86_64")?,
                )?),
                None => None,
            }
        } else {
            None
        };
        let mut context = DoctorContext::from_paths(
            self.paths.clone(),
            manifest,
            hostname,
            jiff::Timestamp::now().to_string(),
        );
        if let Some(helper) = extracted_passt {
            context = context.with_extracted_passt(helper);
        }
        let options = if fix {
            DoctorOptions::fix(elevation_confirmed)
        } else {
            DoctorOptions::inspect()
        };
        let report = run_doctor(&context, options)?;
        emit_result(events, "doctor", &report)
    }

    fn version(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let architecture = Arch::current().map_err(|message| {
            FirestoneError::new(ErrorKind::Dependency, message)
                .with_hint("run Firestone on an x86_64 or aarch64 host")
        })?;
        let dependencies = DependencyManifest::bundled()?
            .artifacts(architecture.as_str())?
            .into_iter()
            .map(|(name, artifact)| {
                (
                    name,
                    VersionDependency {
                        version: artifact.version,
                        sha256: artifact.sha256,
                    },
                )
            })
            .collect();
        let version = env!("CARGO_PKG_VERSION").to_owned();
        emit_result(
            events,
            "version",
            &VersionResult {
                identity: VersionIdentity {
                    release: format!("v{version}"),
                    git_commit: embedded_git_commit()?,
                },
                version,
                architecture: architecture.to_string(),
                dependencies,
                paths: VersionPaths {
                    config: self.paths.config_dir().display().to_string(),
                    data: self.paths.data_dir().display().to_string(),
                    runtime: self.paths.runtime_dir().display().to_string(),
                },
            },
        )
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
    fn image_store(&self) -> Result<ImageStore, FirestoneError> {
        ImageStore::for_host(
            self.paths.clone(),
            self.catalog.clone(),
            self.qemu_img_program()?,
        )
    }

    /// Resolves the qemu-img this process runs, materializing the embedded
    /// helper on a standalone build. No image store, and therefore no HTTP
    /// client, is constructed.
    fn qemu_img_program(&self) -> Result<PathBuf, FirestoneError> {
        if self.qemu_img == Path::new("qemu-img") {
            Ok(
                firestone_core::materialize_embedded_helper(&self.paths, InternalHelper::QemuImg)?
                    .unwrap_or_else(|| self.qemu_img.clone()),
            )
        } else {
            Ok(self.qemu_img.clone())
        }
    }

    fn shim_program(&self) -> Result<PathBuf, FirestoneError> {
        match &self.shim_program {
            Some(path) => Ok(path.clone()),
            None => env::current_exe().map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    "cannot locate the firestone executable for the machine shim",
                )
                .with_hint("run start from an installed firestone executable")
                .with_source(source)
            }),
        }
    }

    fn load_machine_spec(
        &self,
        name: &str,
        state: &MachineState,
    ) -> Result<MachineSpec, FirestoneError> {
        let machine_dir = self.paths.machine_dir(name)?;
        let source = read_file(
            &self.paths.machine_spec(name)?,
            "machine spec",
            ErrorKind::NotFound,
        )?;
        let text = std::str::from_utf8(&source).map_err(|source| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine spec for {name:?} is not UTF-8"),
            )
            .with_hint("save firestone.toml as UTF-8 TOML")
            .with_source(source)
        })?;
        let pinned_image_ref = if state.image.id.is_some() && state.image.sha256.is_some() {
            Some(state.image.r#ref.as_str())
        } else {
            None
        };
        self.load_spec_text_with_pinned(text, &machine_dir, &machine_dir, pinned_image_ref)
            .map(|loaded| loaded.spec)
    }

    fn start(
        &self,
        name: &str,
        wait: bool,
        timeout: Duration,
        events: &mut dyn EventSink,
    ) -> Result<StartResult, FirestoneError> {
        let started = Instant::now();
        if self.start_cancellation.load(Ordering::Relaxed) {
            return Err(start_cancelled_error(name));
        }
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let preliminary = self.read_live_state(name)?;
        ensure_startable(name, &preliminary.state)?;

        let lock_path = self.paths.machine_lock(name)?;
        self.paths
            .validate_owned_data_file(&lock_path, "machine lock", 0o600, false)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let live = self.read_live_state_locked(name, &lock)?;
        ensure_startable(name, &live.state)?;
        let spec = self.load_machine_spec(name, &live.state)?;

        let image_store = self.image_store()?;
        let manifest = DependencyManifest::bundled()?;
        let first_boot_timeout = if self.automatic_start_timeout {
            self.global.start.timeout_first_boot.get()
        } else {
            timeout
        };
        let timeouts = ShimTimeouts {
            launch_request: timeout,
            launch_overall: timeout,
            first_boot_launch_request: first_boot_timeout,
            first_boot_launch_overall: first_boot_timeout,
            ..ShimTimeouts::default()
        };
        let prepared = prepare_start(
            &self.paths,
            &image_store,
            &manifest,
            name,
            &spec,
            live.state,
            &machine_dir,
            &lock,
            events,
            timeouts,
        )?;
        let effective_forwards = prepared.forwards().to_vec();
        let effective_mounts = prepared.mounts().to_vec();
        let first_boot = prepared.seed_rewritten();
        let effective_timeout = prepared.timeout();
        if self.start_cancellation.load(Ordering::Relaxed) {
            cancel_prepared(&self.paths, prepared, &lock)?;
            return Err(start_cancelled_error(name));
        }
        let deadline = started.checked_add(effective_timeout).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "start timeout is out of range")
        })?;
        let shim_program = self.shim_program()?;
        let status = launch_prepared_cancellable(
            &self.paths,
            &shim_program,
            prepared,
            lock,
            events,
            self.start_cancellation.as_ref(),
        )?;
        if status.status != MachineStatus::Running {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "machine {name:?} did not reach running; shim reported {:?}",
                    status.status
                ),
            )
            .with_hint(format!("inspect firestone logs {name} --source shim")));
        }

        if self.start_cancellation.load(Ordering::Relaxed) {
            ShimClient::new(
                self.paths.machine_shim_socket(name)?,
                Duration::from_secs(2),
            )
            .stop(Duration::ZERO, true, events)?;
            return Err(start_cancelled_error(name));
        }

        if wait && spec.cloud_init.provisioning {
            wait_for_ssh_ready(
                ReadinessOptions {
                    paths: &self.paths,
                    current_executable: &shim_program,
                    name,
                    user: &spec.user,
                    first_boot,
                    started,
                    deadline,
                    cancelled: &self.start_cancellation,
                },
                events,
            )?;
        }
        Ok(StartResult {
            name: name.to_owned(),
            status: MachineStatus::Running,
            elapsed_ms: elapsed_millis(started.elapsed()),
            forwards: effective_forwards,
            mounts: effective_mounts,
        })
    }

    fn stop(
        &self,
        name: &str,
        timeout: Duration,
        force: bool,
        events: &mut dyn EventSink,
    ) -> Result<StopResult, FirestoneError> {
        let started = Instant::now();
        let state = self.stop_machine(name, timeout, force, events)?;
        Ok(StopResult {
            name: name.to_owned(),
            status: state.status,
            elapsed_ms: elapsed_millis(started.elapsed()),
        })
    }

    fn stop_machine(
        &self,
        name: &str,
        timeout: Duration,
        force: bool,
        events: &mut dyn EventSink,
    ) -> Result<MachineState, FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let live = self.read_live_state(name)?;
        if !live.state.status.is_active() {
            events.emit(Event::StepSkip {
                id: "stop".into(),
                reason: "not running".to_owned(),
            })?;
            return Ok(live.state);
        }

        let use_shim_client =
            live.supervision == Some(Supervision::Supervised) || cfg!(not(target_os = "linux"));
        if use_shim_client {
            let client = ShimClient::new(
                self.paths.machine_shim_socket(name)?,
                ShimTimeouts::default().control_io,
            );
            match client.stop(timeout, force, events) {
                Ok(()) => {
                    let state = StateStore::new(self.paths.machine_state(name)?).read()?;
                    if state.status.is_active() {
                        return Err(FirestoneError::new(
                            ErrorKind::Conflict,
                            format!("machine {name:?} remained active after shim stop"),
                        )
                        .with_hint(format!("retry firestone stop {name}")));
                    }
                    return Ok(state);
                }
                Err(error) if error.kind() == ErrorKind::NotRunning => {}
                Err(error) => return Err(error),
            }
        }

        let lock_path = self.paths.machine_lock(name)?;
        self.paths
            .validate_owned_data_file(&lock_path, "machine lock", 0o600, false)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        let current = self.read_live_state_locked(name, &lock)?;
        if !current.state.status.is_active() {
            events.emit(Event::StepSkip {
                id: "stop".into(),
                reason: "not running".to_owned(),
            })?;
            return Ok(current.state);
        }
        stop_unsupervised(
            &self.paths,
            name,
            current.state,
            &lock,
            timeout,
            force,
            events,
        )
    }

    fn restart(
        &self,
        name: &str,
        timeout: Duration,
        events: &mut dyn EventSink,
    ) -> Result<StartResult, FirestoneError> {
        self.stop_machine(name, self.global.stop.timeout.get(), false, events)?;
        self.start(name, true, timeout, events)
    }

    pub fn remove_confirmation_names(
        &self,
        names: &[String],
    ) -> Result<Vec<String>, FirestoneError> {
        validate_remove_names(names)?;
        let mut running = Vec::new();
        for name in names {
            self.validate_machine_storage()?;
            let machine_dir = self.paths.machine_dir(name)?;
            ensure_machine_exists(&self.paths, name, &machine_dir)?;
            if self.read_live_state(name)?.state.status.is_active() {
                running.push(name.clone());
            }
        }
        Ok(running)
    }

    fn remove(
        &self,
        names: &[String],
        force: bool,
        events: &mut dyn EventSink,
    ) -> Result<RemoveResult, FirestoneError> {
        let running = self.remove_confirmation_names(names)?;
        if !force && !running.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "running machine(s) require confirmation: {}",
                    running.join(", ")
                ),
            )
            .with_hint("retry with --force or --yes"));
        }
        for name in names {
            let machine_dir = self.paths.validate_machine_data_directory(name)?;
            self.paths.validate_owned_data_file(
                &self.paths.machine_lock(name)?,
                "machine lock",
                0o600,
                false,
            )?;
            validate_removal_tree(&self.paths, &machine_dir, "machine directory")?;
        }

        if !force {
            // Snapshots live inside the machine directory and go with it, so
            // the only honest thing to do is say so before deleting them.
            for name in names {
                let snapshots = self.published_snapshot_names(name)?;
                if !snapshots.is_empty() {
                    events.emit(Event::Log {
                        level: Level::Warn,
                        message: format!(
                            "machine `{name}` has {} snapshot(s) that are removed with it: {}",
                            snapshots.len(),
                            snapshots.join(", ")
                        ),
                    })?;
                }
            }
        }

        let mut removed = Vec::with_capacity(names.len());
        for name in names {
            let live = self.read_live_state(name)?;
            if live.state.status.is_active() {
                self.stop_machine(name, self.global.stop.timeout.get(), false, events)?;
            }
            self.remove_one(name, events)?;
            removed.push(name.clone());
        }
        Ok(RemoveResult { removed })
    }

    fn remove_one(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let machine_dir = self.paths.machine_dir(name)?;
        let lock_path = self.paths.machine_lock(name)?;
        self.paths
            .validate_owned_data_file(&lock_path, "machine lock", 0o600, false)?;
        let lock = MachineLock::acquire(name, &lock_path, events)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let live = self.read_live_state_locked(name, &lock)?;
        if live.state.status.is_active() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine {name:?} became active during removal"),
            )
            .with_hint(format!("stop {name} and retry")));
        }
        self.paths.validate_machine_data_directory(name)?;
        validate_removal_tree(&self.paths, &machine_dir, "machine directory")?;
        match fs::symlink_metadata(self.paths.runtime_dir()) {
            Ok(_) => self.paths.clear_machine_runtime_dir(name, true)?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Dependency,
                    format!(
                        "cannot inspect runtime directory {}",
                        self.paths.runtime_dir().display()
                    ),
                    "check the Firestone runtime directory permissions",
                    source,
                ));
            }
        }

        let tombstone = self.paths.machine_removal_dir(name)?;
        match fs::symlink_metadata(&tombstone) {
            Ok(_) => {
                validate_removal_tree(&self.paths, &tombstone, "machine removal directory")?;
                fs::remove_dir_all(&tombstone).map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot clear stale removal {}", tombstone.display()),
                        "remove the stale owned directory and retry",
                        source,
                    )
                })?;
                sync_directory(&self.paths.machines_dir())?;
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot inspect removal path {}", tombstone.display()),
                    "check the machines directory permissions",
                    source,
                ));
            }
        }

        fs::rename(&machine_dir, &tombstone).map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot unpublish machine {name:?}"),
                "check the machines directory permissions",
                source,
            )
        })?;
        sync_directory(&self.paths.machines_dir())?;
        drop(lock);
        fs::remove_dir_all(&tombstone).map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot finish removing machine {name:?}"),
                "remove the owned removal directory and retry",
                source,
            )
        })?;
        sync_directory(&self.paths.machines_dir())
    }

    pub fn image_remove_confirmation(&self, id: &str) -> Result<Vec<String>, FirestoneError> {
        self.image_store()?.referencing_machines(id)
    }

    /// Plans the exact scp invocation for one `firestone cp` operand pair (SPEC 11.8).
    ///
    /// The dispatcher owns operand classification, machine lookup, and the running check. Only the
    /// CLI executes the returned argv.
    pub fn cp_plan(
        &self,
        source: &str,
        target: &str,
        recursive: bool,
    ) -> Result<CpResult, FirestoneError> {
        let operands = classify_cp_operands(source, target)?;
        let name = operands.machine();
        let machine = self.terminal_machine(name)?;
        if machine.state.status != MachineStatus::Running {
            return Err(cp_not_running_error(name));
        }
        let user = machine.spec.user;
        let plan = scp_command_plan(
            &self.paths,
            &self.shim_program()?,
            name,
            &user,
            recursive,
            operands.source(),
            operands.target(),
        )?;
        let program = cp_argument(plan.program())?;
        let args = plan
            .args()
            .iter()
            .map(|argument| cp_argument(argument))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CpResult {
            name: name.to_owned(),
            user,
            recursive,
            program,
            args,
        })
    }

    fn catalog_list(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let entries = self
            .catalog
            .entries()
            .map(|entry| CatalogEntrySummary {
                reference: entry.canonical_reference(),
                aliases: entry.aliases.clone(),
                architectures: entry
                    .arch
                    .iter()
                    .map(|(architecture, source)| CatalogArchitectureSummary {
                        architecture: architecture.clone(),
                        firmware: source.firmware.unwrap_or(entry.firmware),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        emit_result(events, "catalog", &entries)
    }

    fn image_list(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        emit_result(events, "images-ls", &self.image_store()?.list()?)
    }

    fn image_pull(
        &self,
        image: firestone_core::ImageRef,
        sha256: Option<String>,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let mut request = ImagePullRequest::new(image, self.source_base.clone());
        request.sha256 = sha256;
        let pulled = self.image_store()?.pull(&request, events)?;
        emit_result(events, "images-pull", &pulled)
    }

    fn image_inspect(&self, id: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        emit_result(events, "images-inspect", &self.image_store()?.inspect(id)?)
    }

    fn image_remove(
        &self,
        id: &str,
        force: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let store = self.image_store()?;
        if !force {
            let referenced_by = store.referencing_machines(id)?;
            if !referenced_by.is_empty() {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!(
                        "image {id:?} is referenced by machine(s): {}",
                        referenced_by.join(", ")
                    ),
                )
                .with_hint("remove the machines first or retry with --force or --yes"));
            }
        }
        emit_result(events, "images-rm", &store.remove(id, force)?)
    }

    fn image_prune(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        emit_result(events, "images-prune", &self.image_store()?.prune()?)
    }

    /// Machines the destructive prune tier would remove, in list order.
    ///
    /// The CLI shows this list before it asks for confirmation, so the names in
    /// the prompt are exactly the names the action will act on (SPEC §26).
    pub fn prune_confirmation_names(&self) -> Result<Vec<String>, FirestoneError> {
        self.validate_machine_storage()?;
        let mut names = Vec::new();
        for name in self.prunable_machine_names()? {
            if prune_removable_status(self.read_live_state(&name)?.state.status) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// Published machines, excluding Firestone's own dot-prefixed entries.
    ///
    /// A `.removing-<name>` tombstone still holds a complete machine directory
    /// and would otherwise read as a machine; prune treats it as the debris it
    /// is (SPEC §26).
    fn prunable_machine_names(&self) -> Result<Vec<String>, FirestoneError> {
        Ok(self
            .machine_names()?
            .into_iter()
            .filter(|name| !name.starts_with('.'))
            .collect())
    }

    /// Reclaims disk space held by Firestone's own artifacts (SPEC §26).
    ///
    /// The whole plan is built before anything is deleted, so `--dry-run`
    /// reports exactly the list a real run acts on. Deletions then happen in
    /// plan order under the same locks every other action takes.
    fn system_prune(
        &self,
        machines: bool,
        images: bool,
        force: bool,
        dry_run: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        // A dry run removes nothing, so it is the one way to see what the
        // destructive tier would do before authorizing it (SPEC §26).
        if machines && !force && !dry_run {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                "pruning machines requires `force`",
            )
            .with_hint(
                "confirm the listed machines interactively, pass --force or --yes on the CLI, or send \"force\": true in the REST body",
            ));
        }
        self.validate_machine_storage()?;

        let names = self.prunable_machine_names()?;
        let mut statuses = BTreeMap::new();
        for name in &names {
            statuses.insert(name.clone(), self.read_live_state(name)?.state.status);
        }

        // Bytes already claimed by an inert item inside a machine directory.
        // The destructive tier subtracts them so one byte is never counted
        // twice and a dry run reports the same total a real run reclaims.
        let mut counted: BTreeMap<String, u64> = BTreeMap::new();
        let mut plan = self.plan_stale_runtime_dirs(&statuses)?;
        let mut logs = Vec::new();
        let mut partials = Vec::new();
        let mut snapshot_partials = Vec::new();
        // Every machine directory is measured before the first deletion, so
        // the destructive tier reports the same size in both modes.
        let mut totals: BTreeMap<String, u64> = BTreeMap::new();
        for name in &names {
            totals.insert(
                name.clone(),
                allocated_tree_bytes(&self.paths.machine_dir(name)?)?,
            );
            let previous = self.paths.machine_console_previous_log(name)?;
            if let Some(bytes) = owned_regular_file_bytes(&previous)? {
                add_counted(&mut counted, name, bytes);
                logs.push(PrunePlanItem {
                    kind: PruneKind::Log,
                    id: format!("{name}/console.log.previous"),
                    bytes,
                    target: PruneTarget::File(previous),
                });
            }
            for (path, entry) in self.machine_partial_entries(name)? {
                add_counted(&mut counted, name, entry.bytes);
                partials.push(PrunePlanItem {
                    kind: PruneKind::Partial,
                    id: format!("machines/{name}/{}", entry.file_name),
                    bytes: entry.bytes,
                    target: if entry.directory {
                        PruneTarget::Tree(path)
                    } else {
                        PruneTarget::File(path)
                    },
                });
            }
            for (path, entry) in self.snapshot_partial_entries(name)? {
                add_counted(&mut counted, name, entry.bytes);
                snapshot_partials.push(PrunePlanItem {
                    kind: PruneKind::SnapshotPartial,
                    id: format!("{name}/snapshots/{}", entry.file_name),
                    bytes: entry.bytes,
                    target: PruneTarget::SnapshotTree {
                        machine: name.clone(),
                        path,
                    },
                });
            }
        }
        partials.extend(self.plan_orphan_removal_dirs()?);
        plan.extend(logs);
        plan.extend(partials);
        plan.extend(snapshot_partials);

        let mut removed = self.execute_prune_plan(&plan, dry_run, events)?;

        // The image store is measured and mutated behind its own lock, so the
        // two tiers below run as whole steps rather than through the plan.
        let store = self.image_store()?;
        for artifact in store.prune_partials(dry_run)? {
            removed.push(emit_prune_item(
                events,
                PruneKind::Partial,
                format!("images/{}", artifact.id),
                artifact.bytes,
                dry_run,
            )?);
        }
        if images {
            for artifact in store.prune_unreferenced(dry_run)? {
                removed.push(emit_prune_item(
                    events,
                    PruneKind::Image,
                    artifact.id,
                    artifact.bytes,
                    dry_run,
                )?);
            }
        }

        if machines {
            for name in &names {
                if !statuses
                    .get(name)
                    .copied()
                    .is_some_and(prune_removable_status)
                {
                    continue;
                }
                let bytes = totals
                    .get(name)
                    .copied()
                    .unwrap_or_default()
                    .saturating_sub(counted.get(name).copied().unwrap_or_default());
                let started = prune_step_start(events, PruneKind::Machine, name)?;
                if !dry_run {
                    self.remove_one(name, events)?;
                }
                removed.push(prune_step_done(
                    events,
                    PruneKind::Machine,
                    name.clone(),
                    bytes,
                    dry_run,
                    started,
                )?);
            }
        }

        let reclaimed_bytes = removed
            .iter()
            .fold(0_u64, |total, item| total.saturating_add(item.bytes));
        emit_result(
            events,
            "system-prune",
            &PruneResult {
                dry_run,
                reclaimed_bytes,
                removed,
            },
        )
    }

    /// Runtime directories of machines that are not starting, running, or stopping.
    ///
    /// A directory whose machine no longer exists is stale by definition. A
    /// directory whose machine exists but is not readable as a complete
    /// machine is left alone: only the machine's own lock can settle that.
    fn plan_stale_runtime_dirs(
        &self,
        statuses: &BTreeMap<String, MachineStatus>,
    ) -> Result<Vec<PrunePlanItem>, FirestoneError> {
        let runtime_dir = self.paths.runtime_dir().to_path_buf();
        let entries = match fs::read_dir(&runtime_dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot read runtime directory {}", runtime_dir.display()),
                    "check the Firestone runtime directory permissions",
                    source,
                ));
            }
        };
        let mut items = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| {
                filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot read runtime directory {}", runtime_dir.display()),
                    "check the Firestone runtime directory permissions",
                    source,
                )
            })?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|source| {
                filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot inspect runtime entry {}", entry.path().display()),
                    "check the Firestone runtime directory permissions",
                    source,
                )
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if self.paths.machine_runtime_dir(&name).is_err() {
                continue;
            }
            let published = match statuses.get(&name) {
                Some(status) => {
                    if status.is_active() {
                        continue;
                    }
                    true
                }
                // An unlisted machine whose directory still exists is either
                // half-created or unreadable; leave its runtime state alone.
                None if machine_directory_exists(&self.paths, &name)? => continue,
                None => false,
            };
            items.push(PrunePlanItem {
                kind: PruneKind::Runtime,
                id: name.clone(),
                bytes: allocated_tree_bytes(&entry.path())?,
                target: PruneTarget::RuntimeDir {
                    machine: name,
                    published,
                },
            });
        }
        items.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(items)
    }

    /// Interrupted `.partial` artifacts directly inside one machine directory.
    fn machine_partial_entries(
        &self,
        name: &str,
    ) -> Result<Vec<(PathBuf, PruneDirEntry)>, FirestoneError> {
        let machine_dir = self.paths.machine_dir(name)?;
        read_prune_entries(&machine_dir, |entry| entry.ends_with(".partial"))
    }

    /// Unfinished snapshot working and removal directories of one machine.
    fn snapshot_partial_entries(
        &self,
        name: &str,
    ) -> Result<Vec<(PathBuf, PruneDirEntry)>, FirestoneError> {
        let snapshots_dir = self.paths.machine_snapshots_dir(name)?;
        read_prune_entries(&snapshots_dir, |entry| {
            entry.starts_with(".partial-") || entry.starts_with(".removing-")
        })
    }

    /// Machine removal tombstones an interrupted `rm` left behind.
    fn plan_orphan_removal_dirs(&self) -> Result<Vec<PrunePlanItem>, FirestoneError> {
        let machines_dir = self.paths.machines_dir();
        let mut items = Vec::new();
        for (path, entry) in
            read_prune_entries(&machines_dir, |entry| entry.starts_with(".removing-"))?
        {
            let Some(machine) = entry.file_name.strip_prefix(".removing-") else {
                continue;
            };
            if machine.is_empty() || self.paths.machine_dir(machine).is_err() {
                continue;
            }
            items.push(PrunePlanItem {
                kind: PruneKind::Partial,
                id: format!("machines/{}", entry.file_name),
                bytes: entry.bytes,
                target: PruneTarget::MachineRemovalDir {
                    machine: machine.to_owned(),
                    path,
                },
            });
        }
        Ok(items)
    }

    /// Applies one prune plan in order, or reports it unchanged for a dry run.
    fn execute_prune_plan(
        &self,
        plan: &[PrunePlanItem],
        dry_run: bool,
        events: &mut dyn EventSink,
    ) -> Result<Vec<PruneItem>, FirestoneError> {
        let mut removed = Vec::with_capacity(plan.len());
        for item in plan {
            let started = prune_step_start(events, item.kind, &item.id)?;
            if !dry_run && !self.apply_prune_item(item, events)? {
                continue;
            }
            removed.push(prune_step_done(
                events,
                item.kind,
                item.id.clone(),
                item.bytes,
                dry_run,
                started,
            )?);
        }
        Ok(removed)
    }

    /// Deletes one planned artifact, reporting false when it is no longer safe.
    fn apply_prune_item(
        &self,
        item: &PrunePlanItem,
        events: &mut dyn EventSink,
    ) -> Result<bool, FirestoneError> {
        match &item.target {
            PruneTarget::RuntimeDir { machine, published } => {
                if *published {
                    let lock =
                        MachineLock::acquire(machine, &self.paths.machine_lock(machine)?, events)?;
                    // The machine may have started between the plan and now.
                    if self
                        .read_live_state_locked(machine, &lock)?
                        .state
                        .status
                        .is_active()
                    {
                        events.emit(Event::StepSkip {
                            id: item.kind.as_str().into(),
                            reason: format!("{machine} is running"),
                        })?;
                        return Ok(false);
                    }
                    self.paths.clear_machine_runtime_dir(machine, true)?;
                    drop(lock);
                } else {
                    self.paths.clear_machine_runtime_dir(machine, true)?;
                }
                Ok(true)
            }
            PruneTarget::File(path) => {
                remove_owned_prune_file(path)?;
                Ok(true)
            }
            PruneTarget::Tree(path) => {
                self.remove_prune_tree(path)?;
                Ok(true)
            }
            PruneTarget::SnapshotTree { machine, path } => {
                let lock = MachineLock::acquire(
                    machine,
                    &self.paths.machine_snapshot_lock(machine)?,
                    events,
                )?;
                let result = self.remove_prune_tree(path);
                drop(lock);
                result?;
                Ok(true)
            }
            PruneTarget::MachineRemovalDir { machine, path } => {
                // The tombstone usually outlives its machine directory, and a
                // lock file cannot be created where no directory remains.
                if machine_directory_exists(&self.paths, machine)? {
                    let lock =
                        MachineLock::acquire(machine, &self.paths.machine_lock(machine)?, events)?;
                    let result = self.remove_prune_tree(path);
                    drop(lock);
                    result?;
                } else {
                    self.remove_prune_tree(path)?;
                }
                Ok(true)
            }
        }
    }

    /// Removes one validated Firestone-owned directory tree.
    fn remove_prune_tree(&self, path: &Path) -> Result<(), FirestoneError> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                validate_removal_tree(&self.paths, path, "prune directory")?;
                fs::remove_dir_all(path).map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot remove {}", path.display()),
                        "remove the owned directory and retry",
                        source,
                    )
                })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(filesystem_error(
                ErrorKind::Dependency,
                format!("cannot inspect {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            )),
        }
    }

    fn show_vmconfig(&self, name: &str, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let value = self.read_published_vmconfig(name)?;
        emit_result(events, "show-vmconfig", &value)
    }

    fn logs(
        &self,
        name: &str,
        source: LogSource,
        lines: u32,
        follow: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        if !follow {
            let cancelled = AtomicBool::new(false);
            return self.logs_until(name, source, lines, false, &cancelled, events);
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let signal =
            signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancelled))
                .map_err(|source| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        "cannot install log-follow interrupt handler",
                    )
                    .with_source(source)
                })?;
        let result = self.logs_until(name, source, lines, true, &cancelled, events);
        signal_hook::low_level::unregister(signal);
        result
    }

    fn logs_until(
        &self,
        name: &str,
        source: LogSource,
        lines: u32,
        follow: bool,
        cancelled: &AtomicBool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        if lines > MAX_LOG_LINES {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                format!("log line count exceeds the {MAX_LOG_LINES} line limit"),
            )
            .with_hint(format!("pass -n {MAX_LOG_LINES} or fewer")));
        }
        self.validate_machine_storage()?;
        let machine_dir = self.paths.machine_dir(name)?;
        ensure_machine_exists(&self.paths, name, &machine_dir)?;
        let path = self.log_path(name, source)?;
        let mut file = open_owned_data_file(
            &self.paths,
            &path,
            "machine log",
            ErrorKind::NotFound,
            Some(format!("start machine {name:?} or choose another --source")),
        )?;
        let (initial, emitted_lines, mut offset) = tail_log(&mut file, lines, &path)?;
        if !initial.is_empty() {
            events.emit(Event::Output {
                data: String::from_utf8_lossy(&initial).into_owned(),
            })?;
        }
        if !follow {
            return emit_result(
                events,
                "logs",
                &LogsResult {
                    name: name.to_owned(),
                    source,
                    lines: emitted_lines,
                    follow: false,
                },
            );
        }

        let mut identity = file.metadata().map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect open log {}", path.display()),
                "check the machine log permissions",
                source,
            )
        })?;
        loop {
            if events.is_cancelled() {
                return Ok(());
            }
            if cancelled.load(Ordering::Relaxed) {
                return Err(FirestoneError::new(
                    ErrorKind::Interrupted,
                    format!("log follow for machine {name:?} was interrupted"),
                ));
            }

            match fs::symlink_metadata(&path) {
                Ok(metadata)
                    if metadata.dev() != identity.dev() || metadata.ino() != identity.ino() =>
                {
                    file = open_owned_data_file(
                        &self.paths,
                        &path,
                        "machine log",
                        ErrorKind::NotFound,
                        Some(format!("start machine {name:?} or choose another --source")),
                    )?;
                    identity = file.metadata().map_err(|source| {
                        filesystem_error(
                            ErrorKind::Generic,
                            format!("cannot inspect reopened log {}", path.display()),
                            "check the machine log permissions",
                            source,
                        )
                    })?;
                    offset = 0;
                }
                Ok(_) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    thread::sleep(LOG_FOLLOW_INTERVAL);
                    continue;
                }
                Err(source) => {
                    return Err(filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot inspect followed log {}", path.display()),
                        "check the machine log permissions",
                        source,
                    ));
                }
            }

            let length = file
                .metadata()
                .map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot inspect followed log {}", path.display()),
                        "check the machine log permissions",
                        source,
                    )
                })?
                .len();
            if length < offset {
                offset = 0;
            }
            if length > offset {
                file.seek(SeekFrom::Start(offset)).map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot seek followed log {}", path.display()),
                        "check the machine log permissions",
                        source,
                    )
                })?;
                let count = (length - offset).min(LOG_FOLLOW_CHUNK_BYTES);
                let capacity = usize::try_from(count).map_err(|_| {
                    FirestoneError::new(ErrorKind::Generic, "log chunk length overflowed usize")
                })?;
                let mut chunk = Vec::with_capacity(capacity);
                file.by_ref()
                    .take(count)
                    .read_to_end(&mut chunk)
                    .map_err(|source| {
                        filesystem_error(
                            ErrorKind::Generic,
                            format!("cannot read followed log {}", path.display()),
                            "check the machine log permissions",
                            source,
                        )
                    })?;
                offset = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                if !chunk.is_empty() {
                    events.emit(Event::Output {
                        data: String::from_utf8_lossy(&chunk).into_owned(),
                    })?;
                }
            }
            thread::sleep(LOG_FOLLOW_INTERVAL);
        }
    }

    fn log_path(&self, name: &str, source: LogSource) -> Result<PathBuf, FirestoneError> {
        match source {
            LogSource::Console => self.paths.machine_console_log(name),
            LogSource::Vmm => self.paths.machine_vmm_log(name),
            LogSource::Shim => self.paths.machine_shim_log(name),
            LogSource::Passt => self.paths.machine_passt_log(name),
            LogSource::Virtiofsd(index) => {
                self.paths.machine_virtiofsd_log(name, usize::from(index))
            }
        }
    }
}

impl Dispatcher for LocalDispatcher {
    fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
        Box::pin(async move {
            match action {
                Action::Create { name, spec } => self.create(&name, spec, events),
                Action::Start {
                    name,
                    wait,
                    timeout,
                } => {
                    let result = self.start(&name, wait, timeout, events)?;
                    emit_result(events, "start", &result)
                }
                Action::Stop {
                    name,
                    timeout,
                    force,
                } => {
                    let result = self.stop(&name, timeout, force, events)?;
                    emit_result(events, "stop", &result)
                }
                Action::Restart { name, timeout } => {
                    let result = self.restart(&name, timeout, events)?;
                    emit_result(events, "restart", &result)
                }
                Action::Remove { names, force } => {
                    let result = self.remove(&names, force, events)?;
                    emit_result(events, "rm", &result)
                }
                Action::List => self.list(events),
                Action::Show { name, vmconfig } => {
                    if vmconfig {
                        self.show_vmconfig(&name, events)
                    } else {
                        self.show(&name, events)
                    }
                }
                Action::SetSpec { name, spec } => self.set_spec(&name, spec, events),
                Action::PatchSpec { name, patch } => self.patch_spec(&name, &patch, events),
                Action::Logs {
                    name,
                    source,
                    lines,
                    follow,
                } => self.logs(&name, source, lines, follow, events),
                Action::Cp {
                    source,
                    target,
                    recursive,
                } => {
                    let result = self.cp_plan(&source, &target, recursive)?;
                    emit_result(events, "cp", &result)
                }
                Action::Metrics { name } => self.metrics(&name, events),
                Action::CatalogList => self.catalog_list(events),
                Action::ImageList => self.image_list(events),
                Action::ImagePull { r#ref, sha256 } => self.image_pull(r#ref, sha256, events),
                Action::ImageRemove { id, force } => self.image_remove(&id, force, events),
                Action::ImageInspect { id } => self.image_inspect(&id, events),
                Action::ImagePrune => self.image_prune(events),
                Action::Doctor {
                    fix,
                    elevation_confirmed,
                } => self.doctor(fix, elevation_confirmed, events),
                Action::Version => self.version(events),
                Action::Clone {
                    source,
                    dest,
                    fresh_disk,
                } => self.clone_machine(&source, &dest, fresh_disk, events),
                Action::Resize { name, cpus, memory } => self.resize(&name, cpus, memory, events),
                Action::SnapshotCreate { name, snapshot } => {
                    self.snapshot_create(&name, snapshot, events)
                }
                Action::SnapshotList { name } => self.snapshot_list(&name, events),
                Action::SnapshotRestore {
                    name,
                    snapshot,
                    force,
                    start,
                    timeout,
                } => self.snapshot_restore(&name, &snapshot, force, start, timeout, events),
                Action::SnapshotRemove { name, snapshot } => {
                    self.snapshot_remove(&name, &snapshot, events)
                }
                Action::SystemPrune {
                    machines,
                    images,
                    force,
                    dry_run,
                } => self.system_prune(machines, images, force, dry_run, events),
            }
        })
    }
}

fn embedded_git_commit() -> Result<Option<String>, FirestoneError> {
    let Some(commit) = option_env!("FIRESTONE_GIT_COMMIT") else {
        return Ok(None);
    };
    let valid = commit.len() == 40
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            "the embedded Firestone git commit is invalid",
        )
        .with_hint("build with FIRESTONE_GIT_COMMIT set to the full lowercase 40-hex revision"));
    }
    Ok(Some(commit.to_owned()))
}

fn pinned_image_reference(state: &MachineState) -> Option<&str> {
    if state.image.id.is_some() && state.image.sha256.is_some() {
        Some(state.image.r#ref.as_str())
    } else {
        None
    }
}

fn start_cancelled_error(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Interrupted,
        format!("start for machine {name} was interrupted before readiness"),
    )
    .with_hint(format!(
        "machine {name} was not left running; retry firestone start {name}"
    ))
}

fn ensure_startable(name: &str, state: &MachineState) -> Result<(), FirestoneError> {
    match state.status {
        MachineStatus::Created | MachineStatus::Stopped | MachineStatus::Failed => Ok(()),
        MachineStatus::Starting | MachineStatus::Running | MachineStatus::Stopping => {
            Err(FirestoneError::new(
                ErrorKind::AlreadyRunning,
                format!("machine {name:?} is already active"),
            )
            .with_hint(format!(
                "use firestone stop {name} before starting it again"
            )))
        }
    }
}

/// Only a machine that owns no running processes can be copied consistently.
/// Byte counts one captured snapshot reports and records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SnapshotOutcome {
    disk_bytes: u64,
    memory_bytes: Option<u64>,
}

/// Maps a machine's lifecycle state onto the snapshot tier it supports.
///
/// A stopped or created machine yields a guaranteed cold snapshot. A running
/// machine yields a warm one. A machine in transition or failed has no
/// coherent disk to copy, so it is refused rather than guessed at.
fn snapshot_kind_for(name: &str, status: MachineStatus) -> Result<SnapshotKind, FirestoneError> {
    match status {
        MachineStatus::Created | MachineStatus::Stopped => Ok(SnapshotKind::Cold),
        MachineStatus::Running => Ok(SnapshotKind::Warm),
        MachineStatus::Starting | MachineStatus::Stopping | MachineStatus::Failed => {
            Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "machine `{name}` is {} and cannot be snapshotted",
                    machine_status_word(status)
                ),
            )
            .with_hint(format!(
                "wait for machine {name} to settle, or stop it and take a cold snapshot"
            )))
        }
    }
}

fn ensure_clonable(name: &str, status: MachineStatus) -> Result<(), FirestoneError> {
    if matches!(status, MachineStatus::Created | MachineStatus::Stopped) {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Conflict,
        format!(
            "machine `{name}` is {} and cannot be cloned",
            machine_status_word(status)
        ),
    )
    .with_hint(format!(
        "run `firestone stop {name}` and clone the stopped machine"
    )))
}

/// Stable lowercase status word used in clone refusal messages.
const fn machine_status_word(status: MachineStatus) -> &'static str {
    match status {
        MachineStatus::Created => "created",
        MachineStatus::Starting => "starting",
        MachineStatus::Running => "running",
        MachineStatus::Stopping => "stopping",
        MachineStatus::Stopped => "stopped",
        MachineStatus::Failed => "failed",
    }
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn validate_remove_names(names: &[String]) -> Result<(), FirestoneError> {
    if names.is_empty() {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "rm requires at least one machine name",
        ));
    }
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                format!("machine {name:?} is repeated in rm arguments"),
            )
            .with_hint("list each machine once"));
        }
    }
    Ok(())
}

fn validate_removal_tree(paths: &Paths, root: &Path, label: &str) -> Result<(), FirestoneError> {
    paths.validate_owned_data_directory(root, label, false)?;
    validate_removal_entry(paths, root)
}

fn validate_removal_entry(paths: &Paths, path: &Path) -> Result<(), FirestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect removal entry {}", path.display()),
            "repair the owned machine directory before removing it",
            source,
        )
    })?;
    if metadata.uid() != paths.uid() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "refusing to remove {} because uid {} does not match Firestone uid {}",
                path.display(),
                metadata.uid(),
                paths.uid()
            ),
        )
        .with_hint("move the unsafe machine directory aside and inspect it manually"));
    }
    if metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("refusing to remove symlink {}", path.display()),
        )
        .with_hint("remove the symlink manually and retry"));
    }
    if metadata.is_dir() {
        if metadata.mode() & 0o7777 != 0o700 {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "refusing to remove directory {} with mode {:04o}; expected 0700",
                    path.display(),
                    metadata.mode() & 0o7777
                ),
            )
            .with_hint("restore the owned directory mode to 0700 and retry"));
        }
        let mut entries = fs::read_dir(path)
            .map_err(|source| {
                filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot read removal directory {}", path.display()),
                    "repair the owned machine directory before removing it",
                    source,
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| {
                filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot read removal directory {}", path.display()),
                    "repair the owned machine directory before removing it",
                    source,
                )
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            validate_removal_entry(paths, &entry.path())?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("refusing to remove special file {}", path.display()),
        )
        .with_hint("remove the special file manually and retry"));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), FirestoneError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot sync directory {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            )
        })
}

fn open_owned_data_file(
    paths: &Paths,
    path: &Path,
    label: &str,
    missing_kind: ErrorKind,
    missing_hint: Option<String>,
) -> Result<File, FirestoneError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| {
            let mut error = FirestoneError::new(
                if source.kind() == std::io::ErrorKind::NotFound {
                    missing_kind
                } else {
                    ErrorKind::Dependency
                },
                format!("cannot open {label} {}", path.display()),
            )
            .with_source(source);
            if let Some(hint) = &missing_hint {
                error = error.with_hint(hint.clone());
            }
            error
        })?;
    paths.validate_owned_data_file_handle(path, label, 0o600, &file)?;
    Ok(file)
}

fn tail_log(
    file: &mut File,
    lines: u32,
    path: &Path,
) -> Result<(Vec<u8>, u32, u64), FirestoneError> {
    let end = file
        .metadata()
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect machine log {}", path.display()),
                "check the machine log permissions",
                source,
            )
        })?
        .len();
    if lines == 0 || end == 0 {
        return Ok((Vec::new(), 0, end));
    }
    let window_start = end.saturating_sub(MAX_LOG_TAIL_BYTES);
    let window_len = end - window_start;
    file.seek(SeekFrom::Start(window_start)).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot seek machine log {}", path.display()),
            "check the machine log permissions",
            source,
        )
    })?;
    let capacity = usize::try_from(window_len)
        .map_err(|_| FirestoneError::new(ErrorKind::Generic, "log tail length overflowed usize"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(window_len)
        .read_to_end(&mut bytes)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot read machine log {}", path.display()),
                "check the machine log permissions",
                source,
            )
        })?;

    let mut cursor = bytes.len();
    if bytes.last() == Some(&b'\n') {
        cursor = cursor.saturating_sub(1);
    }
    let mut separators = 0_u32;
    let mut start = 0_usize;
    while cursor > 0 {
        cursor -= 1;
        if bytes[cursor] == b'\n' {
            separators = separators.saturating_add(1);
            if separators == lines {
                start = cursor + 1;
                break;
            }
        }
    }
    if separators < lines && window_start != 0 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "requested log tail in {} exceeds the {MAX_LOG_TAIL_BYTES} byte scan limit",
                path.display()
            ),
        )
        .with_hint("request fewer lines or inspect the owned log file directly"));
    }
    let selected = bytes.split_off(start);
    let logical_lines = if selected.is_empty() {
        0
    } else {
        let newlines = selected.iter().filter(|byte| **byte == b'\n').count();
        let trailing = usize::from(selected.last() != Some(&b'\n'));
        u32::try_from(newlines.saturating_add(trailing))
            .map_err(|_| FirestoneError::new(ErrorKind::Generic, "log line count overflowed u32"))?
    };
    Ok((selected, logical_lines, end))
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

/// Spec forwards that a running machine has not applied yet (§12.4).
///
/// passt fixes its forwards at spawn, so a difference against the recorded
/// state is pending until the next start. A machine that is not running has
/// nothing applied and is never pending.
fn forwards_pending(spec: &MachineSpec, state: &MachineState) -> bool {
    state.status == MachineStatus::Running
        && forwards_differ(&spec.network.forward, &state.forwards)
}

/// The spec now names an image other than the one a running machine booted
/// (§8.2).
///
/// `state.image.ref` is the *canonical* reference: `created_state` writes the
/// catalog's `distro:version` for a catalog machine, and the pull rewrites it
/// again from the resolved entry. The spec keeps whatever the operator typed,
/// so a machine created as `ubuntu` or `ubuntu:noble` runs `ubuntu:24.04` and
/// has not drifted at all. Comparing the two strings directly would therefore
/// accuse the flagship `firestone run ubuntu` of permanent image drift, and no
/// restart would ever clear it, so the spec reference is resolved through the
/// same catalog first. Only catalog references need it: a local path, an HTTPS
/// URL and an OCI reference are already canonical in the spec and compare
/// verbatim. A machine that is not running has booted nothing to disagree with.
fn image_pending(catalog: &Catalog, spec: &MachineSpec, state: &MachineState) -> bool {
    state.status == MachineStatus::Running
        && canonical_image_reference(catalog, spec.image.as_str()) != state.image.r#ref
}

/// The catalog's canonical `distro:version` for a reference it knows, and the
/// reference unchanged for one it does not.
fn canonical_image_reference(catalog: &Catalog, reference: &str) -> String {
    catalog
        .entry(reference)
        .map_or_else(|| reference.to_owned(), CatalogEntry::canonical_reference)
}

fn emit_forward_restart_warning(
    spec: &MachineSpec,
    state: &MachineState,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    if forwards_pending(spec, state) {
        events.emit(Event::Log {
            level: Level::Warn,
            message: "port forwards apply on restart".to_owned(),
        })?;
    }
    Ok(())
}

/// Live-resize headroom read back from the VmConfig the running VM booted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VmConfigCapacity {
    boot_vcpus: u8,
    max_vcpus: u8,
    boot_memory: u64,
    max_memory: u64,
}

impl VmConfigCapacity {
    /// Refuses any request the booted machine provably cannot satisfy, so the
    /// spec is never written for a resize Cloud Hypervisor would reject.
    fn check(self, name: &str, cpus: u8, memory: ByteSize) -> Result<(), FirestoneError> {
        if cpus > self.max_vcpus {
            return Err(no_headroom_error(
                name,
                format!(
                    "machine `{name}` booted with max_vcpus {} and cannot reach {cpus} vCPUs",
                    self.max_vcpus
                ),
            ));
        }
        let requested = memory.as_bytes();
        if requested > self.max_memory {
            return Err(no_headroom_error(
                name,
                format!(
                    "machine `{name}` booted with {} bytes of hotplug headroom and cannot reach {requested} bytes",
                    self.max_memory
                ),
            ));
        }
        if requested < self.boot_memory {
            return Err(no_headroom_error(
                name,
                format!(
                    "machine `{name}` booted with {} bytes of memory and cannot shrink below it",
                    self.boot_memory
                ),
            ));
        }
        if cpus < 1 {
            return Err(FirestoneError::new(
                ErrorKind::InvalidSpec,
                format!("machine `{name}` must keep at least one vCPU"),
            )
            .with_hint("set cpus to 1 or more"));
        }
        let _ = self.boot_vcpus;
        Ok(())
    }
}

fn no_headroom_error(name: &str, message: String) -> FirestoneError {
    FirestoneError::new(ErrorKind::Conflict, message).with_hint(format!(
        "set cpus_max/memory_max and restart machine {name:?}"
    ))
}

/// Reads boot and maximum CPU/memory out of one published canonical VmConfig.
fn published_vmconfig_capacity(
    name: &str,
    value: &serde_json::Value,
) -> Result<VmConfigCapacity, FirestoneError> {
    let field = |path: &[&str]| -> Result<Option<u64>, FirestoneError> {
        let mut cursor = value;
        for key in path {
            match cursor.get(key) {
                Some(next) => cursor = next,
                None => return Ok(None),
            }
        }
        cursor.as_u64().map(Some).ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "published VmConfig for machine `{name}` has a non-numeric `{}`",
                    path.join(".")
                ),
            )
            .with_hint(format!(
                "restart machine {name:?} to republish canonical VmConfig"
            ))
        })
    };
    let required = |path: &[&str], found: Option<u64>| -> Result<u64, FirestoneError> {
        found.ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "published VmConfig for machine `{name}` is missing `{}`",
                    path.join(".")
                ),
            )
            .with_hint(format!(
                "restart machine {name:?} to republish canonical VmConfig"
            ))
        })
    };
    let vcpu = |path: &[&str], value: u64| -> Result<u8, FirestoneError> {
        u8::try_from(value).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Dependency,
                format!(
                    "published VmConfig for machine `{name}` has an out-of-range `{}`",
                    path.join(".")
                ),
            )
            .with_hint(format!(
                "restart machine {name:?} to republish canonical VmConfig"
            ))
            .with_source(source)
        })
    };

    let boot_path = ["cpus", "boot_vcpus"];
    let max_path = ["cpus", "max_vcpus"];
    let size_path = ["memory", "size"];
    let boot_vcpus = vcpu(&boot_path, required(&boot_path, field(&boot_path)?)?)?;
    let max_vcpus = vcpu(&max_path, required(&max_path, field(&max_path)?)?)?;
    let boot_memory = required(&size_path, field(&size_path)?)?;
    let hotplug = field(&["memory", "hotplug_size"])?.unwrap_or(0);
    Ok(VmConfigCapacity {
        boot_vcpus,
        max_vcpus,
        boot_memory,
        max_memory: boot_memory.saturating_add(hotplug),
    })
}

/// Warns that a credential or provisioning change reaches the guest only at the
/// next start, because cloud-init runs once per instance identity.
fn emit_credential_change_warning(
    previous: Option<&MachineSpec>,
    next: &MachineSpec,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let mut changed = Vec::new();
    if previous.user != next.user {
        changed.push("user");
    }
    if previous.cloud_init != next.cloud_init {
        changed.push("cloud_init");
    }
    if changed.is_empty() {
        return Ok(());
    }
    events.emit(Event::Log {
        level: Level::Warn,
        message: format!(
            "{} changed; cloud-init reprovisions the guest, so this applies on next start",
            changed.join(" and ")
        ),
    })
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

fn cp_argument(value: &OsStr) -> Result<String, FirestoneError> {
    value.to_str().map(str::to_owned).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Usage,
            format!("cp argument {value:?} is not valid UTF-8"),
        )
        .with_hint("use UTF-8 cp operands and a UTF-8 Firestone home")
    })
}

fn cp_not_running_error(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::NotRunning,
        format!("machine {name} is not running"),
    )
    .with_hint(format!("start it with firestone start {name}"))
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

/// The shared machine-not-running failure for read-only runtime surfaces.
fn machine_not_running(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Conflict,
        format!("machine `{name}` is not running"),
    )
    .with_hint(format!("run firestone start {name}"))
}

/// One deletion system prune planned before it removed anything (SPEC §26).
#[derive(Debug)]
struct PrunePlanItem {
    kind: PruneKind,
    id: String,
    bytes: u64,
    target: PruneTarget,
}

/// Where a planned prune item lives and which lock protects it.
#[derive(Debug)]
enum PruneTarget {
    /// A per-machine runtime directory. `published` records whether the
    /// machine still exists and therefore whether a machine lock is taken.
    RuntimeDir {
        machine: String,
        published: bool,
    },
    File(PathBuf),
    Tree(PathBuf),
    SnapshotTree {
        machine: String,
        path: PathBuf,
    },
    MachineRemovalDir {
        machine: String,
        path: PathBuf,
    },
}

/// One measured directory entry a prune scan matched.
#[derive(Debug)]
struct PruneDirEntry {
    file_name: String,
    bytes: u64,
    directory: bool,
}

/// Statuses the destructive prune tier may remove (SPEC §26).
const fn prune_removable_status(status: MachineStatus) -> bool {
    matches!(
        status,
        MachineStatus::Stopped | MachineStatus::Created | MachineStatus::Failed
    )
}

fn add_counted(counted: &mut BTreeMap<String, u64>, name: &str, bytes: u64) {
    let total = counted.entry(name.to_owned()).or_default();
    *total = total.saturating_add(bytes);
}

fn machine_directory_exists(paths: &Paths, name: &str) -> Result<bool, FirestoneError> {
    let Ok(machine_dir) = paths.machine_dir(name) else {
        return Ok(false);
    };
    match fs::symlink_metadata(&machine_dir) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect machine directory {}", machine_dir.display()),
            "check the machine directory permissions",
            source,
        )),
    }
}

/// Matched entries of one directory, measured and sorted by name.
///
/// A missing directory yields nothing. Symlinks are never followed and never
/// matched: prune only ever deletes real Firestone-owned artifacts.
fn read_prune_entries(
    directory: &Path,
    matches: impl Fn(&str) -> bool,
) -> Result<Vec<(PathBuf, PruneDirEntry)>, FirestoneError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Dependency,
                format!("cannot read directory {}", directory.display()),
                "check the Firestone data directory permissions",
                source,
            ));
        }
    };
    let mut matched = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot read directory {}", directory.display()),
                "check the Firestone data directory permissions",
                source,
            )
        })?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        if !matches(&file_name) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot inspect {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            )
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let directory_entry = metadata.is_dir();
        if !directory_entry && !metadata.is_file() {
            continue;
        }
        let bytes = if directory_entry {
            allocated_tree_bytes(&path)?
        } else {
            allocated_metadata_bytes(&metadata)
        };
        matched.push((
            path,
            PruneDirEntry {
                file_name,
                bytes,
                directory: directory_entry,
            },
        ));
    }
    matched.sort_by(|(_, left), (_, right)| left.file_name.cmp(&right.file_name));
    Ok(matched)
}

/// Bytes one owned regular file occupies, or none when it does not exist.
fn owned_regular_file_bytes(path: &Path) -> Result<Option<u64>, FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(allocated_metadata_bytes(&metadata)))
        }
        Ok(_) => Ok(None),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect {}", path.display()),
            "check the Firestone data directory permissions",
            source,
        )),
    }
}

/// Deletes one planned regular file, refusing anything else in its place.
fn remove_owned_prune_file(path: &Path) -> Result<(), FirestoneError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|source| {
                filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot remove {}", path.display()),
                    "remove the owned file and retry",
                    source,
                )
            })
        }
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "refusing to remove {}: it is not a regular file",
                path.display()
            ),
        )
        .with_hint("remove the symlink or special file manually and retry")),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect {}", path.display()),
            "check the Firestone data directory permissions",
            source,
        )),
    }
}

/// Bytes one metadata record's file occupies on disk, holes excluded.
fn allocated_metadata_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.blocks().saturating_mul(512)
}

/// Bytes one directory tree occupies on disk, including the directories.
///
/// Symlinks are counted as their own inodes and never followed.
fn allocated_tree_bytes(path: &Path) -> Result<u64, FirestoneError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Dependency,
                format!("cannot inspect {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            ));
        }
    };
    let mut total = allocated_metadata_bytes(&metadata);
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(total);
    }
    let entries = fs::read_dir(path).map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot read directory {}", path.display()),
            "check the Firestone data directory permissions",
            source,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot read directory {}", path.display()),
                "check the Firestone data directory permissions",
                source,
            )
        })?;
        total = total.saturating_add(allocated_tree_bytes(&entry.path())?);
    }
    Ok(total)
}

/// Opens one prune step and returns the instant it started.
fn prune_step_start(
    events: &mut dyn EventSink,
    kind: PruneKind,
    id: &str,
) -> Result<Instant, FirestoneError> {
    events.emit(Event::StepStart {
        id: kind.as_str().into(),
        label: id.to_owned(),
    })?;
    Ok(Instant::now())
}

/// Closes one prune step and returns the row it contributes to the result.
fn prune_step_done(
    events: &mut dyn EventSink,
    kind: PruneKind,
    id: String,
    bytes: u64,
    dry_run: bool,
    started: Instant,
) -> Result<PruneItem, FirestoneError> {
    if dry_run {
        events.emit(Event::StepSkip {
            id: kind.as_str().into(),
            reason: format!("dry run · {bytes} bytes"),
        })?;
    } else {
        events.emit(Event::StepDone {
            id: kind.as_str().into(),
            detail: Some(format!("{bytes} bytes")),
            elapsed_ms: elapsed_millis(started.elapsed()),
        })?;
    }
    Ok(PruneItem { kind, id, bytes })
}

/// Reports one already-completed prune step as a start/finish pair.
fn emit_prune_item(
    events: &mut dyn EventSink,
    kind: PruneKind,
    id: String,
    bytes: u64,
    dry_run: bool,
) -> Result<PruneItem, FirestoneError> {
    let started = prune_step_start(events, kind, &id)?;
    prune_step_done(events, kind, id, bytes, dry_run, started)
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
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use firestone_core::{
        Action, Arch, ByteSize, Catalog, Dispatcher, ErrorKind, Event, EventSink, FirestoneError,
        GlobalConfig, ImageRef, Level, LogSource, MachineLock, MachineSpec, MachineSpecPatch,
        MachineStatus, NetworkSpecPatch, PathInputs, Paths, RealValidationHost, StateStore,
        Supervision, ValidationContext, VersionResult,
    };

    use super::{
        LocalDispatcher, MAX_LOG_TAIL_BYTES, display_forward, display_status,
        emit_forward_restart_warning, format_uptime_seconds, forwards_pending, image_pending,
        parse_editor_command, published_vmconfig_capacity,
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

    async fn create_machine(
        dispatcher: &LocalDispatcher,
        name: &str,
        spec: MachineSpec,
    ) -> Result<(), FirestoneError> {
        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Create {
                    name: name.to_owned(),
                    spec,
                },
                &mut events,
            )
            .await
    }

    fn write_owned(path: &std::path::Path, bytes: &[u8]) -> Result<(), std::io::Error> {
        fs::write(path, bytes)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }

    fn fake_qemu(root: &std::path::Path) -> Result<std::path::PathBuf, std::io::Error> {
        let program = root.join("fake-qemu-img");
        fs::write(
            &program,
            br#"#!/bin/sh
case "$1" in
  info)
    printf '%s\n' '{"format":"qcow2","virtual-size":1048576,"dirty-flag":false,"format-specific":{"type":"qcow2","data":{"corrupt":false}}}'
    ;;
  *)
    exit 64
    ;;
esac
"#,
        )?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        Ok(program)
    }

    #[tokio::test]
    async fn resize_stopped_machine_persists_spec_without_a_live_apply()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        create_machine(&dispatcher, "cold", MachineSpec::default()).await?;

        let mut events = Vec::new();
        dispatcher
            .run(
                Action::Resize {
                    name: "cold".to_owned(),
                    cpus: Some(4),
                    memory: Some(ByteSize::from_gib(4)?),
                },
                &mut events,
            )
            .await?;

        let payload = events
            .iter()
            .find_map(|event| match event {
                Event::Result { action, payload } if action == "resize" => Some(payload),
                _ => None,
            })
            .ok_or("missing resize result")?;
        assert_eq!(payload["name"], "cold");
        assert_eq!(payload["applied_live"], false);
        assert_eq!(payload["cpus"], 4);
        assert_eq!(payload["memory"], "4G");

        let (persisted, _) = dispatcher.load_machine("cold")?;
        assert_eq!(persisted.cpus, 4);
        assert_eq!(persisted.memory, ByteSize::from_gib(4)?);
        Ok(())
    }

    #[tokio::test]
    async fn resize_without_cpus_or_memory_is_a_usage_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        create_machine(&dispatcher, "empty-resize", MachineSpec::default()).await?;

        let mut events = Vec::new();
        let error = dispatcher
            .run(
                Action::Resize {
                    name: "empty-resize".to_owned(),
                    cpus: None,
                    memory: None,
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("a no-op resize unexpectedly succeeded")?;

        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("--cpus") && hint.contains("--memory"))
        );
        assert!(events.is_empty());
        Ok(())
    }

    #[test]
    fn published_vmconfig_capacity_reads_boot_and_hotplug_headroom()
    -> Result<(), Box<dyn std::error::Error>> {
        let with_headroom = published_vmconfig_capacity(
            "demo",
            &serde_json::json!({
                "cpus": {"boot_vcpus": 2, "max_vcpus": 8},
                "memory": {"size": 2_147_483_648_u64, "hotplug_size": 4_294_967_296_u64}
            }),
        )?;
        assert_eq!(with_headroom.max_vcpus, 8);
        assert_eq!(with_headroom.boot_memory, 2_147_483_648);
        assert_eq!(with_headroom.max_memory, 6_442_450_944);

        let without_headroom = published_vmconfig_capacity(
            "demo",
            &serde_json::json!({
                "cpus": {"boot_vcpus": 2, "max_vcpus": 2},
                "memory": {"size": 2_147_483_648_u64}
            }),
        )?;
        assert_eq!(without_headroom.max_vcpus, 2);
        assert_eq!(without_headroom.max_memory, 2_147_483_648);

        let missing = published_vmconfig_capacity("demo", &serde_json::json!({"cpus": {}}))
            .err()
            .ok_or("a VmConfig without cpus.boot_vcpus unexpectedly parsed")?;
        assert_eq!(missing.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[test]
    fn resize_beyond_booted_headroom_refuses_with_a_restart_hint()
    -> Result<(), Box<dyn std::error::Error>> {
        let capacity = published_vmconfig_capacity(
            "demo",
            &serde_json::json!({
                "cpus": {"boot_vcpus": 2, "max_vcpus": 4},
                "memory": {"size": 2_147_483_648_u64, "hotplug_size": 2_147_483_648_u64}
            }),
        )?;

        capacity.check("demo", 4, ByteSize::from_gib(4)?)?;
        capacity.check("demo", 1, ByteSize::from_gib(2)?)?;

        for (cpus, memory) in [
            (5_u8, ByteSize::from_gib(2)?),
            (2, ByteSize::from_gib(5)?),
            (2, ByteSize::from_gib(1)?),
        ] {
            let error = capacity
                .check("demo", cpus, memory)
                .err()
                .ok_or("a request outside the booted headroom unexpectedly passed")?;
            assert_eq!(error.kind(), ErrorKind::Conflict);
            assert!(
                error
                    .hint()
                    .is_some_and(|hint| hint.contains("set cpus_max/memory_max and restart")),
                "{:?}",
                error.hint()
            );
        }
        Ok(())
    }

    /// A fake qemu-img whose `info` reports a fixed overlay virtual size.
    fn fake_qemu_reporting(
        root: &std::path::Path,
        name: &str,
        virtual_size: u64,
    ) -> Result<std::path::PathBuf, std::io::Error> {
        let program = root.join(name);
        fs::write(
            &program,
            format!(
                r#"#!/bin/sh
case "$1" in
  info)
    printf '%s\n' '{{"format":"qcow2","virtual-size":{virtual_size},"dirty-flag":false,"format-specific":{{"type":"qcow2","data":{{"corrupt":false}}}}}}'
    ;;
  *)
    exit 64
    ;;
esac
"#
            )
            .as_bytes(),
        )?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        Ok(program)
    }

    #[tokio::test]
    async fn patch_and_set_spec_disk_below_existing_overlay_refuse_the_shrink()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, paths) = fixture()?;
        let root = fs::canonicalize(directory.path())?;
        let overlay_bytes = ByteSize::from_gib(20)?.as_bytes();
        let dispatcher = dispatcher.with_programs(
            fake_qemu_reporting(&root, "fake-qemu-20g", overlay_bytes)?,
            root.join("shim"),
        );
        create_machine(&dispatcher, "shrink", MachineSpec::default()).await?;
        write_owned(&paths.machine_disk("shrink")?, b"QFI\xfb")?;

        for patch_disk in [ByteSize::from_gib(10)?, ByteSize::from_mib(128)?] {
            let mut events = Vec::new();
            let error = dispatcher
                .run(
                    Action::PatchSpec {
                        name: "shrink".to_owned(),
                        patch: MachineSpecPatch {
                            disk: Some(patch_disk),
                            ..MachineSpecPatch::default()
                        },
                    },
                    &mut events,
                )
                .await
                .err()
                .ok_or("a disk shrink unexpectedly succeeded")?;
            assert_eq!(error.kind(), ErrorKind::InvalidSpec);
            assert!(
                error.message().contains("disk shrink is not supported"),
                "{}",
                error.message()
            );
            assert!(error.hint().is_some());
        }

        let mut events = Vec::new();
        let error = dispatcher
            .run(
                Action::SetSpec {
                    name: "shrink".to_owned(),
                    spec: MachineSpec {
                        disk: ByteSize::from_gib(10)?,
                        ..MachineSpec::default()
                    },
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("a PUT disk shrink unexpectedly succeeded")?;
        assert!(error.message().contains("disk shrink is not supported"));

        // The persisted spec is untouched, and a grow is accepted.
        let (persisted, _) = dispatcher.load_machine("shrink")?;
        assert_eq!(persisted.disk, ByteSize::from_gib(20)?);
        events.clear();
        dispatcher
            .run(
                Action::PatchSpec {
                    name: "shrink".to_owned(),
                    patch: MachineSpecPatch {
                        disk: Some(ByteSize::from_gib(30)?),
                        ..MachineSpecPatch::default()
                    },
                },
                &mut events,
            )
            .await?;
        let (grown, _) = dispatcher.load_machine("shrink")?;
        assert_eq!(grown.disk, ByteSize::from_gib(30)?);
        Ok(())
    }

    #[tokio::test]
    async fn patch_spec_appends_validates_and_persists_at_dispatcher_source()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        let mut original = MachineSpec::default();
        original.network.forward.push("8080:80".parse()?);
        create_machine(&dispatcher, "patched", original).await?;

        let patch = MachineSpecPatch {
            cpus: Some(4),
            network: Some(NetworkSpecPatch {
                forward: Some(vec!["8081:81".parse()?]),
                ..NetworkSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };
        let mut events = Vec::new();
        dispatcher
            .run(
                Action::PatchSpec {
                    name: "patched".to_owned(),
                    patch,
                },
                &mut events,
            )
            .await?;
        assert!(matches!(events.last(), Some(Event::Result { action, .. }) if action == "edit"));

        let (persisted, _) = dispatcher.load_machine("patched")?;
        assert_eq!(persisted.cpus, 4);
        assert_eq!(
            persisted
                .network
                .forward
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["8080:80", "8081:81"]
        );

        let error = dispatcher
            .run(
                Action::PatchSpec {
                    name: "patched".to_owned(),
                    patch: MachineSpecPatch {
                        cpus: Some(0),
                        ..MachineSpecPatch::default()
                    },
                },
                &mut Vec::new(),
            )
            .await
            .err()
            .ok_or("invalid patch unexpectedly succeeded")?;
        assert_eq!(error.kind(), ErrorKind::InvalidSpec);
        assert_eq!(dispatcher.load_machine("patched")?.0, persisted);
        Ok(())
    }

    #[tokio::test]
    async fn version_action_emits_typed_versions_and_resolved_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut events = Vec::new();
        dispatcher.run(Action::Version, &mut events).await?;
        let payload = match events.as_slice() {
            [Event::Result { action, payload }] if action == "version" => payload,
            _ => return Err("version did not emit exactly one Result".into()),
        };
        let result = serde_json::from_value::<VersionResult>(payload.clone())?;
        let architecture = Arch::current().map_err(std::io::Error::other)?;
        assert_eq!(result.version, env!("CARGO_PKG_VERSION"));
        // Derived, not pinned: the release workflow bumps the workspace
        // version, and a literal here is wrong the moment it does.
        assert_eq!(
            result.identity.release,
            format!("v{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            result.identity.git_commit.as_deref(),
            option_env!("FIRESTONE_GIT_COMMIT")
        );
        assert_eq!(result.architecture, architecture.as_str());
        let expected_dependencies = match architecture {
            Arch::X86_64 => 7,
            Arch::Aarch64 => 5,
        };
        assert_eq!(result.dependencies.len(), expected_dependencies);
        let kernel = result
            .dependencies
            .get("cloud-hypervisor-kernel")
            .ok_or("direct-boot kernel version pin is missing")?;
        assert_eq!(kernel.version, "ch-release-v6.16.9-20260508");
        assert_eq!(
            kernel.sha256,
            match architecture {
                Arch::X86_64 => "58088758f601a04ef85b09cf23db5530d51edc039ed47afbf2264c5b762cb568",
                Arch::Aarch64 => "69d1b1235381ec50f1b45cf771a7dff4a9013d452833ab34682d6283e2114010",
            }
        );
        if architecture == Arch::X86_64 {
            assert_eq!(
                result
                    .dependencies
                    .get("passt")
                    .map(|value| value.sha256.as_str()),
                Some("40e59201765c60a0a5bbd0f2caae1aae3fd8f9a9a0628a835159fb2f17ff7025")
            );
            assert_eq!(
                result
                    .dependencies
                    .get("qemu-img")
                    .map(|value| value.sha256.as_str()),
                Some("30bff329fe1001635cafcfebddc68a1c824d25110c66f968b428c4cf4785d75d")
            );
        }
        let cloud_hypervisor = result
            .dependencies
            .get("cloud-hypervisor")
            .ok_or("cloud-hypervisor version pin is missing")?;
        assert_eq!(cloud_hypervisor.version, "v53.0");
        assert_eq!(
            cloud_hypervisor.sha256,
            match architecture {
                Arch::X86_64 => "448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc",
                Arch::Aarch64 => "f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850",
            }
        );
        assert_eq!(
            result.paths.config,
            paths.config_dir().display().to_string()
        );
        assert_eq!(result.paths.data, paths.data_dir().display().to_string());
        assert_eq!(
            result.paths.runtime,
            paths.runtime_dir().display().to_string()
        );
        Ok(())
    }

    #[tokio::test]
    async fn stop_created_machine_emits_skip_and_one_result()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "created-stop", MachineSpec::default()).await?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Stop {
                    name: "created-stop".to_owned(),
                    timeout: Duration::from_secs(1),
                    force: false,
                },
                &mut events,
            )
            .await?;

        assert!(matches!(
            events.first(),
            Some(Event::StepSkip { id, reason }) if id.as_str() == "stop" && reason == "not running"
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Result { action, .. } if action == "stop"))
                .count(),
            1
        );
        assert_eq!(
            StateStore::new(paths.machine_state("created-stop")?)
                .read()?
                .status,
            MachineStatus::Created
        );
        Ok(())
    }

    #[tokio::test]
    async fn show_vmconfig_missing_then_canonical_returns_exact_object()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "vmconfig", MachineSpec::default()).await?;
        let mut events = Vec::new();
        let action = Action::Show {
            name: "vmconfig".to_owned(),
            vmconfig: true,
        };

        let missing = dispatcher
            .run(action.clone(), &mut events)
            .await
            .err()
            .ok_or("missing VmConfig unexpectedly succeeded")?;
        assert_eq!(missing.kind(), ErrorKind::NotFound);
        assert!(
            missing
                .hint()
                .is_some_and(|hint| hint.contains("start machine"))
        );

        let exact = br#"{"a":1,"nested":{"b":2}}"#;
        write_owned(&paths.machine_vmconfig("vmconfig")?, exact)?;
        events.clear();
        dispatcher.run(action, &mut events).await?;
        let payload = events
            .iter()
            .find_map(|event| match event {
                Event::Result { action, payload } if action == "show-vmconfig" => Some(payload),
                _ => None,
            })
            .ok_or("missing show-vmconfig result")?;
        assert_eq!(serde_json::to_vec(payload)?, exact);
        Ok(())
    }

    #[tokio::test]
    async fn show_vmconfig_symlink_and_mode_are_refused() -> Result<(), Box<dyn std::error::Error>>
    {
        let (directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "unsafe-vmconfig", MachineSpec::default()).await?;
        let target = directory.path().join("outside.json");
        write_owned(&target, br#"{}"#)?;
        let vmconfig = paths.machine_vmconfig("unsafe-vmconfig")?;
        symlink(&target, &vmconfig)?;
        let action = Action::Show {
            name: "unsafe-vmconfig".to_owned(),
            vmconfig: true,
        };
        let mut events = Vec::new();

        let symlink_error = dispatcher
            .run(action.clone(), &mut events)
            .await
            .err()
            .ok_or("VmConfig symlink unexpectedly succeeded")?;
        assert_eq!(symlink_error.kind(), ErrorKind::Dependency);
        fs::remove_file(&vmconfig)?;
        write_owned(&vmconfig, br#"{}"#)?;
        fs::set_permissions(&vmconfig, fs::Permissions::from_mode(0o644))?;
        let mode_error = dispatcher
            .run(action, &mut events)
            .await
            .err()
            .ok_or("permissive VmConfig unexpectedly succeeded")?;
        assert_eq!(mode_error.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[tokio::test]
    async fn logs_tail_returns_last_lines_and_one_result() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "logs", MachineSpec::default()).await?;
        write_owned(
            &paths.machine_console_log("logs")?,
            b"one\ntwo\nthree\nfour\n",
        )?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Logs {
                    name: "logs".to_owned(),
                    source: LogSource::Console,
                    lines: 2,
                    follow: false,
                },
                &mut events,
            )
            .await?;

        assert!(matches!(
            events.first(),
            Some(Event::Output { data }) if data == "three\nfour\n"
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Result { action, .. } if action == "logs"))
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn logs_follow_cancellation_is_bounded_without_result()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "follow", MachineSpec::default()).await?;
        write_owned(&paths.machine_console_log("follow")?, b"ready\n")?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancelled);
        let thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(120));
            setter.store(true, Ordering::Relaxed);
        });
        let mut events = Vec::new();
        let started = Instant::now();

        let error = dispatcher
            .logs_until(
                "follow",
                LogSource::Console,
                1,
                true,
                &cancelled,
                &mut events,
            )
            .err()
            .ok_or("follow did not cancel")?;
        thread.join().map_err(|_| "cancellation thread panicked")?;

        assert_eq!(error.kind(), ErrorKind::Interrupted);
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Result { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn logs_symlink_and_permissive_mode_are_refused() -> Result<(), Box<dyn std::error::Error>>
    {
        let (directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "unsafe-log", MachineSpec::default()).await?;
        let target = directory.path().join("outside.log");
        write_owned(&target, b"outside\n")?;
        let log = paths.machine_console_log("unsafe-log")?;
        symlink(&target, &log)?;
        let action = Action::Logs {
            name: "unsafe-log".to_owned(),
            source: LogSource::Console,
            lines: 1,
            follow: false,
        };
        let mut events = Vec::new();

        let symlink_error = dispatcher
            .run(action.clone(), &mut events)
            .await
            .err()
            .ok_or("log symlink unexpectedly succeeded")?;
        assert_eq!(symlink_error.kind(), ErrorKind::Dependency);
        fs::remove_file(&log)?;
        write_owned(&log, b"owned\n")?;
        fs::set_permissions(&log, fs::Permissions::from_mode(0o666))?;
        let mode_error = dispatcher
            .run(action, &mut events)
            .await
            .err()
            .ok_or("permissive log unexpectedly succeeded")?;
        assert_eq!(mode_error.kind(), ErrorKind::Dependency);
        Ok(())
    }

    #[tokio::test]
    async fn logs_single_line_beyond_scan_bound_is_refused()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "bounded-log", MachineSpec::default()).await?;
        let length = usize::try_from(MAX_LOG_TAIL_BYTES)?.saturating_add(1);
        write_owned(
            &paths.machine_console_log("bounded-log")?,
            &vec![b'x'; length],
        )?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Logs {
                    name: "bounded-log".to_owned(),
                    source: LogSource::Console,
                    lines: 1,
                    follow: false,
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("oversized log line unexpectedly succeeded")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.message().contains("scan limit"));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Result { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn rm_duplicate_and_symlink_refuse_without_partial_deletion()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "one", MachineSpec::default()).await?;
        create_machine(&dispatcher, "two", MachineSpec::default()).await?;
        let mut events = Vec::new();
        let duplicate = dispatcher
            .run(
                Action::Remove {
                    names: vec!["one".to_owned(), "one".to_owned()],
                    force: false,
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("duplicate rm unexpectedly succeeded")?;
        assert_eq!(duplicate.kind(), ErrorKind::Usage);
        assert!(paths.machine_dir("one")?.exists());
        assert!(paths.machine_dir("two")?.exists());

        let outside = directory.path().join("outside");
        fs::create_dir(&outside)?;
        symlink(&outside, paths.machine_dir("two")?.join("redirect"))?;
        let unsafe_error = dispatcher
            .run(
                Action::Remove {
                    names: vec!["one".to_owned(), "two".to_owned()],
                    force: false,
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("rm with symlink unexpectedly succeeded")?;
        assert_eq!(unsafe_error.kind(), ErrorKind::Dependency);
        assert!(paths.machine_dir("one")?.exists());
        assert!(paths.machine_dir("two")?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn rm_stopped_machines_removes_publications_but_preserves_images()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "one", MachineSpec::default()).await?;
        create_machine(&dispatcher, "two", MachineSpec::default()).await?;
        write_owned(&paths.machine_known_hosts("one")?, b"trusted-host-key")?;
        paths.ensure_owned_data_directory(&paths.images_dir(), "images directory", false)?;
        let shared = paths.images_dir().join("shared-sentinel");
        write_owned(&shared, b"shared")?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Remove {
                    names: vec!["one".to_owned(), "two".to_owned()],
                    force: false,
                },
                &mut events,
            )
            .await?;

        assert!(!paths.machine_dir("one")?.exists());
        assert!(!paths.machine_dir("two")?.exists());
        assert!(shared.exists());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Result { action, .. } if action == "rm"))
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn images_actions_pull_list_inspect_reference_and_remove()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, paths) = fixture()?;
        let qemu = fake_qemu(directory.path())?;
        let dispatcher = dispatcher
            .with_programs(qemu.clone(), qemu)
            .with_source_base(directory.path().to_path_buf());
        let source = directory.path().join("base.qcow2");
        write_owned(&source, b"QFI\xfbowned-image")?;
        let mut events = Vec::new();

        dispatcher.image_pull(
            ImageRef::from(source.to_string_lossy().into_owned()),
            None,
            &mut events,
        )?;
        let pull = events
            .iter()
            .find_map(|event| match event {
                Event::Result { action, payload } if action == "images-pull" => Some(payload),
                _ => None,
            })
            .ok_or("missing image pull result")?;
        let id = pull
            .pointer("/metadata/id")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing image id")?
            .to_owned();
        let source_sha = pull
            .pointer("/metadata/source_sha256")
            .and_then(serde_json::Value::as_str)
            .ok_or("missing source sha")?
            .to_owned();

        events.clear();
        dispatcher.image_list(&mut events)?;
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Result { action, payload }
                if action == "images-ls" && payload.as_array().is_some_and(|images| images.len() == 1)
        )));
        events.clear();
        dispatcher.image_inspect(&id, &mut events)?;
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Result { action, payload }
                if action == "images-inspect" && payload["image"]["metadata"]["id"] == id
        )));

        dispatcher.create("image-user", MachineSpec::default(), &mut events)?;
        let state_path = paths.machine_state("image-user")?;
        let mut state = StateStore::new(state_path.clone()).read()?;
        state.image.id = Some(id.clone());
        state.image.sha256 = Some(source_sha);
        StateStore::new(state_path).write_from_shim(&state)?;
        let referenced = dispatcher
            .image_remove(&id, false, &mut events)
            .err()
            .ok_or("referenced image unexpectedly removed")?;
        assert_eq!(referenced.kind(), ErrorKind::Conflict);
        dispatcher.image_remove(&id, true, &mut events)?;
        Ok(())
    }

    #[test]
    fn images_pull_sha256_on_local_file_returns_usage() -> Result<(), Box<dyn std::error::Error>> {
        let (directory, dispatcher, _paths) = fixture()?;
        let qemu = fake_qemu(directory.path())?;
        let dispatcher = dispatcher
            .with_programs(qemu.clone(), qemu)
            .with_source_base(directory.path().to_path_buf());
        let source = directory.path().join("base.qcow2");
        write_owned(&source, b"QFI\xfbowned-image")?;
        let mut events = Vec::new();

        let error = dispatcher
            .image_pull(
                ImageRef::from(source.to_string_lossy().into_owned()),
                Some("a".repeat(64)),
                &mut events,
            )
            .err()
            .ok_or("local --sha256 unexpectedly succeeded")?;
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert_eq!(
            error.hint(),
            Some("remove --sha256 when pulling a local file")
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Result { .. }))
        );
        Ok(())
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
                    vmconfig: false,
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
        create_machine(&dispatcher, "retired", MachineSpec::default()).await?;
        let spec = MachineSpec {
            image: "retired:1".into(),
            ..MachineSpec::default()
        };
        fs::write(paths.machine_spec("retired")?, spec.to_toml()?)?;
        let state_path = paths.machine_state("retired")?;
        let mut state = StateStore::new(state_path.clone()).read()?;
        state.image.r#ref = "retired:1".to_owned();
        StateStore::new(state_path.clone()).write_from_shim(&state)?;
        let mut events = Vec::new();
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
                    vmconfig: false,
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

    /// Extracts the terminal prune payload from one action's event stream.
    fn prune_result(
        events: &[Event],
    ) -> Result<firestone_core::PruneResult, Box<dyn std::error::Error>> {
        for event in events {
            if let Event::Result { action, payload } = event
                && action == "system-prune"
            {
                return Ok(serde_json::from_value(payload.clone())?);
            }
        }
        Err("the prune action emitted no system-prune result".into())
    }

    fn prune_action(machines: bool, images: bool, force: bool, dry_run: bool) -> Action {
        Action::SystemPrune {
            machines,
            images,
            force,
            dry_run,
        }
    }

    /// Writes one of every inert artifact class into a fresh machine.
    fn seed_prune_debris(paths: &Paths, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        write_owned(&paths.machine_console_previous_log(name)?, &[b'p'; 8192])?;
        write_owned(&paths.machine_disk_partial(name)?, &[b'd'; 4096])?;
        let snapshots = paths.machine_snapshots_dir(name)?;
        firestone_core::ensure_snapshot_directory(&snapshots)?;
        let partial = paths.machine_snapshot_partial_dir(name, "snap-20260902-123456")?;
        firestone_core::ensure_snapshot_directory(&partial)?;
        write_owned(&partial.join("metadata.json"), b"{}")?;
        Ok(())
    }

    /// Creates one stale runtime directory whose machine no longer exists.
    fn seed_ghost_runtime_dir(paths: &Paths, name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let directory = paths.ensure_machine_runtime_dir(name)?;
        write_owned(&directory.join("shim.pid"), b"4242\n")?;
        Ok(())
    }

    /// Answers `vmm.ping` on one machine's api socket so it reconciles running.
    fn spawn_fake_ping(socket: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let listener = std::os::unix::net::UnixListener::bind(socket)?;
        thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let body =
                    br#"{"build_version":"v53.0","version":"53.0.0","pid":42,"features":["kvm"]}"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nServer: Cloud Hypervisor API\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.flush();
            }
        });
        Ok(())
    }

    #[test]
    fn system_prune_dry_run_lists_exactly_what_the_act_removes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        firestone_core::block_on(create_machine(&dispatcher, "keep", MachineSpec::default()))?;
        firestone_core::block_on(create_machine(&dispatcher, "old", MachineSpec::default()))?;
        seed_prune_debris(&paths, "keep")?;
        seed_ghost_runtime_dir(&paths, "ghost")?;
        let tombstone = paths.machine_removal_dir("gone")?;
        fs::create_dir_all(&tombstone)?;
        fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o700))?;
        write_owned(&tombstone.join("state.json"), b"{}")?;
        paths.ensure_owned_data_directory(&paths.images_dir(), "images directory", true)?;
        let store_partial =
            paths.image_file(&format!(".pull-{}.stored.partial", "b".repeat(64)))?;
        write_owned(&store_partial, &[b'i'; 2048])?;

        let mut events = Vec::new();
        firestone_core::block_on(
            dispatcher.run(prune_action(true, true, true, true), &mut events),
        )?;
        let planned = prune_result(&events)?;
        assert!(planned.dry_run);
        assert!(planned.reclaimed_bytes > 0);
        assert_eq!(
            planned
                .removed
                .iter()
                .map(|item| (item.kind.to_string(), item.id.clone()))
                .collect::<Vec<_>>(),
            vec![
                ("runtime".to_owned(), "ghost".to_owned()),
                ("log".to_owned(), "keep/console.log.previous".to_owned()),
                (
                    "partial".to_owned(),
                    "machines/keep/disk.qcow2.partial".to_owned()
                ),
                ("partial".to_owned(), "machines/.removing-gone".to_owned()),
                (
                    "snapshot-partial".to_owned(),
                    "keep/snapshots/.partial-snap-20260902-123456".to_owned()
                ),
                (
                    "partial".to_owned(),
                    format!("images/.pull-{}.stored.partial", "b".repeat(64))
                ),
                ("machine".to_owned(), "keep".to_owned()),
                ("machine".to_owned(), "old".to_owned()),
            ]
        );
        // A dry run is a read: every artifact it named is still there.
        assert!(store_partial.exists());
        assert!(tombstone.exists());
        assert!(paths.machine_dir("keep")?.exists());

        events.clear();
        firestone_core::block_on(
            dispatcher.run(prune_action(true, true, true, false), &mut events),
        )?;
        let acted = prune_result(&events)?;
        assert!(!acted.dry_run);
        assert_eq!(acted.removed, planned.removed);
        assert_eq!(acted.reclaimed_bytes, planned.reclaimed_bytes);
        assert!(!store_partial.exists());
        assert!(!tombstone.exists());
        assert!(!paths.machine_dir("keep")?.exists());
        assert!(!paths.machine_dir("old")?.exists());
        assert!(!paths.machine_runtime_dir("ghost")?.exists());

        // The second act finds nothing left and says so without failing.
        events.clear();
        firestone_core::block_on(
            dispatcher.run(prune_action(true, true, true, false), &mut events),
        )?;
        let empty = prune_result(&events)?;
        assert_eq!(empty.removed, Vec::new());
        assert_eq!(empty.reclaimed_bytes, 0);
        Ok(())
    }

    #[test]
    fn system_prune_machines_without_force_is_refused_before_anything_is_removed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        firestone_core::block_on(create_machine(&dispatcher, "old", MachineSpec::default()))?;
        seed_prune_debris(&paths, "old")?;

        let mut events = Vec::new();
        let error = firestone_core::block_on(
            dispatcher.run(prune_action(true, false, false, false), &mut events),
        )
        .err()
        .ok_or("expected the destructive tier to require force")?;
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(error.message().contains("force"), "{error}");
        assert!(
            error.hint().is_some_and(|hint| hint.contains("force")),
            "{error:?}"
        );
        assert!(events.is_empty(), "{events:?}");
        assert!(paths.machine_dir("old")?.exists());
        assert!(paths.machine_console_previous_log("old")?.exists());

        // The preview is exempt: it is the only way to see the tier's effect.
        let mut events = Vec::new();
        firestone_core::block_on(
            dispatcher.run(prune_action(true, false, false, true), &mut events),
        )?;
        let planned = prune_result(&events)?;
        assert!(
            planned
                .removed
                .iter()
                .any(|item| item.kind == firestone_core::PruneKind::Machine && item.id == "old")
        );
        assert!(paths.machine_dir("old")?.exists());
        assert_eq!(
            dispatcher.prune_confirmation_names()?,
            vec!["old".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn system_prune_keeps_a_running_machine_runtime_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        firestone_core::block_on(create_machine(&dispatcher, "live", MachineSpec::default()))?;
        firestone_core::block_on(create_machine(&dispatcher, "idle", MachineSpec::default()))?;
        let live_runtime = paths.ensure_machine_runtime_dir("live")?;
        spawn_fake_ping(&paths.machine_api_socket("live")?)?;
        let idle_runtime = paths.ensure_machine_runtime_dir("idle")?;
        write_owned(&idle_runtime.join("shim.pid"), b"9\n")?;

        let mut events = Vec::new();
        firestone_core::block_on(
            dispatcher.run(prune_action(false, false, false, false), &mut events),
        )?;
        let result = prune_result(&events)?;

        let runtime_ids = result
            .removed
            .iter()
            .filter(|item| item.kind == firestone_core::PruneKind::Runtime)
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(runtime_ids, vec!["idle".to_owned()]);
        assert!(live_runtime.exists());
        assert!(!idle_runtime.exists());
        // Neither machine is a candidate without the destructive tier.
        assert!(paths.machine_dir("live")?.exists());
        assert!(paths.machine_dir("idle")?.exists());
        Ok(())
    }

    #[test]
    fn system_prune_counts_allocated_bytes_once_per_artifact()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        firestone_core::block_on(create_machine(&dispatcher, "sized", MachineSpec::default()))?;
        let previous = paths.machine_console_previous_log("sized")?;
        write_owned(&previous, &[b'p'; 8192])?;
        let expected_log_bytes = fs::symlink_metadata(&previous)?
            .blocks()
            .saturating_mul(512);
        let machine_bytes = super::allocated_tree_bytes(&paths.machine_dir("sized")?)?;

        let mut events = Vec::new();
        firestone_core::block_on(
            dispatcher.run(prune_action(true, false, true, true), &mut events),
        )?;
        let result = prune_result(&events)?;

        let log = result
            .removed
            .iter()
            .find(|item| item.kind == firestone_core::PruneKind::Log)
            .ok_or("expected the rotated console log to be reported")?;
        assert_eq!(log.bytes, expected_log_bytes);
        assert!(log.bytes >= 8192, "{log:?}");
        let machine = result
            .removed
            .iter()
            .find(|item| item.kind == firestone_core::PruneKind::Machine)
            .ok_or("expected the machine to be reported")?;
        // The rotated log is charged to its own row, never to the machine's.
        assert_eq!(
            machine.bytes,
            machine_bytes.saturating_sub(expected_log_bytes)
        );
        assert_eq!(
            result.reclaimed_bytes,
            result
                .removed
                .iter()
                .fold(0_u64, |total, item| total + item.bytes)
        );
        assert_eq!(result.reclaimed_bytes, machine_bytes);
        Ok(())
    }

    #[tokio::test]
    async fn doctor_action_emits_all_check_results() -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        let mut events = Vec::new();

        dispatcher
            .run(
                Action::Doctor {
                    fix: false,
                    elevation_confirmed: false,
                },
                &mut events,
            )
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
                vmconfig: false,
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
            .run(
                Action::Doctor {
                    fix: false,
                    elevation_confirmed: false,
                },
                &mut events,
            )
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
                    vmconfig: false,
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

    #[tokio::test]
    async fn forwards_pending_diff_matrix_ignores_order_and_requires_running()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut spec = MachineSpec::default();
        spec.network.forward.push("8080:80".parse()?);
        spec.network.forward.push("udp:5353:53".parse()?);
        create_machine(&dispatcher, "pending", spec.clone()).await?;

        let store = StateStore::new(paths.machine_state("pending")?);
        let mut state = store.read()?;

        // A machine that never started has applied nothing and is never pending.
        state.forwards.clear();
        assert!(!forwards_pending(&spec, &state));

        state.status = MachineStatus::Running;
        // Same set, opposite configuration order.
        state.forwards = vec!["udp:5353:53".to_owned(), "8080:80".to_owned()];
        assert!(!forwards_pending(&spec, &state));
        // One forward removed from the applied set.
        state.forwards = vec!["8080:80".to_owned()];
        assert!(forwards_pending(&spec, &state));
        // One forward the spec no longer configures.
        state.forwards = vec![
            "8080:80".to_owned(),
            "udp:5353:53".to_owned(),
            "9090:90".to_owned(),
        ];
        assert!(forwards_pending(&spec, &state));

        // The same difference on a stopped machine is not pending.
        state.status = MachineStatus::Stopped;
        assert!(!forwards_pending(&spec, &state));
        Ok(())
    }

    #[tokio::test]
    async fn image_pending_resolves_catalog_aliases_before_reporting_drift()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let catalog = Catalog::built_in()?;
        let mut spec = MachineSpec {
            image: ImageRef::new("ubuntu:24.04"),
            ..MachineSpec::default()
        };
        create_machine(&dispatcher, "aliased", spec.clone()).await?;

        let store = StateStore::new(paths.machine_state("aliased")?);
        let mut state = store.read()?;
        assert_eq!(state.image.r#ref, "ubuntu:24.04");
        state.status = MachineStatus::Running;

        // The default reference and the alias both resolve to what was booted.
        spec.image = ImageRef::new("ubuntu");
        assert!(!image_pending(&catalog, &spec, &state));
        spec.image = ImageRef::new("ubuntu:noble");
        assert!(!image_pending(&catalog, &spec, &state));
        spec.image = ImageRef::new("ubuntu:24.04");
        assert!(!image_pending(&catalog, &spec, &state));

        // A different catalog entry, by canonical reference or by alias, is
        // real drift.
        spec.image = ImageRef::new("debian:12");
        assert!(image_pending(&catalog, &spec, &state));
        spec.image = ImageRef::new("ubuntu:jammy");
        assert!(image_pending(&catalog, &spec, &state));

        // A reference the catalog does not own is already canonical in the
        // spec and compares verbatim.
        spec.image = ImageRef::new("docker://nginx:latest");
        assert!(image_pending(&catalog, &spec, &state));
        state.image.r#ref = "docker://nginx:latest".to_owned();
        assert!(!image_pending(&catalog, &spec, &state));

        // Nothing is pending against a machine that booted nothing.
        state.status = MachineStatus::Stopped;
        spec.image = ImageRef::new("debian:12");
        assert!(!image_pending(&catalog, &spec, &state));
        Ok(())
    }

    #[tokio::test]
    async fn emit_forward_restart_warning_running_change_emits_one_warning()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        let mut spec = MachineSpec::default();
        spec.network.forward.push("8080:80".parse()?);
        create_machine(&dispatcher, "warned", spec.clone()).await?;

        let store = StateStore::new(paths.machine_state("warned")?);
        let mut state = store.read()?;
        state.status = MachineStatus::Running;
        state.forwards = vec!["9090:90".to_owned()];

        let mut events = Vec::new();
        emit_forward_restart_warning(&spec, &state, &mut events)?;
        assert_eq!(
            events,
            vec![Event::Log {
                level: Level::Warn,
                message: "port forwards apply on restart".to_owned(),
            }]
        );

        // An unchanged forward set, and a stopped machine, stay silent.
        let mut events = Vec::new();
        state.forwards = vec!["8080:80".to_owned()];
        emit_forward_restart_warning(&spec, &state, &mut events)?;
        state.status = MachineStatus::Stopped;
        state.forwards = vec!["9090:90".to_owned()];
        emit_forward_restart_warning(&spec, &state, &mut events)?;
        assert!(events.is_empty());
        Ok(())
    }

    #[test]
    fn format_uptime_seconds_boundaries_use_short_units() {
        assert_eq!(format_uptime_seconds(41), "41s");
        assert_eq!(format_uptime_seconds(60), "1m");
        assert_eq!(format_uptime_seconds(3_600), "1h");
        assert_eq!(format_uptime_seconds(172_800), "2d");
    }

    #[tokio::test]
    async fn cp_plan_stopped_machine_refuses_with_the_shell_not_running_family()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        create_machine(&dispatcher, "copy", MachineSpec::default()).await?;

        let error = dispatcher
            .cp_plan("./notes.txt", "copy:/tmp/notes.txt", false)
            .err()
            .ok_or("a created machine must refuse cp")?;

        assert_eq!(error.kind(), ErrorKind::NotRunning);
        assert_eq!(error.message(), "machine copy is not running");
        assert_eq!(error.hint(), Some("start it with firestone start copy"));
        Ok(())
    }

    #[tokio::test]
    async fn cp_plan_operand_pairs_without_one_machine_are_usage_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        create_machine(&dispatcher, "copy", MachineSpec::default()).await?;

        let local = dispatcher
            .cp_plan("./here", "./there", false)
            .err()
            .ok_or("two local operands must fail")?;
        assert_eq!(local.kind(), ErrorKind::Usage);
        assert_eq!(local.message(), "neither cp operand names a machine");

        let remote = dispatcher
            .cp_plan("copy:/a", "copy:/b", false)
            .err()
            .ok_or("two remote operands must fail")?;
        assert_eq!(remote.kind(), ErrorKind::Usage);
        assert_eq!(remote.message(), "both cp operands name a machine");

        let missing = dispatcher
            .cp_plan("./here", "absent:/there", false)
            .err()
            .ok_or("an unknown machine must fail")?;
        assert_eq!(missing.kind(), ErrorKind::NotFound);
        Ok(())
    }

    #[tokio::test]
    async fn cp_action_dispatch_reports_the_same_refusal_as_the_plan()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        create_machine(&dispatcher, "copy", MachineSpec::default()).await?;
        let mut events = Vec::new();

        let error = dispatcher
            .run(
                Action::Cp {
                    source: "copy:/etc/hostname".to_owned(),
                    target: "./hostname".to_owned(),
                    recursive: false,
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("a created machine must refuse the cp action")?;

        assert_eq!(error.kind(), ErrorKind::NotRunning);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Result { .. }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn metrics_created_machine_returns_conflict_without_touching_the_vmm_socket()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, paths) = fixture()?;
        create_machine(&dispatcher, "idle", MachineSpec::default()).await?;

        let mut events = Vec::new();
        let error = dispatcher
            .run(
                Action::Metrics {
                    name: "idle".to_owned(),
                },
                &mut events,
            )
            .await
            .err()
            .ok_or("metrics on a stopped machine unexpectedly succeeded")?;
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert!(error.message().contains("machine `idle` is not running"));
        assert!(events.is_empty());
        assert!(!paths.machine_api_socket("idle")?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn metrics_missing_machine_returns_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, dispatcher, _paths) = fixture()?;
        let error = dispatcher
            .run(
                Action::Metrics {
                    name: "absent".to_owned(),
                },
                &mut Vec::new(),
            )
            .await
            .err()
            .ok_or("metrics for a missing machine unexpectedly succeeded")?;
        assert_eq!(error.kind(), ErrorKind::NotFound);
        Ok(())
    }
}
