//! Per-machine shim preparation, control protocol, and process supervision.

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
            process::ExitStatusExt,
        },
    },
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(not(any(target_os = "linux", target_os = "android")))]
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    sys::socket::{
        AddressFamily, SockFlag, SockProtocol, SockType, UnixAddr, connect, getsockopt, socket,
        sockopt::SocketError,
    },
    unistd::{getpgrp, getpid, setsid},
};
#[cfg(target_os = "linux")]
use nix::{
    sys::{
        signal::{Signal, killpg},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::ffi::OsStr;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

use crate::{
    Arch, Cmd, DependencyManifest, ErrorInfo, ErrorKind, Event, EventSink, ExitReason,
    FirestoneError, ImageStore, LastExit, MachineLock, MachineSpec, MachineState, MachineStatus,
    ManagedProcess, NetMode, Paths, ProcessSignal, StateStore, StepId, VmConfigInput, VmState,
    VmmApi, atomic, publish_seed, publish_vm_config,
};

const PLAN_VERSION: u32 = 1;
const IDENTITY_VERSION: u32 = 1;
const MAX_CONTROL_REQUEST_BYTES: usize = 4 * 1024;
const MAX_CONTROL_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CONTROL_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_LAUNCH_PLAN_BYTES: u64 = 256 * 1024;
const MAX_PROCESS_IDENTITY_BYTES: u64 = 64 * 1024;
const MAX_VMCONFIG_BYTES: u64 = 51_200;
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STOP_TIMEOUT_SECONDS: u64 = 60 * 60;
const LOOP_INTERVAL: Duration = Duration::from_millis(20);
const CHILD_TERM_GRACE: Duration = Duration::from_secs(5);
const CONTROL_ACCEPT_BACKOFF: Duration = Duration::from_millis(10);
const LOG_TAIL_BYTES: u64 = 64 * 1024;
const LOG_REASON_BYTES: usize = 4096;

/// Bounded timings used by shim preparation and supervision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShimTimeouts {
    pub api: Duration,
    pub readiness: Duration,
    pub control_io: Duration,
    pub launch_request: Duration,
}

impl Default for ShimTimeouts {
    fn default() -> Self {
        Self {
            api: Duration::from_secs(2),
            readiness: Duration::from_secs(10),
            control_io: Duration::from_secs(2),
            launch_request: Duration::from_secs(30),
        }
    }
}

impl ShimTimeouts {
    fn validate(self) -> Result<Self, FirestoneError> {
        for (name, duration) in [
            ("api", self.api),
            ("readiness", self.readiness),
            ("control_io", self.control_io),
            ("launch_request", self.launch_request),
        ] {
            if duration.is_zero() {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    format!("shim {name} timeout must be greater than zero"),
                ));
            }
            if duration > Duration::from_secs(MAX_STOP_TIMEOUT_SECONDS) {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    format!(
                        "shim {name} timeout exceeds the {MAX_STOP_TIMEOUT_SECONDS} second limit"
                    ),
                ));
            }
        }
        Ok(self)
    }
}

/// Durable output of image, seed, VmConfig, and launch-plan preparation.
#[derive(Debug)]
pub struct PreparedStart {
    name: String,
    state: MachineState,
    previous_status: MachineStatus,
}

impl PreparedStart {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn state(&self) -> &MachineState {
        &self.state
    }
}

/// Pids returned by the shim status operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShimPids {
    pub shim: u32,
    pub vmm: Option<u32>,
    pub sidecars: BTreeMap<String, u32>,
}

/// Cross-process status returned by `{"op":"status"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShimStatus {
    pub status: MachineStatus,
    pub pids: ShimPids,
    pub started_at: Option<String>,
    pub degraded: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchPlan {
    version: u32,
    name: String,
    vmm_binary: PathBuf,
    vmm_binary_sha256: String,
    vmm_extra_args: Vec<String>,
    vmconfig_sha256: String,
    vmconfig_len: u64,
    api_timeout_ms: u64,
    readiness_timeout_ms: u64,
    control_io_timeout_ms: u64,
    launch_request_timeout_ms: u64,
}

impl LaunchPlan {
    fn timeouts(&self) -> Result<ShimTimeouts, FirestoneError> {
        let timeouts = ShimTimeouts {
            api: Duration::from_millis(self.api_timeout_ms),
            readiness: Duration::from_millis(self.readiness_timeout_ms),
            control_io: Duration::from_millis(self.control_io_timeout_ms),
            launch_request: Duration::from_millis(self.launch_request_timeout_ms),
        };
        timeouts.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity {
    version: u32,
    shim: ProcessRecord,
    vmm: Option<ProcessRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessRecord {
    pid: u32,
    process_group: u32,
    executable: PathBuf,
    uid: u32,
    start_time_ticks: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ControlRequest {
    Launch,
    Status,
    Stop { timeout_s: u64, force: bool },
    Ping,
}

#[derive(Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    ok: bool,
    error: &'a ErrorInfo,
}

#[derive(Serialize)]
struct StatusResponse<'a> {
    ok: bool,
    status: MachineStatus,
    pids: &'a ShimPids,
    started_at: &'a Option<String>,
    degraded: &'a [String],
}

/// Rejects M2/M3-only process requirements before any M1 start side effects.
pub fn validate_m1_start_scope(spec: &MachineSpec) -> Result<(), FirestoneError> {
    if spec.network.mode != NetMode::None {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!(
                "network.mode `{}` is not runnable by the M1 shim",
                match spec.network.mode {
                    NetMode::Passt => "passt",
                    NetMode::Tap => "tap",
                    NetMode::None => "none",
                }
            ),
        )
        .with_hint("set network.mode to none until M3 networking is available"));
    }
    if !spec.network.forward.is_empty() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            "network forwards are not runnable by the M1 shim",
        )
        .with_hint("remove network.forward entries until M3 networking is available"));
    }
    if !spec.mounts.is_empty() {
        return Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            "shared mounts are not runnable by the M1 shim",
        )
        .with_hint("remove mount entries until M3 virtiofsd support is available"));
    }
    Ok(())
}

/// Composes the approved image, seed, and canonical VmConfig foundations.
///
/// The caller holds the machine lock. This function leaves lifecycle status at
/// its previous value; [`launch_prepared`] performs the `starting` handoff only
/// after every durable input and the private launch plan are ready.
#[allow(clippy::too_many_arguments)]
pub fn prepare_start(
    paths: &Paths,
    image_store: &ImageStore,
    manifest: &DependencyManifest,
    name: &str,
    spec: &MachineSpec,
    mut state: MachineState,
    source_base: &Path,
    lock: &MachineLock,
    events: &mut dyn EventSink,
    timeouts: ShimTimeouts,
) -> Result<PreparedStart, FirestoneError> {
    validate_m1_start_scope(spec)?;
    let timeouts = timeouts.validate()?;
    match state.status {
        MachineStatus::Created | MachineStatus::Stopped | MachineStatus::Failed => {}
        MachineStatus::Starting | MachineStatus::Running | MachineStatus::Stopping => {
            return Err(FirestoneError::new(
                ErrorKind::AlreadyRunning,
                format!("machine `{name}` is already active"),
            )
            .with_hint(format!(
                "use `firestone stop {name}` before starting it again"
            )));
        }
    }
    ensure_no_live_runtime(paths, name, timeouts.control_io.min(timeouts.api))?;
    validate_machine_lock(paths, name, lock)?;
    paths.validate_machine_data_directory(name)?;

    let disk_existed = paths.machine_disk(name)?.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect machine `{name}` overlay"),
            source,
        )
    })?;
    let prepared_image = image_store.prepare_machine_image(
        name,
        &mut state,
        source_base,
        spec.disk,
        lock,
        events,
    )?;
    if disk_existed {
        events.emit(Event::StepSkip {
            id: StepId::from("disk"),
            reason: "exists".to_owned(),
        })?;
    } else {
        events.emit(Event::StepDone {
            id: StepId::from("disk"),
            detail: Some(format!("{} overlay", spec.disk)),
            elapsed_ms: 0,
        })?;
    }

    events.emit(Event::StepStart {
        id: StepId::from("seed"),
        label: "render cloud-init seed".to_owned(),
    })?;
    let rendered = publish_seed(paths, name, spec)?;
    state.instance_id = Some(rendered.instance_id.clone());
    if state.mac.is_none() {
        state.mac = Some(allocated_mac(paths, name));
    }
    events.emit(Event::StepDone {
        id: StepId::from("seed"),
        detail: Some(format!("instance {}", rendered.instance_id)),
        elapsed_ms: 0,
    })?;

    let architecture = image_store.architecture();
    let config = publish_vm_config(
        paths,
        manifest,
        VmConfigInput {
            name,
            spec,
            state: &state,
            architecture,
            catalog_firmware: prepared_image.image.firmware,
        },
    )?;
    let (vmm_binary, vmm_binary_sha256) = resolve_vmm_binary(paths, manifest, architecture, spec)?;
    let plan = LaunchPlan {
        version: PLAN_VERSION,
        name: name.to_owned(),
        vmm_binary,
        vmm_binary_sha256,
        vmm_extra_args: spec.vmm.extra_args.clone(),
        vmconfig_sha256: sha256_hex(config.as_bytes()),
        vmconfig_len: u64::try_from(config.as_bytes().len()).map_err(|_| {
            FirestoneError::new(
                ErrorKind::InvalidSpec,
                "canonical VmConfig length overflowed u64",
            )
        })?,
        api_timeout_ms: duration_millis(timeouts.api, "api")?,
        readiness_timeout_ms: duration_millis(timeouts.readiness, "readiness")?,
        control_io_timeout_ms: duration_millis(timeouts.control_io, "control_io")?,
        launch_request_timeout_ms: duration_millis(timeouts.launch_request, "launch_request")?,
    };
    let state_path = paths.machine_state(name)?;
    paths.ensure_machine_runtime_dir(name)?;
    paths.clear_machine_runtime_dir(name, false)?;
    if let Err(error) = publish_launch_plan(paths, name, &plan) {
        let _ = paths.clear_machine_runtime_dir(name, true);
        return Err(error);
    }
    let previous_status = state.status;
    if let Err(error) = StateStore::new(state_path).write_from_locked_action(&state, lock) {
        let _ = paths.clear_machine_runtime_dir(name, true);
        return Err(error);
    }
    Ok(PreparedStart {
        name: name.to_owned(),
        state,
        previous_status,
    })
}

/// Spawns `firestone _shim NAME`, hands state ownership to it, and launches.
///
/// The supplied lock is consumed. It protects the two `starting` writes and is
/// released only after the shim pid is durable; the shim then acquires and owns
/// the same lock until its one final state write.
pub fn launch_prepared(
    paths: &Paths,
    shim_program: &Path,
    mut prepared: PreparedStart,
    lock: MachineLock,
    events: &mut dyn EventSink,
) -> Result<ShimStatus, FirestoneError> {
    validate_machine_lock(paths, &prepared.name, &lock)?;
    let plan = match read_launch_plan(paths, &prepared.name) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = paths.clear_machine_runtime_dir(&prepared.name, true);
            return Err(error);
        }
    };
    let plan_timeouts = plan.timeouts()?;
    let shim_program = match validate_shim_program(shim_program) {
        Ok(program) => program,
        Err(error) => {
            rollback_before_shim(paths, &mut prepared, &lock, &error)?;
            return Err(error);
        }
    };
    let shim_log = paths.machine_shim_log(&prepared.name)?;
    prepared.state.status = MachineStatus::Starting;
    prepared.state.shim_pid = None;
    prepared.state.vmm_pid = None;
    prepared.state.sidecar_pids.clear();
    prepared.state.degraded.clear();
    prepared.state.started_at = Some(now_timestamp());
    let state_store = StateStore::new(paths.machine_state(&prepared.name)?);
    if let Err(error) = state_store.write_from_locked_action(&prepared.state, &lock) {
        let _ = paths.clear_machine_runtime_dir(&prepared.name, true);
        return Err(error);
    }

    if let Err(error) = events.emit(Event::StepStart {
        id: StepId::from("shim"),
        label: "start machine supervisor".to_owned(),
    }) {
        rollback_before_shim(paths, &mut prepared, &lock, &error)?;
        return Err(error);
    }
    let command = shim_command(paths, &shim_program, &prepared.name, &shim_log);
    let mut process = match command.spawn_session_candidate() {
        Ok(process) => process,
        Err(error) => {
            rollback_before_shim(paths, &mut prepared, &lock, &error)?;
            return Err(error);
        }
    };
    let shim_pid = process.id();
    prepared.state.shim_pid = Some(shim_pid);
    if let Err(error) = state_store.write_from_locked_action(&prepared.state, &lock) {
        let _ = process.signal_process(ProcessSignal::Kill);
        let _ = process.wait();
        rollback_before_shim(paths, &mut prepared, &lock, &error)?;
        return Err(error);
    }
    drop(lock);

    let socket = paths.machine_shim_socket(&prepared.name)?;
    let wait_deadline = Instant::now()
        .checked_add(plan_timeouts.launch_request)
        .ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "shim launch deadline is out of range")
        })?;
    if let Err(error) = wait_for_shim_socket(&socket, &mut process, wait_deadline) {
        terminate_unready_shim(paths, &prepared.name, &mut process, &prepared.state, &error)?;
        return Err(error);
    }
    if let Err(error) = process.confirm_session() {
        terminate_unready_shim(paths, &prepared.name, &mut process, &prepared.state, &error)?;
        return Err(error);
    }
    let _ = events.emit(Event::StepDone {
        id: StepId::from("shim"),
        detail: Some(format!("pid {shim_pid}")),
        elapsed_ms: 0,
    });

    let client = ShimClient::new(socket, plan_timeouts.readiness + plan_timeouts.control_io);
    client.launch(events)?;
    let status = ShimClient::new(
        paths.machine_shim_socket(&prepared.name)?,
        plan_timeouts.control_io,
    )
    .status()?;
    drop(process);
    Ok(status)
}

/// Bounded client for the private per-machine shim socket.
#[derive(Debug, Clone)]
pub struct ShimClient {
    socket: PathBuf,
    timeout: Duration,
}

impl ShimClient {
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket: socket.into(),
            timeout,
        }
    }

    pub fn ping(&self) -> Result<(), FirestoneError> {
        let terminal = self.request(br#"{"op":"ping"}"#, None, self.timeout)?;
        require_ok_terminal(&terminal)
    }

    pub fn launch(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let terminal = self.request(br#"{"op":"launch"}"#, Some(events), self.timeout)?;
        require_ok_terminal(&terminal)
    }

    pub fn status(&self) -> Result<ShimStatus, FirestoneError> {
        let terminal = self.request(br#"{"op":"status"}"#, None, self.timeout)?;
        if terminal.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(error_from_terminal(&terminal)?);
        }
        serde_json::from_value(Value::Object(
            terminal
                .as_object()
                .cloned()
                .ok_or_else(|| protocol_error("shim status response is not an object"))?,
        ))
        .map(|response: OwnedStatusResponse| response.into_status())
        .map_err(|source| protocol_error_with_source("invalid shim status response", source))
    }

    pub fn stop(
        &self,
        timeout: Duration,
        force: bool,
        events: &mut dyn EventSink,
    ) -> Result<(), FirestoneError> {
        let timeout_s = timeout.as_secs();
        if timeout_s > MAX_STOP_TIMEOUT_SECONDS {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                format!("stop timeout exceeds the {MAX_STOP_TIMEOUT_SECONDS} second limit"),
            ));
        }
        let request = serde_json::to_vec(&serde_json::json!({
            "op": "stop",
            "timeout_s": timeout_s,
            "force": force,
        }))
        .map_err(|source| {
            FirestoneError::new(ErrorKind::Generic, "cannot encode shim stop request")
                .with_source(source)
        })?;
        let total = timeout
            .checked_add(CHILD_TERM_GRACE)
            .and_then(|value| value.checked_add(self.timeout))
            .ok_or_else(|| {
                FirestoneError::new(ErrorKind::Usage, "stop deadline is out of range")
            })?;
        let terminal = self.request(&request, Some(events), total)?;
        require_ok_terminal(&terminal)
    }

    fn request(
        &self,
        request: &[u8],
        mut events: Option<&mut dyn EventSink>,
        timeout: Duration,
    ) -> Result<Value, FirestoneError> {
        if request.len() > MAX_CONTROL_REQUEST_BYTES {
            return Err(protocol_error("shim request exceeds the 4096 byte limit"));
        }
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "shim request deadline is out of range")
        })?;
        let mut stream = connect_control_socket(&self.socket, deadline)?;
        authorize_peer(&stream, nix::unistd::getuid().as_raw())?;
        write_frame(&mut stream, request, deadline)?;
        let mut total = 0_usize;
        loop {
            let frame = read_frame(&mut stream, MAX_CONTROL_RESPONSE_BYTES, deadline)?;
            total = total
                .checked_add(frame.len())
                .ok_or_else(|| protocol_error("shim response stream length overflowed usize"))?;
            if total > MAX_CONTROL_STREAM_BYTES {
                return Err(protocol_error(
                    "shim response stream exceeds the 4 MiB limit",
                ));
            }
            let value: Value = serde_json::from_slice(&frame).map_err(|source| {
                protocol_error_with_source("shim response is not valid JSON", source)
            })?;
            if value.get("type").is_some() {
                let event = serde_json::from_value::<Event>(value).map_err(|source| {
                    protocol_error_with_source(
                        "shim event does not match the Event contract",
                        source,
                    )
                })?;
                if let Some(sink) = events.as_mut() {
                    sink.emit(event)?;
                }
                continue;
            }
            return Ok(value);
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedStatusResponse {
    ok: bool,
    status: MachineStatus,
    pids: ShimPids,
    started_at: Option<String>,
    degraded: Vec<String>,
}

impl OwnedStatusResponse {
    fn into_status(self) -> ShimStatus {
        let _ = self.ok;
        ShimStatus {
            status: self.status,
            pids: self.pids,
            started_at: self.started_at,
            degraded: self.degraded,
        }
    }
}

/// Runs the dedicated shim process until stop, signal, or VMM exit.
pub fn run_shim(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    enter_shim_session()?;
    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_child_subreaper(true).map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot become a Linux child subreaper")
            .with_source(io::Error::from_raw_os_error(source as i32))
    })?;
    let terminating = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&terminating)).map_err(
        |source| {
            FirestoneError::new(ErrorKind::Generic, "cannot install shim SIGINT handler")
                .with_source(source)
        },
    )?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&terminating)).map_err(
        |source| {
            FirestoneError::new(ErrorKind::Generic, "cannot install shim SIGTERM handler")
                .with_source(source)
        },
    )?;

    paths.validate_machine_data_directory(name)?;
    paths.ensure_machine_runtime_dir(name)?;
    let mut lock_events = Vec::new();
    let lock = MachineLock::acquire(name, &paths.machine_lock(name)?, &mut lock_events)?;
    let mut state = StateStore::new(paths.machine_state(name)?).read()?;
    let pid = std::process::id();
    if state.status != MachineStatus::Starting || state.shim_pid != Some(pid) {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("shim pid {pid} does not own the starting state for machine `{name}`"),
        )
        .with_hint("start the shim through the lifecycle preparation API"));
    }
    let plan = read_launch_plan(paths, name)?;
    if plan.name != name {
        return Err(protocol_error(
            "launch plan machine name does not match shim argv",
        ));
    }
    let timeouts = plan.timeouts()?;
    let shim_executable = current_executable()?;
    let shim_record = process_record(pid, pid, shim_executable)?;
    let mut identity = ProcessIdentity {
        version: IDENTITY_VERSION,
        shim: shim_record,
        vmm: None,
    };
    publish_pid_and_identity(paths, name, &identity)?;
    let listener = bind_control_socket(paths, name)?;
    write_shim_log(&format!("shim `{name}` ready as pid {pid}"));
    listener.set_nonblocking(true).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot make machine `{name}` shim socket nonblocking"),
            source,
        )
    })?;

    let launch_deadline = Instant::now()
        .checked_add(timeouts.launch_request)
        .ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "shim launch deadline is out of range")
        })?;
    let mut launched = false;
    let mut vmm: Option<ManagedProcess> = None;
    loop {
        if terminating.load(Ordering::Relaxed) {
            let reason = if launched {
                "shim received a termination signal"
            } else {
                "shim terminated before launch"
            };
            if let Some(process) = vmm.as_mut() {
                let mut sink = Vec::new();
                if let Err(error) = stop_owned_vmm(
                    paths,
                    name,
                    &plan,
                    &mut state,
                    &mut identity,
                    process,
                    Duration::from_secs(30),
                    false,
                    &mut sink,
                ) {
                    if let Some(record) = identity.vmm.clone() {
                        let _ = terminate_process(&record, process);
                    }
                    write_final_state(
                        paths,
                        name,
                        &mut state,
                        MachineStatus::Failed,
                        None,
                        ExitReason::Failure(error.message().to_owned()),
                    )?;
                }
                merge_console_log(paths, name)?;
            } else {
                write_final_state(
                    paths,
                    name,
                    &mut state,
                    MachineStatus::Failed,
                    None,
                    ExitReason::Failure(reason.to_owned()),
                )?;
            }
            cleanup_after_shim(paths, name)?;
            drop(lock);
            return Ok(());
        }

        if !launched && Instant::now() >= launch_deadline {
            let error = FirestoneError::new(
                ErrorKind::Timeout,
                format!("shim for machine `{name}` received no launch request before its deadline"),
            );
            write_final_state(
                paths,
                name,
                &mut state,
                MachineStatus::Failed,
                None,
                ExitReason::Failure(error.message().to_owned()),
            )?;
            cleanup_after_shim(paths, name)?;
            drop(lock);
            return Err(error);
        }

        if let Some(process) = vmm.as_mut()
            && let Some(status) = process.try_wait()?
        {
            let _ = process.signal_group(ProcessSignal::Kill);
            reap_adopted_children();
            write_shim_log(&format!("machine `{name}` VMM exited unexpectedly"));
            let reason = vmm_failure_reason(paths, name, "VMM exited unexpectedly")?;
            write_final_state(
                paths,
                name,
                &mut state,
                MachineStatus::Failed,
                Some(status),
                ExitReason::Failure(reason),
            )?;
            merge_console_log(paths, name)?;
            cleanup_after_shim(paths, name)?;
            drop(lock);
            return Ok(());
        }

        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(source) = stream.set_nonblocking(true) {
                    let error = filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot make machine `{name}` control connection nonblocking"),
                        source,
                    );
                    let _ = write_terminal_error(stream, &error, timeouts.control_io);
                    continue;
                }
                if let Err(error) = authorize_peer(&stream, paths.uid()) {
                    let _ = write_terminal_error(stream, &error, timeouts.control_io);
                    continue;
                }
                let request = match read_request(&stream, timeouts.control_io) {
                    Ok(request) => request,
                    Err(error) => {
                        let _ = write_terminal_error(stream, &error, timeouts.control_io);
                        continue;
                    }
                };
                match request {
                    ControlRequest::Ping => {
                        write_terminal_ok(stream, timeouts.control_io)?;
                    }
                    ControlRequest::Status => {
                        let pids = status_pids(&state, pid);
                        write_status(stream, &state, &pids, timeouts.control_io)?;
                    }
                    ControlRequest::Launch if launched => {
                        let error = FirestoneError::new(
                            ErrorKind::AlreadyRunning,
                            format!("machine `{name}` launch was already requested"),
                        );
                        write_terminal_error(stream, &error, timeouts.control_io)?;
                    }
                    ControlRequest::Launch => {
                        launched = true;
                        let mut sink = ProtocolSink::new(stream, timeouts.control_io);
                        match launch_vmm(
                            paths,
                            name,
                            &plan,
                            &mut state,
                            &mut identity,
                            &terminating,
                            &mut sink,
                        ) {
                            Ok(process) => {
                                sink.terminal_ok();
                                vmm = Some(process);
                            }
                            Err(error) => {
                                let reason = vmm_failure_reason(paths, name, error.message())?;
                                write_final_state(
                                    paths,
                                    name,
                                    &mut state,
                                    MachineStatus::Failed,
                                    None,
                                    ExitReason::Failure(reason),
                                )?;
                                merge_console_log(paths, name)?;
                                cleanup_after_shim(paths, name)?;
                                drop(lock);
                                sink.terminal_error(&error);
                                return Err(error);
                            }
                        }
                    }
                    ControlRequest::Stop { timeout_s, force } => {
                        if timeout_s > MAX_STOP_TIMEOUT_SECONDS {
                            let error = FirestoneError::new(
                                ErrorKind::Usage,
                                format!(
                                    "stop timeout exceeds the {MAX_STOP_TIMEOUT_SECONDS} second limit"
                                ),
                            );
                            write_terminal_error(stream, &error, timeouts.control_io)?;
                            continue;
                        }
                        let mut sink = ProtocolSink::new(stream, timeouts.control_io);
                        let result = if let Some(process) = vmm.as_mut() {
                            stop_owned_vmm(
                                paths,
                                name,
                                &plan,
                                &mut state,
                                &mut identity,
                                process,
                                Duration::from_secs(timeout_s),
                                force,
                                &mut sink,
                            )
                        } else {
                            write_final_state(
                                paths,
                                name,
                                &mut state,
                                MachineStatus::Stopped,
                                None,
                                ExitReason::GuestShutdown,
                            )
                        };
                        match result {
                            Ok(()) => {
                                merge_console_log(paths, name)?;
                                cleanup_after_shim(paths, name)?;
                                drop(lock);
                                sink.terminal_ok();
                                return Ok(());
                            }
                            Err(error) => {
                                if vmm.as_mut().is_some_and(|process| {
                                    process.try_wait().ok().flatten().is_none()
                                }) {
                                    state.status = MachineStatus::Running;
                                    StateStore::new(paths.machine_state(name)?)
                                        .write_from_shim(&state)?;
                                }
                                sink.terminal_error(&error);
                            }
                        }
                    }
                }
            }
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(CONTROL_ACCEPT_BACKOFF);
            }
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot accept machine `{name}` shim control connection"),
                    source,
                ));
            }
        }
    }
}

/// Stops a VMM left running after its shim died.
///
/// Linux verifies executable, argv, process group, and `/proc` start time before
/// every signal. Other systems may still complete the API-only graceful path,
/// but refuse signal escalation because they lack the required stable evidence.
pub fn stop_unsupervised(
    paths: &Paths,
    name: &str,
    mut state: MachineState,
    lock: &MachineLock,
    timeout: Duration,
    force: bool,
    events: &mut dyn EventSink,
) -> Result<MachineState, FirestoneError> {
    validate_machine_lock(paths, name, lock)?;
    if let Some(shim_pid) = state.shim_pid {
        #[cfg(target_os = "linux")]
        {
            if crate::verify_shim_identity(Path::new("/proc"), shim_pid, name)? {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("machine `{name}` still has a verified live shim"),
                )
                .with_hint("use the shim control socket while the supervisor is alive"));
            }
            state.shim_pid = None;
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = shim_pid;
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` still records a shim pid"),
            )
            .with_hint("unsupervised stop signal recovery requires Linux process identity"));
        }
    }
    let plan = read_launch_plan(paths, name)?;
    let identity = read_process_identity(paths, name)?;
    let record = identity.vmm.ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` has no recorded VMM identity"),
        )
        .with_hint("refuse signal escalation without a verified process identity")
    })?;
    if state.vmm_pid != Some(record.pid) {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` VMM pid does not match its process identity"),
        ));
    }
    state.status = MachineStatus::Stopping;
    StateStore::new(paths.machine_state(name)?).write_from_locked_action(&state, lock)?;
    events.emit(Event::StepStart {
        id: StepId::from("stop"),
        label: if force {
            "force stop VMM".to_owned()
        } else {
            "ACPI power button".to_owned()
        },
    })?;

    let api_socket = paths.machine_api_socket(name)?;
    let api = VmmApi::new(&api_socket, plan.timeouts()?.api);
    let mut status = None;
    let mut reason = ExitReason::GuestShutdown;
    if force {
        signal_verified_group(&record, ProcessSignal::Kill)?;
        reason = ExitReason::Failure("forced stop".to_owned());
    } else {
        let graceful = api.vm_power_button();
        if graceful.is_ok() {
            let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
                FirestoneError::new(ErrorKind::Usage, "stop deadline is out of range")
            })?;
            while Instant::now() < deadline {
                if !verified_process_alive(&record)? {
                    break;
                }
                match api.vm_info() {
                    Ok(info) if info.state == VmState::Shutdown => {
                        let _ = api.vmm_shutdown();
                        break;
                    }
                    Ok(_) => thread::sleep(LOOP_INTERVAL),
                    Err(_) if !verified_process_alive(&record)? => break,
                    Err(_) => break,
                }
            }
        }
        if verified_process_alive(&record)? {
            signal_verified_group(&record, ProcessSignal::Terminate)?;
            let deadline = Instant::now() + CHILD_TERM_GRACE;
            while Instant::now() < deadline && verified_process_alive(&record)? {
                thread::sleep(LOOP_INTERVAL);
            }
            reason = ExitReason::Failure(if graceful.is_err() {
                "VMM API failed during graceful stop".to_owned()
            } else {
                "graceful stop timed out".to_owned()
            });
        }
        if verified_process_alive(&record)? {
            signal_verified_group(&record, ProcessSignal::Kill)?;
        }
    }
    wait_for_record_exit(&record, CHILD_TERM_GRACE)?;
    state.status = MachineStatus::Stopped;
    state.shim_pid = None;
    state.vmm_pid = None;
    state.sidecar_pids.clear();
    state.started_at = None;
    state.degraded.clear();
    state.last_exit = Some(last_exit(status.take(), reason));
    StateStore::new(paths.machine_state(name)?).write_from_locked_action(&state, lock)?;
    events.emit(Event::StepDone {
        id: StepId::from("stop"),
        detail: Some(
            state
                .last_exit
                .as_ref()
                .map_or("stopped", |exit| exit.reason.as_str())
                .to_owned(),
        ),
        elapsed_ms: 0,
    })?;
    merge_console_log(paths, name)?;
    paths.clear_machine_runtime_dir(name, true)?;
    Ok(state)
}

fn launch_vmm(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    terminating: &AtomicBool,
    events: &mut dyn EventSink,
) -> Result<ManagedProcess, FirestoneError> {
    events.emit(Event::StepSkip {
        id: StepId::from("net"),
        reason: "network.mode is none".to_owned(),
    })?;
    events.emit(Event::StepSkip {
        id: StepId::from("fs"),
        reason: "no mounts".to_owned(),
    })?;
    write_shim_log(&format!("machine `{name}` launching cloud-hypervisor"));
    events.emit(Event::StepStart {
        id: StepId::from("vmm"),
        label: "start cloud-hypervisor".to_owned(),
    })?;
    validate_plan_executable(plan)?;
    let vmconfig = read_exact_vmconfig(paths, name, plan)?;
    rotate_console_log(paths, name)?;
    let vmm_log = paths.machine_vmm_log(name)?;
    let api_socket = paths.machine_api_socket(name)?;
    let command = vmm_environment(
        Cmd::new(plan.vmm_binary.as_os_str())
            .arg("--api-socket")
            .arg(api_socket.as_os_str())
            .arg("--log-file")
            .arg(vmm_log.as_os_str())
            .args(plan.vmm_extra_args.iter().map(String::as_str))
            .cwd("/")
            .stdin_null()
            .stdout_append(&vmm_log)
            .stderr_append(&vmm_log)
            .error_kind(ErrorKind::Dependency),
    );
    let mut process = command.spawn_process_group()?;
    let record = match process_record(
        process.id(),
        process.process_group().unwrap_or(process.id()),
        plan.vmm_binary.clone(),
    ) {
        Ok(record) => record,
        Err(error) => {
            let _ = process.signal_group(ProcessSignal::Kill);
            let _ = process.wait();
            return Err(error);
        }
    };
    identity.vmm = Some(record.clone());
    publish_process_identity(paths, name, identity)?;
    state.vmm_pid = Some(process.id());
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;

    let timeouts = plan.timeouts()?;
    let readiness_deadline = Instant::now()
        .checked_add(timeouts.readiness)
        .ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "VMM readiness deadline is out of range")
        })?;
    let ping = loop {
        if terminating.load(Ordering::Relaxed) {
            terminate_process(&record, &mut process)?;
            return Err(FirestoneError::new(
                ErrorKind::Interrupted,
                format!("machine `{name}` launch interrupted by signal"),
            ));
        }
        if let Some(status) = process.try_wait()? {
            let reason = status_description(status);
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("cloud-hypervisor exited before API readiness: {reason}"),
            ));
        }
        if Instant::now() >= readiness_deadline {
            terminate_process(&record, &mut process)?;
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "cloud-hypervisor API for machine `{name}` was not ready within {} ms",
                    timeouts.readiness.as_millis()
                ),
            )
            .with_hint(format!("inspect {}", vmm_log.display())));
        }
        let remaining = readiness_deadline.saturating_duration_since(Instant::now());
        let api_timeout = timeouts.api.min(remaining);
        match VmmApi::new(&api_socket, api_timeout).vmm_ping() {
            Ok(ping) => break ping,
            Err(error) if matches!(error.kind(), ErrorKind::NotRunning | ErrorKind::Timeout) => {
                thread::sleep(LOOP_INTERVAL.min(remaining));
            }
            Err(error) => {
                terminate_process(&record, &mut process)?;
                return Err(error);
            }
        }
    };
    if ping.pid != i64::from(process.id()) {
        terminate_process(&record, &mut process)?;
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "VMM API pid {} does not match spawned cloud-hypervisor pid {}",
                ping.pid,
                process.id()
            ),
        ));
    }

    let api = VmmApi::new(&api_socket, timeouts.api);
    if let Err(error) = api.vm_create(&vmconfig) {
        terminate_process(&record, &mut process)?;
        return Err(error);
    }
    secure_console_log(paths, name)?;
    if let Err(error) = api.vm_boot() {
        let _ = api.vmm_shutdown();
        terminate_process(&record, &mut process)?;
        return Err(error);
    }
    state.status = MachineStatus::Running;
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    write_shim_log(&format!("machine `{name}` is running"));
    events.emit(Event::StepDone {
        id: StepId::from("vmm"),
        detail: Some(format!("cloud-hypervisor {}", ping.build_version)),
        elapsed_ms: 0,
    })?;
    Ok(process)
}

#[allow(clippy::too_many_arguments)]
fn stop_owned_vmm(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    process: &mut ManagedProcess,
    timeout: Duration,
    force: bool,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    state.status = MachineStatus::Stopping;
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    events.emit(Event::StepStart {
        id: StepId::from("stop"),
        label: if force {
            "force stop VMM".to_owned()
        } else {
            "ACPI power button".to_owned()
        },
    })?;
    write_shim_log(&format!("machine `{name}` stopping; force={force}"));
    let record = identity.vmm.as_ref().ok_or_else(|| {
        FirestoneError::new(ErrorKind::Conflict, "running VMM has no process identity")
    })?;
    verify_owned_process(record, process)?;
    let api_socket = paths.machine_api_socket(name)?;
    let api = VmmApi::new(&api_socket, plan.timeouts()?.api);
    let mut status = None;
    let reason;

    if force {
        verify_owned_process(record, process)?;
        process.signal_group(ProcessSignal::Kill)?;
        status = process.wait_timeout(CHILD_TERM_GRACE)?;
        reason = ExitReason::Failure("forced stop".to_owned());
    } else {
        let power_result = api.vm_power_button();
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "stop deadline is out of range")
        })?;
        let mut vm_shutdown_observed = false;
        let mut escalated = false;
        if power_result.is_ok() {
            while Instant::now() < deadline {
                if let Some(exit) = process.try_wait()? {
                    status = Some(exit);
                    break;
                }
                match api.vm_info() {
                    Ok(info) if info.state == VmState::Shutdown => {
                        vm_shutdown_observed = true;
                        break;
                    }
                    Ok(_) => thread::sleep(LOOP_INTERVAL),
                    Err(_) => {
                        if let Some(exit) = process.try_wait()? {
                            status = Some(exit);
                            break;
                        }
                        thread::sleep(LOOP_INTERVAL);
                    }
                }
            }
        }
        if status.is_none() && vm_shutdown_observed {
            let _ = api.vmm_shutdown();
            status = process.wait_timeout(plan.timeouts()?.api + LOOP_INTERVAL)?;
        }
        if status.is_none() {
            escalated = true;
            events.emit(Event::StepUpdate {
                id: StepId::from("stop"),
                detail: "graceful stop did not complete; sending SIGTERM".to_owned(),
            })?;
            verify_owned_process(record, process)?;
            process.signal_group(ProcessSignal::Terminate)?;
            status = process.wait_timeout(CHILD_TERM_GRACE)?;
        }
        if status.is_none() {
            events.emit(Event::StepUpdate {
                id: StepId::from("stop"),
                detail: "SIGTERM grace expired; sending SIGKILL".to_owned(),
            })?;
            verify_owned_process(record, process)?;
            process.signal_group(ProcessSignal::Kill)?;
            status = process.wait_timeout(CHILD_TERM_GRACE)?;
        }
        reason = if power_result.is_ok() && status.is_some() && !escalated {
            ExitReason::GuestShutdown
        } else if power_result.is_err() {
            ExitReason::Failure("VMM API failed during graceful stop".to_owned())
        } else if vm_shutdown_observed {
            ExitReason::GuestShutdown
        } else {
            ExitReason::Failure("graceful stop timed out".to_owned())
        };
    }

    if status.is_none() {
        return Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!("cannot reap cloud-hypervisor for machine `{name}` after SIGKILL"),
        ));
    }
    let _ = process.signal_group(ProcessSignal::Kill);
    reap_adopted_children();
    identity.vmm = None;
    write_final_state(paths, name, state, MachineStatus::Stopped, status, reason)?;
    events.emit(Event::StepDone {
        id: StepId::from("stop"),
        detail: Some(
            state
                .last_exit
                .as_ref()
                .map_or("stopped", |exit| exit.reason.as_str())
                .to_owned(),
        ),
        elapsed_ms: 0,
    })?;
    Ok(())
}

fn write_final_state(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    status: MachineStatus,
    process_status: Option<ExitStatus>,
    reason: ExitReason,
) -> Result<(), FirestoneError> {
    state.status = status;
    state.shim_pid = None;
    state.vmm_pid = None;
    state.sidecar_pids.clear();
    state.started_at = None;
    state.degraded.clear();
    state.last_exit = Some(last_exit(process_status, reason));
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)
}

fn last_exit(status: Option<ExitStatus>, reason: ExitReason) -> LastExit {
    LastExit {
        at: now_timestamp(),
        code: status.and_then(|value| value.code()),
        signal: status.and_then(|value| value.signal()),
        reason,
    }
}

fn ensure_no_live_runtime(
    paths: &Paths,
    name: &str,
    timeout: Duration,
) -> Result<(), FirestoneError> {
    let runtime = paths.machine_runtime_dir(name)?;
    if !runtime.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect machine `{name}` runtime directory"),
            source,
        )
    })? {
        return Ok(());
    }
    let shim_socket = paths.machine_shim_socket(name)?;
    match ShimClient::new(&shim_socket, timeout).ping() {
        Ok(()) => {
            return Err(FirestoneError::new(
                ErrorKind::AlreadyRunning,
                format!("machine `{name}` already has a live shim"),
            ));
        }
        Err(error) if matches!(error.kind(), ErrorKind::NotRunning | ErrorKind::Timeout) => {}
        Err(error) => {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("cannot prove stale shim socket {}", shim_socket.display()),
            )
            .with_hint("inspect the existing runtime directory before retrying")
            .with_source(error));
        }
    }
    let api_socket = paths.machine_api_socket(name)?;
    match VmmApi::new(&api_socket, timeout).vmm_ping() {
        Ok(_) => Err(FirestoneError::new(
            ErrorKind::AlreadyRunning,
            format!("machine `{name}` already has a live VMM"),
        )),
        Err(error) if matches!(error.kind(), ErrorKind::NotRunning | ErrorKind::Timeout) => Ok(()),
        Err(error) => Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("cannot prove stale VMM socket {}", api_socket.display()),
        )
        .with_hint("inspect the existing runtime directory before retrying")
        .with_source(error)),
    }
}

fn validate_machine_lock(
    paths: &Paths,
    name: &str,
    lock: &MachineLock,
) -> Result<(), FirestoneError> {
    let expected = paths.machine_lock(name)?;
    if lock.path() == expected {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "machine `{name}` preparation requires lock {}",
                expected.display()
            ),
        ))
    }
}

fn allocated_mac(paths: &Paths, name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"firestone-machine-mac-v1\0");
    hasher.update(paths.data_dir().as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        digest[0], digest[1], digest[2]
    )
}

fn resolve_vmm_binary(
    paths: &Paths,
    manifest: &DependencyManifest,
    architecture: Arch,
    spec: &MachineSpec,
) -> Result<(PathBuf, String), FirestoneError> {
    if let Some(binary) = &spec.vmm.binary {
        let canonical = fs::canonicalize(binary).map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot resolve VMM binary {}", binary.display()),
                source,
            )
        })?;
        validate_executable(&canonical, None)?;
        let digest = hash_file(&canonical, MAX_EXECUTABLE_BYTES, "VMM binary")?;
        return Ok((canonical, digest));
    }

    let artifact = manifest.artifact("cloud-hypervisor", architecture.as_str())?;
    let binary = paths.binary_file(&artifact.install_name)?;
    paths.validate_bin_data_directory()?;
    paths.validate_owned_data_file(&binary, "cloud-hypervisor binary", 0o755, false)?;
    validate_executable(&binary, Some(paths.uid()))?;
    let digest = hash_file(&binary, MAX_EXECUTABLE_BYTES, "cloud-hypervisor binary")?;
    if digest != artifact.sha256 {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "cloud-hypervisor binary {} does not match the pinned {} checksum",
                binary.display(),
                artifact.version
            ),
        )
        .with_hint("run `firestone doctor --fix` to reinstall the pinned VMM"));
    }
    Ok((binary, digest))
}

fn validate_plan_executable(plan: &LaunchPlan) -> Result<(), FirestoneError> {
    validate_executable(&plan.vmm_binary, None)?;
    let digest = hash_file(&plan.vmm_binary, MAX_EXECUTABLE_BYTES, "VMM binary")?;
    if digest == plan.vmm_binary_sha256 {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Checksum,
            format!(
                "VMM binary {} changed after start preparation",
                plan.vmm_binary.display()
            ),
        )
        .with_hint("rerun start after restoring the selected VMM binary"))
    }
}

fn validate_executable(path: &Path, expected_uid: Option<u32>) -> Result<(), FirestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect executable {}", path.display()),
            source,
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "executable {} is not a regular non-symlink file",
                path.display()
            ),
        ));
    }
    if metadata.mode() & 0o111 == 0 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("executable {} has no execute bit", path.display()),
        ));
    }
    if expected_uid.is_some_and(|uid| metadata.uid() != uid) {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "executable {} is not owned by the Firestone user",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_shim_program(path: &Path) -> Result<PathBuf, FirestoneError> {
    if !path.is_absolute() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("shim executable {} is not absolute", path.display()),
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot resolve shim executable {}", path.display()),
            source,
        )
    })?;
    validate_executable(&canonical, None)?;
    Ok(canonical)
}

fn shim_command(paths: &Paths, program: &Path, name: &str, log: &Path) -> Cmd {
    reduced_environment(
        Cmd::new(program.as_os_str())
            .arg("_shim")
            .arg(name)
            .cwd("/")
            .stdin_null()
            .stdout_append(log)
            .stderr_append(log)
            .error_kind(ErrorKind::Dependency),
    )
    .env("FIRESTONE_CONFIG_DIR", paths.config_dir().as_os_str())
    .env("FIRESTONE_DATA_DIR", paths.data_dir().as_os_str())
    .env("FIRESTONE_RUNTIME_DIR", paths.runtime_dir().as_os_str())
}

fn reduced_environment(mut command: Cmd) -> Cmd {
    command = command.env_clear();
    for key in [
        "PATH",
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        if let Some(value) = env::var_os(key).filter(|value| !value.is_empty()) {
            command = command.env(key, value);
        }
    }
    for (key, value) in env::vars_os() {
        if key.to_string_lossy().starts_with("FIRESTONE_") {
            command = command.env(key, value);
        }
    }
    command
}
fn vmm_environment(mut command: Cmd) -> Cmd {
    command = command.env_clear();
    for key in ["PATH", "HOME"] {
        if let Some(value) = env::var_os(key).filter(|value| !value.is_empty()) {
            command = command.env(key, value);
        }
    }
    command
}

fn publish_launch_plan(paths: &Paths, name: &str, plan: &LaunchPlan) -> Result<(), FirestoneError> {
    paths.validate_machine_runtime_dir(name)?;
    atomic::write_json_with_mode(&paths.machine_shim_plan(name)?, plan, 0o600)
}

fn read_launch_plan(paths: &Paths, name: &str) -> Result<LaunchPlan, FirestoneError> {
    paths.validate_machine_runtime_dir(name)?;
    let path = paths.machine_shim_plan(name)?;
    let bytes = read_private_file(
        &path,
        paths.uid(),
        0o600,
        MAX_LAUNCH_PLAN_BYTES,
        "shim launch plan",
    )?;
    let plan: LaunchPlan = serde_json::from_slice(&bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot parse shim launch plan {}", path.display()),
        )
        .with_hint("rerun start to replace the stale launch plan")
        .with_source(source)
    })?;
    if plan.version != PLAN_VERSION {
        return Err(protocol_error("unsupported shim launch plan version"));
    }
    plan.timeouts()?;
    Ok(plan)
}

fn publish_pid_and_identity(
    paths: &Paths,
    name: &str,
    identity: &ProcessIdentity,
) -> Result<(), FirestoneError> {
    paths.validate_machine_runtime_dir(name)?;
    atomic::write_with_mode(
        &paths.machine_shim_pid(name)?,
        format!("{}\n", identity.shim.pid).as_bytes(),
        0o600,
    )?;
    publish_process_identity(paths, name, identity)
}

fn publish_process_identity(
    paths: &Paths,
    name: &str,
    identity: &ProcessIdentity,
) -> Result<(), FirestoneError> {
    atomic::write_json_with_mode(&paths.machine_process_identity(name)?, identity, 0o600)
}

fn read_process_identity(paths: &Paths, name: &str) -> Result<ProcessIdentity, FirestoneError> {
    paths.validate_machine_runtime_dir(name)?;
    let path = paths.machine_process_identity(name)?;
    let bytes = read_private_file(
        &path,
        paths.uid(),
        0o600,
        MAX_PROCESS_IDENTITY_BYTES,
        "process identity",
    )?;
    let identity: ProcessIdentity = serde_json::from_slice(&bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot parse process identity {}", path.display()),
        )
        .with_source(source)
    })?;
    if identity.version != IDENTITY_VERSION {
        return Err(protocol_error("unsupported process identity version"));
    }
    Ok(identity)
}

fn read_exact_vmconfig(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
) -> Result<Vec<u8>, FirestoneError> {
    paths.validate_machine_data_directory(name)?;
    let path = paths.machine_vmconfig(name)?;
    let bytes = read_regular_file(&path, MAX_VMCONFIG_BYTES, "canonical VmConfig")?;
    if u64::try_from(bytes.len()).ok() != Some(plan.vmconfig_len)
        || sha256_hex(&bytes) != plan.vmconfig_sha256
    {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "canonical VmConfig {} changed after preparation",
                path.display()
            ),
        )
        .with_hint("rerun start to publish and launch one exact VmConfig"));
    }
    Ok(bytes)
}

fn read_private_file(
    path: &Path,
    uid: u32,
    mode: u32,
    limit: u64,
    label: &str,
) -> Result<Vec<u8>, FirestoneError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot open {label} {}", path.display()),
                source,
            )
        })?;
    let metadata = file.metadata().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect {label} {}", path.display()),
            source,
        )
    })?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o7777 != mode {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "{label} {} is not a private regular file with uid {uid} and mode {mode:04o}",
                path.display()
            ),
        ));
    }
    read_bounded(&mut file, limit, label)
}

fn read_regular_file(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>, FirestoneError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot open {label} {}", path.display()),
                source,
            )
        })?;
    if !file
        .metadata()
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect {label} {}", path.display()),
                source,
            )
        })?
        .is_file()
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} {} is not a regular file", path.display()),
        ));
    }
    read_bounded(&mut file, limit, label)
}

fn read_bounded(file: &mut File, limit: u64, label: &str) -> Result<Vec<u8>, FirestoneError> {
    let capacity = usize::try_from(limit.min(64 * 1024)).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("{label} limit does not fit usize"),
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut bounded = file.take(limit.saturating_add(1));
    bounded.read_to_end(&mut bytes).map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, format!("cannot read {label}")).with_source(source)
    })?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > limit) {
        return Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("{label} exceeds the {limit} byte limit"),
        ));
    }
    Ok(bytes)
}

fn hash_file(path: &Path, limit: u64, label: &str) -> Result<String, FirestoneError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot open {label} {}", path.display()),
                source,
            )
        })?;
    let metadata = file.metadata().map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect {label} {}", path.display()),
            source,
        )
    })?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} {} is not a bounded regular file", path.display()),
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot hash {label} {}", path.display()),
                source,
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn duration_millis(duration: Duration, label: &str) -> Result<u64, FirestoneError> {
    u64::try_from(duration.as_millis()).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Usage,
            format!("shim {label} timeout does not fit u64 milliseconds"),
        )
    })
}

fn enter_shim_session() -> Result<(), FirestoneError> {
    match setsid() {
        Ok(_) => Ok(()),
        Err(Errno::EPERM) if getpgrp() == getpid() => Ok(()),
        Err(source) => Err(FirestoneError::new(
            ErrorKind::Generic,
            "cannot create the shim session and process group",
        )
        .with_source(io::Error::from_raw_os_error(source as i32))),
    }
}

fn current_executable() -> Result<PathBuf, FirestoneError> {
    env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                "cannot resolve the running shim executable".to_owned(),
                source,
            )
        })
}

fn process_record(
    pid: u32,
    process_group: u32,
    executable: PathBuf,
) -> Result<ProcessRecord, FirestoneError> {
    Ok(ProcessRecord {
        pid,
        process_group,
        executable,
        uid: nix::unistd::getuid().as_raw(),
        start_time_ticks: process_start_time(pid)?,
    })
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Result<Option<u64>, FirestoneError> {
    let path = PathBuf::from("/proc").join(pid.to_string()).join("stat");
    let stat = fs::read_to_string(&path).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot read process start time from {}", path.display()),
            source,
        )
    })?;
    let close = stat.rfind(')').ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("process stat {} has no command terminator", path.display()),
        )
    })?;
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    let start = fields.get(19).ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("process stat {} has no start time", path.display()),
        )
    })?;
    start.parse::<u64>().map(Some).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("process stat {} has invalid start time", path.display()),
        )
        .with_source(source)
    })
}

#[cfg(not(target_os = "linux"))]
fn process_start_time(_pid: u32) -> Result<Option<u64>, FirestoneError> {
    Ok(None)
}

fn verify_owned_process(
    record: &ProcessRecord,
    process: &mut ManagedProcess,
) -> Result<(), FirestoneError> {
    if process.id() != record.pid || process.process_group() != Some(record.process_group) {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            "owned VMM process identity does not match its child handle",
        ));
    }
    if process.try_wait()?.is_some() {
        return Err(FirestoneError::new(
            ErrorKind::NotRunning,
            format!("VMM pid {} already exited", record.pid),
        ));
    }
    // An unreaped Child handle pins this pid; the separately verified pgid was
    // created by Cmd before exec, so it cannot refer to a reused process here.
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_linux_process(record: &ProcessRecord) -> Result<(), FirestoneError> {
    let proc_dir = PathBuf::from("/proc").join(record.pid.to_string());
    let owner = fs::metadata(&proc_dir).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot verify VMM pid {} owner", record.pid),
            source,
        )
    })?;
    if owner.uid() != record.uid {
        return Err(reused_pid_error(record.pid));
    }
    let cmdline = fs::read(proc_dir.join("cmdline")).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot verify VMM pid {} cmdline", record.pid),
            source,
        )
    })?;
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    if OsStr::from_bytes(argv0) != record.executable.as_os_str() {
        return Err(reused_pid_error(record.pid));
    }
    let executable = fs::canonicalize(proc_dir.join("exe")).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot verify VMM pid {} executable", record.pid),
            source,
        )
    })?;
    if executable != record.executable {
        return Err(reused_pid_error(record.pid));
    }
    if process_start_time(record.pid)? != record.start_time_ticks {
        return Err(reused_pid_error(record.pid));
    }
    let group = nix::unistd::getpgid(Some(pid_from_u32(record.pid)?)).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("cannot verify VMM pid {} process group", record.pid),
        )
        .with_source(io::Error::from_raw_os_error(source as i32))
    })?;
    let expected_group =
        i32::try_from(record.process_group).map_err(|_| reused_pid_error(record.pid))?;
    if group.as_raw() != expected_group {
        return Err(reused_pid_error(record.pid));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_state(pid: u32) -> Result<Option<char>, FirestoneError> {
    let path = PathBuf::from("/proc").join(pid.to_string()).join("stat");
    let stat = match fs::read_to_string(&path) {
        Ok(stat) => stat,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Conflict,
                format!("cannot read process state from {}", path.display()),
                source,
            ));
        }
    };
    let close = stat.rfind(')').ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("process stat {} has no command terminator", path.display()),
        )
    })?;
    Ok(stat[close + 1..]
        .split_whitespace()
        .next()
        .and_then(|value| value.chars().next()))
}

fn verified_process_alive(record: &ProcessRecord) -> Result<bool, FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        let proc_dir = PathBuf::from("/proc").join(record.pid.to_string());
        if !proc_dir.try_exists().map_err(|source| {
            filesystem_error(
                ErrorKind::Conflict,
                format!("cannot inspect VMM pid {}", record.pid),
                source,
            )
        })? {
            return Ok(false);
        }
        if matches!(process_state(record.pid)?, None | Some('Z')) {
            return Ok(false);
        }
        verify_linux_process(record)?;
        Ok(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = record;
        Err(FirestoneError::new(
            ErrorKind::Conflict,
            "signal escalation for an unsupervised VMM requires Linux /proc identity",
        )
        .with_hint("restore supervision or stop the verified VMM manually"))
    }
}

#[cfg(target_os = "linux")]
fn signal_verified_group(
    record: &ProcessRecord,
    signal: ProcessSignal,
) -> Result<(), FirestoneError> {
    verify_linux_process(record)?;
    let signal = match signal {
        ProcessSignal::Interrupt => Signal::SIGINT,
        ProcessSignal::Terminate => Signal::SIGTERM,
        ProcessSignal::Kill => Signal::SIGKILL,
    };
    match killpg(pid_from_u32(record.process_group)?, signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(source) => Err(FirestoneError::new(
            ErrorKind::Generic,
            format!(
                "cannot signal verified VMM process group {}",
                record.process_group
            ),
        )
        .with_source(io::Error::from_raw_os_error(source as i32))),
    }
}

#[cfg(not(target_os = "linux"))]
fn signal_verified_group(
    _record: &ProcessRecord,
    _signal: ProcessSignal,
) -> Result<(), FirestoneError> {
    Err(FirestoneError::new(
        ErrorKind::Conflict,
        "refusing to signal an unsupervised VMM without Linux process identity",
    ))
}

fn terminate_process(
    record: &ProcessRecord,
    process: &mut ManagedProcess,
) -> Result<(), FirestoneError> {
    if process.try_wait()?.is_some() {
        return Ok(());
    }
    verify_owned_process(record, process)?;
    process.signal_group(ProcessSignal::Terminate)?;
    if process.wait_timeout(CHILD_TERM_GRACE)?.is_none() {
        verify_owned_process(record, process)?;
        process.signal_group(ProcessSignal::Kill)?;
        if process.wait_timeout(CHILD_TERM_GRACE)?.is_none() {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!("cannot reap VMM pid {} after SIGKILL", record.pid),
            ));
        }
    }
    let _ = process.signal_group(ProcessSignal::Kill);
    reap_adopted_children();
    Ok(())
}

fn wait_for_record_exit(record: &ProcessRecord, timeout: Duration) -> Result<(), FirestoneError> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        FirestoneError::new(ErrorKind::Usage, "process wait deadline is out of range")
    })?;
    while Instant::now() < deadline {
        if !verified_process_alive(record)? {
            return Ok(());
        }
        thread::sleep(LOOP_INTERVAL);
    }
    if verified_process_alive(record)? {
        Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!("verified VMM pid {} did not exit", record.pid),
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn pid_from_u32(pid: u32) -> Result<Pid, FirestoneError> {
    i32::try_from(pid).map(Pid::from_raw).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("process id {pid} does not fit pid_t"),
        )
    })
}

#[cfg(target_os = "linux")]
fn reused_pid_error(pid: u32) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Conflict,
        format!("recorded VMM pid {pid} no longer has its verified process identity"),
    )
    .with_hint("refusing to signal a possibly reused pid")
}

fn reap_adopted_children() {
    #[cfg(target_os = "linux")]
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn bind_control_socket(paths: &Paths, name: &str) -> Result<UnixListener, FirestoneError> {
    let runtime = paths.validate_machine_runtime_dir(name)?;
    let socket = paths.machine_shim_socket(name)?;
    remove_stale_socket(&socket, paths.uid())?;
    let listener = UnixListener::bind(&socket).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot bind shim socket {}", socket.display()),
            source,
        )
    })?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot protect shim socket {}", socket.display()),
            source,
        )
    })?;
    let directory = File::open(&runtime).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!(
                "cannot open machine runtime directory {}",
                runtime.display()
            ),
            source,
        )
    })?;
    directory.sync_all().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!(
                "cannot fsync machine runtime directory {}",
                runtime.display()
            ),
            source,
        )
    })?;
    Ok(listener)
}

fn remove_stale_socket(path: &Path, uid: u32) -> Result<(), FirestoneError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect stale shim socket {}", path.display()),
                source,
            ));
        }
    };
    if metadata.uid() != uid || !metadata.file_type().is_socket() {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "stale shim socket {} is not an owned socket",
                path.display()
            ),
        )
        .with_hint("remove the unsafe runtime entry and retry"));
    }
    fs::remove_file(path).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot remove stale shim socket {}", path.display()),
            source,
        )
    })
}

fn authorize_peer(stream: &UnixStream, expected_uid: u32) -> Result<(), FirestoneError> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let credentials =
            nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(|source| {
                    FirestoneError::new(ErrorKind::Conflict, "cannot read shim peer credentials")
                        .with_source(io::Error::from_raw_os_error(source as i32))
                })?;
        if credentials.uid() != expected_uid {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "shim control peer uid {} does not match owner uid {expected_uid}",
                    credentials.uid()
                ),
            ));
        }
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        let (uid, _) = nix::unistd::getpeereid(stream).map_err(|source| {
            FirestoneError::new(ErrorKind::Conflict, "cannot read shim peer credentials")
                .with_source(io::Error::from_raw_os_error(source as i32))
        })?;
        if uid.as_raw() != expected_uid {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "shim control peer uid {} does not match owner uid {expected_uid}",
                    uid.as_raw()
                ),
            ));
        }
    }
    Ok(())
}

fn read_request(stream: &UnixStream, timeout: Duration) -> Result<ControlRequest, FirestoneError> {
    stream.set_nonblocking(true).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            "cannot make shim control connection nonblocking",
        )
        .with_source(source)
    })?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| FirestoneError::new(ErrorKind::Usage, "control deadline is out of range"))?;
    let frame = read_frame(stream, MAX_CONTROL_REQUEST_BYTES, deadline)?;
    let mut trailing = [0_u8; 1];
    match (&*stream).read(&mut trailing) {
        Ok(0) => {}
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
        Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
        Ok(_) => {
            return Err(protocol_error(
                "shim connection contains more than one request frame",
            ));
        }
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "cannot inspect shim request framing",
            )
            .with_source(source));
        }
    }
    serde_json::from_slice(&frame)
        .map_err(|source| protocol_error_with_source("invalid shim control request", source))
}

fn connect_control_socket(path: &Path, deadline: Instant) -> Result<UnixStream, FirestoneError> {
    let address = UnixAddr::new(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::NotRunning,
            format!("cannot address shim socket {}", path.display()),
        )
        .with_source(io::Error::from(source))
    })?;
    loop {
        if Instant::now() >= deadline {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!("timed out connecting to shim socket {}", path.display()),
            ));
        }
        let flags = {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                SockFlag::empty()
            }
        };
        let descriptor = socket(
            AddressFamily::Unix,
            SockType::Stream,
            flags,
            None::<SockProtocol>,
        )
        .map_err(|source| {
            FirestoneError::new(ErrorKind::Generic, "cannot create shim client socket")
                .with_source(io::Error::from(source))
        })?;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            fcntl(&descriptor, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    "cannot make shim client socket nonblocking",
                )
                .with_source(io::Error::from(source))
            })?;
            fcntl(&descriptor, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    "cannot make shim client socket close-on-exec",
                )
                .with_source(io::Error::from(source))
            })?;
        }
        match connect(descriptor.as_raw_fd(), &address) {
            Ok(()) => return Ok(UnixStream::from(descriptor)),
            Err(Errno::EINPROGRESS | Errno::EALREADY) => {
                let stream = UnixStream::from(descriptor);
                wait_control_connect(&stream, deadline, path)?;
                return Ok(stream);
            }
            Err(Errno::ENOENT | Errno::ECONNREFUSED | Errno::EAGAIN) => {
                thread::sleep(
                    LOOP_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::NotRunning,
                    format!("cannot connect to shim socket {}", path.display()),
                )
                .with_source(io::Error::from(source)));
            }
        }
    }
}

fn wait_control_connect(
    stream: &UnixStream,
    deadline: Instant,
    path: &Path,
) -> Result<(), FirestoneError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!("timed out connecting to shim socket {}", path.display()),
        ));
    }
    let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
    let mut descriptors = [PollFd::new(stream.as_fd(), PollFlags::POLLOUT)];
    let ready = poll(&mut descriptors, timeout).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot poll shim socket {}", path.display()),
        )
        .with_source(io::Error::from(source))
    })?;
    if ready == 0 {
        return Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!("timed out connecting to shim socket {}", path.display()),
        ));
    }
    let pending = getsockopt(stream, SocketError).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot inspect shim socket {} connection", path.display()),
        )
        .with_source(io::Error::from(source))
    })?;
    if pending == 0 {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::NotRunning,
            format!("cannot connect to shim socket {}", path.display()),
        )
        .with_source(io::Error::from_raw_os_error(pending)))
    }
}

fn read_frame(
    mut stream: impl Read,
    limit: usize,
    deadline: Instant,
) -> Result<Vec<u8>, FirestoneError> {
    let mut frame = Vec::with_capacity(limit.min(4096));
    let mut byte = [0_u8; 1];
    loop {
        if Instant::now() >= deadline {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                "shim control frame exceeded its absolute deadline",
            ));
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(protocol_error(
                    "shim control connection closed before newline",
                ));
            }
            Ok(_) if byte[0] == b'\n' => {
                if frame.is_empty() {
                    return Err(protocol_error("shim control frame is empty"));
                }
                return Ok(frame);
            }
            Ok(_) => {
                if frame.len() == limit {
                    return Err(protocol_error("shim control frame exceeds its byte limit"));
                }
                frame.push(byte[0]);
            }
            Err(source)
                if matches!(
                    source.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(LOOP_INTERVAL);
            }
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    "cannot read shim control frame",
                )
                .with_source(source));
            }
        }
    }
}

fn write_frame(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), FirestoneError> {
    write_control_bytes(stream, bytes, deadline)?;
    write_control_bytes(stream, b"\x0a", deadline)
}

fn write_control_bytes(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), FirestoneError> {
    let mut written = 0_usize;
    while written < bytes.len() {
        if Instant::now() >= deadline {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                "shim control write exceeded its absolute deadline",
            ));
        }
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(protocol_error("shim control write made no progress")),
            Ok(count) => written += count,
            Err(source)
                if matches!(
                    source.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(LOOP_INTERVAL);
            }
            Err(source) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    "cannot write shim control frame",
                )
                .with_source(source));
            }
        }
    }
    Ok(())
}

fn write_json_line<T: Serialize>(
    mut stream: UnixStream,
    value: &T,
    timeout: Duration,
) -> Result<(), FirestoneError> {
    let bytes = serde_json::to_vec(value).map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot serialize shim control response")
            .with_source(source)
    })?;
    if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        return Err(protocol_error(
            "shim control response exceeds its byte limit",
        ));
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| FirestoneError::new(ErrorKind::Usage, "control deadline is out of range"))?;
    write_frame(&mut stream, &bytes, deadline)
}

fn write_terminal_ok(stream: UnixStream, timeout: Duration) -> Result<(), FirestoneError> {
    write_json_line(stream, &OkResponse { ok: true }, timeout)
}

fn write_terminal_error(
    stream: UnixStream,
    error: &FirestoneError,
    timeout: Duration,
) -> Result<(), FirestoneError> {
    write_json_line(
        stream,
        &ErrorResponse {
            ok: false,
            error: &error.info(),
        },
        timeout,
    )
}

fn write_status(
    stream: UnixStream,
    state: &MachineState,
    pids: &ShimPids,
    timeout: Duration,
) -> Result<(), FirestoneError> {
    write_json_line(
        stream,
        &StatusResponse {
            ok: true,
            status: state.status,
            pids,
            started_at: &state.started_at,
            degraded: &state.degraded,
        },
        timeout,
    )
}

fn status_pids(state: &MachineState, shim_pid: u32) -> ShimPids {
    ShimPids {
        shim: shim_pid,
        vmm: state.vmm_pid,
        sidecars: state.sidecar_pids.clone(),
    }
}

struct ProtocolSink {
    stream: Option<UnixStream>,
    timeout: Duration,
}

impl ProtocolSink {
    fn new(stream: UnixStream, timeout: Duration) -> Self {
        Self {
            stream: Some(stream),
            timeout,
        }
    }

    fn terminal_ok(&mut self) {
        self.write(&OkResponse { ok: true });
    }

    fn terminal_error(&mut self, error: &FirestoneError) {
        self.write(&ErrorResponse {
            ok: false,
            error: &error.info(),
        });
    }

    fn write<T: Serialize>(&mut self, value: &T) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        let result = serde_json::to_vec(value)
            .map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot encode shim event")
                    .with_source(source)
            })
            .and_then(|bytes| {
                if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
                    return Err(protocol_error("shim event exceeds its byte limit"));
                }
                let deadline = Instant::now()
                    .checked_add(self.timeout)
                    .ok_or_else(|| protocol_error("shim event deadline is out of range"))?;
                write_frame(&mut stream, &bytes, deadline)
            });
        if result.is_ok() {
            self.stream = Some(stream);
        }
    }
}

impl EventSink for ProtocolSink {
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
        self.write(&event);
        Ok(())
    }
}

fn require_ok_terminal(value: &Value) -> Result<(), FirestoneError> {
    match value.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        Some(false) => Err(error_from_terminal(value)?),
        None => Err(protocol_error(
            "shim terminal response has no boolean ok field",
        )),
    }
}

fn error_from_terminal(value: &Value) -> Result<FirestoneError, FirestoneError> {
    let info = value
        .get("error")
        .ok_or_else(|| protocol_error("shim failure response has no error object"))?;
    let info: ErrorInfo = serde_json::from_value(info.clone()).map_err(|source| {
        protocol_error_with_source("shim failure error object is invalid", source)
    })?;
    let mut error = FirestoneError::new(info.kind, info.message);
    if let Some(hint) = info.hint {
        error = error.with_hint(hint);
    }
    Ok(error)
}

fn wait_for_shim_socket(
    socket: &Path,
    process: &mut ManagedProcess,
    deadline: Instant,
) -> Result<(), FirestoneError> {
    loop {
        if let Some(status) = process.try_wait()? {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "shim process {} exited before its control socket was ready: {}",
                    process.id(),
                    status_description(status)
                ),
            ));
        }
        if socket.try_exists().map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect shim socket {}", socket.display()),
                source,
            )
        })? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "shim socket {} was not ready before its deadline",
                    socket.display()
                ),
            ));
        }
        thread::sleep(LOOP_INTERVAL);
    }
}

fn rollback_before_shim(
    paths: &Paths,
    prepared: &mut PreparedStart,
    lock: &MachineLock,
    error: &FirestoneError,
) -> Result<(), FirestoneError> {
    prepared.state.status = prepared.previous_status;
    prepared.state.shim_pid = None;
    prepared.state.vmm_pid = None;
    prepared.state.started_at = None;
    prepared.state.sidecar_pids.clear();
    prepared.state.degraded.clear();
    prepared.state.last_exit = Some(LastExit {
        at: now_timestamp(),
        code: None,
        signal: None,
        reason: ExitReason::Failure(error.message().to_owned()),
    });
    StateStore::new(paths.machine_state(&prepared.name)?)
        .write_from_locked_action(&prepared.state, lock)?;
    paths.clear_machine_runtime_dir(&prepared.name, true)
}

fn terminate_unready_shim(
    paths: &Paths,
    name: &str,
    process: &mut ManagedProcess,
    state: &MachineState,
    error: &FirestoneError,
) -> Result<(), FirestoneError> {
    let _ = process.signal_process(ProcessSignal::Kill);
    let _ = process.wait_timeout(CHILD_TERM_GRACE);
    let mut events = Vec::new();
    let lock = MachineLock::acquire(name, &paths.machine_lock(name)?, &mut events)?;
    let mut failed = state.clone();
    failed.status = MachineStatus::Failed;
    failed.shim_pid = None;
    failed.vmm_pid = None;
    failed.started_at = None;
    failed.last_exit = Some(LastExit {
        at: now_timestamp(),
        code: None,
        signal: None,
        reason: ExitReason::Failure(error.message().to_owned()),
    });
    StateStore::new(paths.machine_state(name)?).write_from_locked_action(&failed, &lock)?;
    paths.clear_machine_runtime_dir(name, true)
}

fn cleanup_after_shim(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    paths.clear_machine_runtime_dir(name, true)
}

fn rotate_console_log(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    recover_console_log(paths, name)?;
    paths.validate_machine_data_directory(name)?;
    let current = paths.machine_console_log(name)?;
    let previous = paths.machine_console_previous_log(name)?;
    match fs::symlink_metadata(&current) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(FirestoneError::new(
                    ErrorKind::Dependency,
                    format!("console log {} is not a regular file", current.display()),
                ));
            }
            if metadata.len() > 0 {
                fs::rename(&current, &previous).map_err(|source| {
                    filesystem_error(
                        ErrorKind::Generic,
                        format!("cannot rotate console log {}", current.display()),
                        source,
                    )
                })?;
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect console log {}", current.display()),
                source,
            ));
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&current)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot create console log {}", current.display()),
                source,
            )
        })?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot protect console log {}", current.display()),
                source,
            )
        })?;
    Ok(())
}

fn secure_console_log(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    let path = paths.machine_console_log(name)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot protect console log {}", path.display()),
            source,
        )
    })
}

fn recover_console_log(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    let previous = paths.machine_console_previous_log(name)?;
    if !previous.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect previous console log {}", previous.display()),
            source,
        )
    })? {
        return Ok(());
    }
    merge_console_log(paths, name)
}

fn merge_console_log(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    paths.validate_machine_data_directory(name)?;
    let previous = paths.machine_console_previous_log(name)?;
    if !previous.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect previous console log {}", previous.display()),
            source,
        )
    })? {
        return Ok(());
    }
    let current = paths.machine_console_log(name)?;
    validate_log_file(&previous)?;
    if current.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect console log {}", current.display()),
            source,
        )
    })? {
        validate_log_file(&current)?;
    }
    atomic::write_stream(&current, |target| {
        let mut old = File::open(&previous)?;
        io::copy(&mut old, target)?;
        if current.exists() {
            let mut new = File::open(&current)?;
            io::copy(&mut new, target)?;
        }
        Ok(())
    })?;
    fs::set_permissions(&current, fs::Permissions::from_mode(0o600)).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot protect merged console log {}", current.display()),
            source,
        )
    })?;
    fs::remove_file(&previous).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot remove previous console log {}", previous.display()),
            source,
        )
    })
}

fn validate_log_file(path: &Path) -> Result<(), FirestoneError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect log {}", path.display()),
            source,
        )
    })?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("log {} is not a regular non-symlink file", path.display()),
        ))
    }
}

fn write_shim_log(message: &str) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}

fn vmm_failure_reason(paths: &Paths, name: &str, fallback: &str) -> Result<String, FirestoneError> {
    let path = paths.machine_vmm_log(name)?;
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(fallback.to_owned()),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!("cannot read VMM log {}", path.display()),
                source,
            ));
        }
    };
    let length = file
        .metadata()
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot inspect VMM log {}", path.display()),
                source,
            )
        })?
        .len();
    file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)))
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot seek VMM log {}", path.display()),
                source,
            )
        })?;
    let mut tail = Vec::new();
    file.take(LOG_TAIL_BYTES)
        .read_to_end(&mut tail)
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot read VMM log {}", path.display()),
                source,
            )
        })?;
    let text = String::from_utf8_lossy(&tail);
    let reason = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or(fallback);
    Ok(reason.chars().take(LOG_REASON_BYTES).collect())
}

fn status_description(status: ExitStatus) -> String {
    status.code().map_or_else(
        || {
            status.signal().map_or_else(
                || "unknown status".to_owned(),
                |signal| format!("signal {signal}"),
            )
        },
        |code| format!("code {code}"),
    )
}

fn now_timestamp() -> String {
    jiff::Timestamp::now().to_string()
}

fn protocol_error(message: impl Into<String>) -> FirestoneError {
    FirestoneError::new(ErrorKind::Generic, message.into())
        .with_hint("the socket must use Firestone's bounded newline-delimited JSON protocol")
}

fn protocol_error_with_source(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FirestoneError {
    protocol_error(message).with_source(source)
}

fn filesystem_error(kind: ErrorKind, message: String, source: io::Error) -> FirestoneError {
    FirestoneError::new(kind, message)
        .with_hint("check the Firestone-owned path and retry")
        .with_source(source)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    #[cfg(target_os = "linux")]
    use super::{ProcessRecord, process_record, verify_linux_process};
    use super::{authorize_peer, validate_m1_start_scope};
    use crate::{ErrorKind, FirestoneError, MachineSpec, MountSpec, NetMode};

    fn require_error<T>(result: Result<T, FirestoneError>, label: &str) -> FirestoneError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("{label}"),
        }
    }

    #[test]
    fn m1_scope_network_and_mount_sidecars_are_rejected() {
        let mut spec = MachineSpec::default();
        let passt = require_error(validate_m1_start_scope(&spec), "default passt is deferred");
        assert_eq!(passt.kind(), ErrorKind::InvalidSpec);
        assert!(passt.message().contains("network.mode `passt`"));

        spec.network.mode = NetMode::Tap;
        let tap = require_error(validate_m1_start_scope(&spec), "tap is deferred");
        assert!(tap.message().contains("network.mode `tap`"));

        spec.network.mode = NetMode::None;
        spec.mounts.push(MountSpec {
            host: PathBuf::from("/tmp"),
            guest: PathBuf::from("/work"),
            readonly: false,
            tag: None,
        });
        let mount = require_error(validate_m1_start_scope(&spec), "mounts are deferred");
        assert!(mount.message().contains("shared mounts"));

        spec.mounts.clear();
        assert!(validate_m1_start_scope(&spec).is_ok());
    }

    #[test]
    fn peer_credentials_same_uid_is_authorized_and_other_uid_is_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let (client, server) = std::os::unix::net::UnixStream::pair()?;
        let uid = nix::unistd::getuid().as_raw();
        authorize_peer(&server, uid)?;
        let error = require_error(
            authorize_peer(&client, uid.saturating_add(1)),
            "different uid must be rejected",
        );
        assert_eq!(error.kind(), ErrorKind::Conflict);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_identity_changed_start_token_and_executable_are_rejected()
    -> Result<(), Box<dyn std::error::Error>> {
        let pid = std::process::id();
        let group = nix::unistd::getpgid(Some(super::pid_from_u32(pid)?))?;
        let executable = std::fs::canonicalize(std::env::current_exe()?)?;
        let record = process_record(pid, u32::try_from(group.as_raw())?, executable.clone())?;
        verify_linux_process(&record)?;

        let mut reused = record.clone();
        reused.start_time_ticks = reused.start_time_ticks.map(|value| value + 1);
        let error = require_error(
            verify_linux_process(&reused),
            "changed start time must fail",
        );
        assert_eq!(error.kind(), ErrorKind::Conflict);

        let wrong = ProcessRecord {
            executable: PathBuf::from("/bin/false"),
            ..record
        };
        let error = require_error(verify_linux_process(&wrong), "changed executable must fail");
        assert_eq!(error.kind(), ErrorKind::Conflict);
        Ok(())
    }

    #[test]
    fn shim_timeouts_zero_or_unbounded_are_rejected() {
        let zero = super::ShimTimeouts {
            api: Duration::ZERO,
            ..super::ShimTimeouts::default()
        };
        assert_eq!(
            require_error(zero.validate(), "zero timeout").kind(),
            ErrorKind::Usage
        );
        let large = super::ShimTimeouts {
            readiness: Duration::from_secs(super::MAX_STOP_TIMEOUT_SECONDS + 1),
            ..super::ShimTimeouts::default()
        };
        assert_eq!(
            require_error(large.validate(), "large timeout").kind(),
            ErrorKind::Usage
        );
    }
}
