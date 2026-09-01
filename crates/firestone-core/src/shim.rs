//! Per-machine shim preparation, control protocol, and process supervision.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
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
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
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
    unistd::{getpgrp, getpid, getsid, setsid},
};
#[cfg(target_os = "linux")]
use nix::{
    sys::wait::{WaitPidFlag, WaitStatus, waitpid},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;

use crate::cmd::ProcessDiagnostic;

use crate::{
    Arch, Cmd, ConsoleBroker, DependencyManifest, ErrorInfo, ErrorKind, Event, EventSink,
    ExitReason, FirestoneError, ImageStore, InternalHelper, LastExit, MachineLock, MachineSpec,
    MachineState, MachineStatus, ManagedProcess, NetMode, NetworkPlan, NetworkPlanOptions, Paths,
    ProcessSignal, RealValidationHost, StateStore, StepId, VirtiofsPlan, VirtiofsReadinessPlan,
    VirtiofsSandbox, VmConfigInput, VmState, VmmApi, VmmApiLivenessProbe, VmmPingProbe, atomic,
    embedded_helper, ensure_ssh_identity, invalidate_known_hosts_for_seed,
    materialize_embedded_helper, network::NetworkPlanSnapshot, prepare_network,
    prepare_virtiofs_plans_with_readiness, publish_seed_with_sshd_path, publish_vm_config,
    resolve_verified_apparmor_passt, virtiofs::VirtiofsPlanSnapshot,
    vmm::selected_pinned_boot_artifact,
};

const PLAN_VERSION: u32 = 2;
const IDENTITY_VERSION: u32 = 3;
const MAX_CONTROL_REQUEST_BYTES: usize = 4 * 1024;
const MAX_CONTROL_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CONTROL_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_LAUNCH_PLAN_BYTES: u64 = 256 * 1024;
const MAX_PROCESS_IDENTITY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_VMCONFIG_BYTES: u64 = 51_200;
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STOP_TIMEOUT_SECONDS: u64 = 60 * 60;
const LOOP_INTERVAL: Duration = Duration::from_millis(20);
const CHILD_TERM_GRACE: Duration = Duration::from_secs(5);
const STOP_API_PHASE_CAP: Duration = Duration::from_secs(2);
const CONTROL_ACCEPT_BACKOFF: Duration = Duration::from_millis(10);
const LOG_TAIL_BYTES: u64 = 64 * 1024;
const LOG_REASON_BYTES: usize = 4096;
const VMM_API_UNRESPONSIVE: &str = "vmm API unresponsive";

static OWNER_EVIDENCE_PID: AtomicU32 = AtomicU32::new(0);
static OWNER_EVIDENCE_STATE: AtomicU8 = AtomicU8::new(0);
const OWNER_ARMED: u8 = 1;
const OWNER_REAPED: u8 = 2;

/// Bounded timings used by shim preparation and supervision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShimTimeouts {
    pub api: Duration,
    pub readiness: Duration,
    pub control_io: Duration,
    pub launch_request: Duration,
    pub launch_overall: Duration,
    pub first_boot_launch_request: Duration,
    pub first_boot_launch_overall: Duration,
}

impl Default for ShimTimeouts {
    fn default() -> Self {
        Self {
            api: Duration::from_secs(2),
            readiness: Duration::from_secs(10),
            control_io: Duration::from_secs(2),
            launch_request: Duration::from_secs(30),
            launch_overall: Duration::from_secs(30),
            first_boot_launch_request: Duration::from_secs(30),
            first_boot_launch_overall: Duration::from_secs(30),
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
            ("launch_overall", self.launch_overall),
            ("first_boot_launch_request", self.first_boot_launch_request),
            ("first_boot_launch_overall", self.first_boot_launch_overall),
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
    seed_rewritten: bool,
    timeout: Duration,
    forwards: Vec<String>,
    mounts: Vec<String>,
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

    #[must_use]
    pub const fn seed_rewritten(&self) -> bool {
        self.seed_rewritten
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub fn forwards(&self) -> &[String] {
        &self.forwards
    }

    #[must_use]
    pub fn mounts(&self) -> &[String] {
        &self.mounts
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
    network: NetworkPlanSnapshot,
    filesystems: Vec<VirtiofsPlanSnapshot>,
    control_io_timeout_ms: u64,
    launch_request_timeout_ms: u64,
    launch_overall_timeout_ms: u64,
}

impl LaunchPlan {
    fn timeouts(&self) -> Result<ShimTimeouts, FirestoneError> {
        let timeouts = ShimTimeouts {
            api: Duration::from_millis(self.api_timeout_ms),
            readiness: Duration::from_millis(self.readiness_timeout_ms),
            control_io: Duration::from_millis(self.control_io_timeout_ms),
            launch_request: Duration::from_millis(self.launch_request_timeout_ms),
            launch_overall: Duration::from_millis(self.launch_overall_timeout_ms),
            first_boot_launch_request: Duration::from_millis(self.launch_request_timeout_ms),
            first_boot_launch_overall: Duration::from_millis(self.launch_overall_timeout_ms),
        };
        timeouts.validate()
    }
    fn network_plan(&self) -> Result<NetworkPlan, FirestoneError> {
        NetworkPlan::from_snapshot(self.network.clone())
    }

    fn filesystem_plans(&self) -> Result<Vec<VirtiofsPlan>, FirestoneError> {
        self.filesystems
            .iter()
            .cloned()
            .map(VirtiofsPlan::from_snapshot)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity {
    version: u32,
    shim: ProcessRecord,
    vmm: Option<ProcessRecord>,
    sidecars: BTreeMap<String, ProcessRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessRecord {
    pid: u32,
    process_group: u32,
    executable: PathBuf,
    executable_dev: u64,
    executable_ino: u64,
    argv_hex: Vec<String>,
    launch_artifact: Option<PathBuf>,
    launch_argv_hex: Option<Vec<String>>,
    launch_binding: Option<String>,
    launch_sha256: Option<String>,
    uid: u32,
    start_time_ticks: Option<u64>,
}

type PreservedProcessRoots = BTreeMap<u32, Option<u64>>;

fn preserved_process_roots<'a>(
    records: impl IntoIterator<Item = &'a ProcessRecord>,
) -> PreservedProcessRoots {
    records
        .into_iter()
        .map(|record| (record.pid, record.start_time_ticks))
        .collect()
}

struct RecoveredProcess {
    label: String,
    record: ProcessRecord,
    artifacts: Vec<RuntimeArtifact>,
    #[cfg(target_os = "linux")]
    pidfd: rustix::fd::OwnedFd,
}

impl RecoveredProcess {
    fn new(
        label: impl Into<String>,
        record: ProcessRecord,
        artifacts: Vec<RuntimeArtifact>,
    ) -> Result<Self, FirestoneError> {
        let label = label.into();
        #[cfg(target_os = "linux")]
        {
            verify_linux_process(&record)?;
            let raw = i32::try_from(record.pid).map_err(|_| reused_pid_error(record.pid))?;
            let pid =
                rustix::process::Pid::from_raw(raw).ok_or_else(|| reused_pid_error(record.pid))?;
            let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
                .map_err(|source| {
                    FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("cannot pin recovered {label} pid {}", record.pid),
                    )
                    .with_source(io::Error::from_raw_os_error(source.raw_os_error()))
                })?;
            verify_linux_process(&record)?;
            Ok(Self {
                label,
                record,
                artifacts,
                pidfd,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (label, record, artifacts);
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                "recovered process supervision requires Linux pidfds",
            ))
        }
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn record(&self) -> &ProcessRecord {
        &self.record
    }

    fn artifacts(&self) -> &[RuntimeArtifact] {
        &self.artifacts
    }

    fn is_alive(&self) -> Result<bool, FirestoneError> {
        #[cfg(target_os = "linux")]
        {
            let mut descriptors = [PollFd::new(self.pidfd.as_fd(), PollFlags::POLLIN)];
            poll(&mut descriptors, PollTimeout::ZERO).map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!(
                        "cannot poll recovered {} pidfd for pid {}",
                        self.label, self.record.pid
                    ),
                )
                .with_source(io::Error::from_raw_os_error(source as i32))
            })?;
            let events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
            Ok(!events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                "recovered process liveness requires Linux pidfds",
            ))
        }
    }
}

struct OwnedVmm {
    process: Option<ManagedProcess>,
    diagnostic: ProcessDiagnostic,
    record: ProcessRecord,
    reaped_status: Option<ExitStatus>,
    preserved_children: PreservedProcessRoots,
    console: Option<ConsoleBroker>,
}

impl OwnedVmm {
    fn from_spawn(
        process: ManagedProcess,
        executable: PathBuf,
        preserved_children: PreservedProcessRoots,
    ) -> Self {
        let pid = process.id();
        let process_group = process.process_group().unwrap_or(pid);
        let diagnostic = process.diagnostic();
        OWNER_EVIDENCE_PID.store(pid, Ordering::Relaxed);
        OWNER_EVIDENCE_STATE.store(OWNER_ARMED, Ordering::Release);
        Self {
            process: Some(process),
            diagnostic,
            record: ProcessRecord {
                pid,
                process_group,
                executable: executable.clone(),
                executable_dev: 0,
                executable_ino: 0,
                argv_hex: Vec::new(),
                launch_artifact: Some(executable),
                launch_argv_hex: None,
                launch_binding: None,
                launch_sha256: None,
                uid: nix::unistd::getuid().as_raw(),
                start_time_ticks: None,
            },
            reaped_status: None,
            preserved_children,
            console: None,
        }
    }

    fn bind_record(&mut self, record: ProcessRecord) {
        self.record = record;
    }

    fn attach_console(&mut self, broker: ConsoleBroker) {
        self.console = Some(broker);
    }

    fn id(&self) -> u32 {
        self.process
            .as_ref()
            .map_or(self.record.pid, ManagedProcess::id)
    }

    fn record(&self) -> &ProcessRecord {
        &self.record
    }

    fn is_fully_reaped(&self) -> bool {
        self.process.is_none() && self.reaped_status.is_some()
    }

    fn retained_status(&self) -> Option<ExitStatus> {
        self.reaped_status
    }
    fn exit_error(&self, status: ExitStatus, context: &str) -> FirestoneError {
        self.diagnostic.exit_error(
            status,
            context,
            "inspect the cloud-hypervisor log and retry the machine start",
        )
    }

    fn observe_exit(&self) -> Result<bool, FirestoneError> {
        if self.reaped_status.is_some() {
            return Ok(true);
        }
        self.process
            .as_ref()
            .ok_or_else(|| FirestoneError::new(ErrorKind::NotRunning, "VMM child was reaped"))?
            .observe_exit()
    }

    fn terminate_and_reap(
        &mut self,
        send_term: bool,
        term_grace: Duration,
    ) -> Result<ExitStatus, FirestoneError> {
        let cleanup_budget = term_grace
            .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
            .ok_or_else(|| {
                FirestoneError::new(ErrorKind::Usage, "VMM cleanup deadline is out of range")
            })?;
        let deadline = Instant::now().checked_add(cleanup_budget).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "VMM cleanup deadline is out of range")
        })?;
        self.terminate_and_reap_before(send_term, term_grace, deadline)
    }

    fn terminate_and_reap_before(
        &mut self,
        send_term: bool,
        term_grace: Duration,
        overall_deadline: Instant,
    ) -> Result<ExitStatus, FirestoneError> {
        if self.process.is_none() {
            return self.reaped_status.ok_or_else(|| {
                FirestoneError::new(ErrorKind::NotRunning, "VMM child was already released")
            });
        }

        if self.reaped_status.is_none() {
            let mut descendants =
                snapshot_owned_descendants_preserving(self.record.pid, &self.preserved_children)
                    .unwrap_or_else(|error| {
                        write_shim_log(&format!(
                            "cannot snapshot all VMM descendants ({}); group cleanup continues",
                            error.kind()
                        ));
                        BTreeMap::new()
                    });
            let process = self.process.as_mut().ok_or_else(|| {
                FirestoneError::new(ErrorKind::NotRunning, "VMM child was already released")
            })?;
            if send_term && !process.observe_exit()? {
                // Stop new forks in the pinned leader group before any fallible
                // descendant inspection or signalling.
                process.signal_group(ProcessSignal::Terminate)?;
                if let Err(error) = signal_descendants(&descendants, ProcessSignal::Terminate) {
                    write_shim_log(&format!(
                        "cannot signal every VMM descendant with SIGTERM ({}); cleanup continues",
                        error.kind()
                    ));
                }

                let deadline = Instant::now()
                    .checked_add(term_grace)
                    .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));

                while Instant::now() < deadline && !process.observe_exit()? {
                    thread::sleep(LOOP_INTERVAL);
                }
            }

            if let Ok(new_descendants) =
                snapshot_owned_descendants_preserving(self.record.pid, &self.preserved_children)
            {
                descendants.extend(new_descendants);
            }
            // The leader is still unreaped and pins this pgid. This is the last
            // numeric group signal; no killpg is permitted after wait().
            let leader_exited = process.observe_exit()?;
            signal_cleanup_group(process, ProcessSignal::Kill, leader_exited)?;
            if let Err(error) = signal_descendants(&descendants, ProcessSignal::Kill) {
                write_shim_log(&format!(
                    "cannot signal every escaped VMM descendant with SIGKILL ({}); drain continues",
                    error.kind()
                ));
            }

            let deadline = Instant::now()
                .checked_add(CHILD_TERM_GRACE)
                .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));

            while Instant::now() < deadline && !process.observe_exit()? {
                thread::sleep(LOOP_INTERVAL);
            }
            if !process.observe_exit()? {
                return Err(FirestoneError::new(
                    ErrorKind::Timeout,
                    format!("VMM pid {} did not exit after SIGKILL", self.record.pid),
                ));
            }
            self.reaped_status = Some(process.wait()?);
        }
        if let Some(console) = self.console.take() {
            if let Err(error) = console.shutdown() {
                write_shim_log(&format!(
                    "console broker shutdown failed ({}); VMM cleanup continues",
                    error.kind()
                ));
            }
        }

        // Keep the reaped ManagedProcess marker armed until every adopted VMM
        // child except separately-owned sidecars is gone. A drain error leaves
        // this guard retryable without signalling the old numeric pgid again.
        drain_adopted_children_preserving(
            overall_deadline.saturating_duration_since(Instant::now()),
            &self.preserved_children,
        )?;
        let status = self.reaped_status.ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "VMM exit status was not retained")
        })?;
        OWNER_EVIDENCE_PID.store(self.record.pid, Ordering::Relaxed);
        OWNER_EVIDENCE_STATE.store(OWNER_REAPED, Ordering::Release);
        self.process = None;
        Ok(status)
    }

    fn reap_exited_group(&mut self) -> Result<ExitStatus, FirestoneError> {
        self.terminate_and_reap(false, Duration::ZERO)
    }
}

fn take_owner_reap_proof(pid: u32) -> bool {
    if OWNER_EVIDENCE_PID.load(Ordering::Acquire) != pid {
        return false;
    }
    OWNER_EVIDENCE_STATE
        .compare_exchange(OWNER_REAPED, 0, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn signal_cleanup_group(
    process: &ManagedProcess,
    signal: ProcessSignal,
    _leader_exited: bool,
) -> Result<(), FirestoneError> {
    match process.signal_group(signal) {
        Ok(()) => Ok(()),
        #[cfg(not(target_os = "linux"))]
        Err(error) if _leader_exited => {
            write_shim_log(&format!(
                "exited VMM group was no longer signalable ({}); leader remains pinned until reap",
                error.kind()
            ));
            Ok(())
        }
        Err(error) => Err(error),
    }
}

impl Drop for OwnedVmm {
    fn drop(&mut self) {
        if self.process.is_none() {
            return;
        }
        if let Err(error) = self.terminate_and_reap(false, Duration::ZERO) {
            write_shim_log(&format!(
                "VMM ownership guard could not finish cleanup ({}); recovery evidence is preserved",
                error.kind()
            ));
        }
    }
}

#[derive(Clone, Copy)]
enum RuntimeArtifactKind {
    Socket,
    Regular,
}

#[derive(Clone)]
struct RuntimeArtifact {
    path: PathBuf,
    uid: u32,
    mode: u32,
    kind: RuntimeArtifactKind,
}

struct OwnedSidecar {
    name: String,
    process: Option<ManagedProcess>,
    diagnostic: ProcessDiagnostic,
    record: ProcessRecord,
    reaped_status: Option<ExitStatus>,
    artifacts: Vec<RuntimeArtifact>,
}

impl OwnedSidecar {
    fn from_spawn(
        name: String,
        process: ManagedProcess,
        executable: PathBuf,
        artifacts: Vec<RuntimeArtifact>,
    ) -> Self {
        let pid = process.id();
        let process_group = process.process_group().unwrap_or(pid);
        let diagnostic = process.diagnostic();
        Self {
            name,
            process: Some(process),
            diagnostic,
            record: ProcessRecord {
                pid,
                process_group,
                executable: executable.clone(),
                executable_dev: 0,
                executable_ino: 0,
                argv_hex: Vec::new(),
                launch_artifact: Some(executable),
                launch_argv_hex: None,
                launch_binding: None,
                launch_sha256: None,
                uid: nix::unistd::getuid().as_raw(),
                start_time_ticks: None,
            },
            reaped_status: None,
            artifacts,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn exit_error(&self, status: ExitStatus, context: &str) -> FirestoneError {
        self.diagnostic.exit_error(
            status,
            context,
            format!(
                "inspect the {} process log and retry the machine start",
                self.name
            ),
        )
    }
    fn id(&self) -> u32 {
        self.process
            .as_ref()
            .map_or(self.record.pid, ManagedProcess::id)
    }

    fn bind_record(&mut self, record: ProcessRecord) {
        self.record = record;
    }

    fn artifacts(&self) -> &[RuntimeArtifact] {
        &self.artifacts
    }

    fn observe_exit(&self) -> Result<bool, FirestoneError> {
        if self.reaped_status.is_some() {
            return Ok(true);
        }
        self.process
            .as_ref()
            .ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::NotRunning,
                    format!("sidecar {} child was reaped", self.name),
                )
            })?
            .observe_exit()
    }

    fn reap_exited(&mut self) -> Result<ExitStatus, FirestoneError> {
        if !self.observe_exit()? {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("sidecar {} is still running", self.name),
            ));
        }
        self.reap()
    }

    fn terminate_and_reap(
        &mut self,
        overall_deadline: Instant,
    ) -> Result<ExitStatus, FirestoneError> {
        if self.process.is_none() {
            return self.reaped_status.ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::NotRunning,
                    format!("sidecar {} child was already released", self.name),
                )
            });
        }
        if !self.observe_exit()? {
            let process = self.process.as_ref().ok_or_else(|| {
                FirestoneError::new(ErrorKind::NotRunning, "sidecar child disappeared")
            })?;
            process.signal_group(ProcessSignal::Terminate)?;
            let term_deadline = Instant::now()
                .checked_add(CHILD_TERM_GRACE)
                .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
            while Instant::now() < term_deadline && !process.observe_exit()? {
                thread::sleep(
                    LOOP_INTERVAL.min(term_deadline.saturating_duration_since(Instant::now())),
                );
            }
            if !process.observe_exit()? {
                process.signal_group(ProcessSignal::Kill)?;
                let kill_deadline = Instant::now()
                    .checked_add(CHILD_TERM_GRACE)
                    .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
                while Instant::now() < kill_deadline && !process.observe_exit()? {
                    thread::sleep(
                        LOOP_INTERVAL.min(kill_deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
        }
        if !self.observe_exit()? {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!("sidecar {} pid {} did not exit", self.name, self.record.pid),
            ));
        }
        self.reap()
    }

    fn reap(&mut self) -> Result<ExitStatus, FirestoneError> {
        if let Some(status) = self.reaped_status {
            return Ok(status);
        }
        let status = self
            .process
            .as_mut()
            .ok_or_else(|| FirestoneError::new(ErrorKind::NotRunning, "sidecar child is absent"))?
            .wait()?;
        self.reaped_status = Some(status);
        self.process = None;
        Ok(status)
    }
}

impl Drop for OwnedSidecar {
    fn drop(&mut self) {
        if self.process.is_none() {
            return;
        }
        let deadline = Instant::now()
            .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
            .unwrap_or_else(Instant::now);
        if let Err(error) = self.terminate_and_reap(deadline) {
            write_shim_log(&format!(
                "sidecar {} ownership guard could not finish cleanup ({}); recovery evidence is preserved",
                self.name,
                error.kind()
            ));
        }
    }
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
    let mut timeouts = timeouts.validate()?;
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
    let previous_status = state.status;

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
    if prepared_image.overlay.grown {
        events.emit(Event::StepDone {
            id: StepId::from("disk"),
            detail: Some(format!("grown to {} overlay", spec.disk)),
            elapsed_ms: 0,
        })?;
    } else if disk_existed {
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

    if state.mac.is_none() {
        state.mac = Some(allocated_mac(paths, name));
    }
    let mac_value = state.mac.as_deref().ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` has no persisted MAC address"),
        )
        .with_hint("repair state.json before starting the machine")
    })?;
    let mac = mac_value.parse().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` has invalid persisted MAC address `{mac_value}`"),
        )
        .with_hint("repair state.json before starting the machine")
        .with_source(source)
    })?;

    paths.ensure_machine_runtime_dir(name)?;
    paths.clear_machine_runtime_dir(name, false)?;
    let prepared = (|| -> Result<PreparedStart, FirestoneError> {
        let architecture = image_store.architecture();
        let passt_program = if spec.network.mode == NetMode::Passt {
            if embedded_helper(InternalHelper::Passt).is_some() {
                let artifact = manifest.embedded_passt(architecture.as_str())?;
                match resolve_verified_apparmor_passt(paths, &artifact)? {
                    Some(verified) => verified.executable().to_path_buf(),
                    None => materialize_embedded_helper(paths, InternalHelper::Passt)?.ok_or_else(
                        || {
                            FirestoneError::new(
                                ErrorKind::Dependency,
                                "standalone Firestone has no embedded passt payload",
                            )
                            .with_hint("replace the executable with an intact x86_64 release")
                        },
                    )?,
                }
            } else {
                find_required_program(
                    "passt",
                    "run `firestone doctor` and install the pinned passt",
                )?
            }
        } else {
            PathBuf::from("passt")
        };
        let host = RealValidationHost::new();
        let mut network_options = NetworkPlanOptions::new(
            paths,
            name,
            &spec.network,
            mac,
            passt_program.as_os_str(),
            &host,
        );
        network_options.readiness_timeout = timeouts.readiness;
        network_options.readiness_poll_interval = Duration::from_millis(10).min(timeouts.readiness);
        let network = prepare_network(network_options)?;

        let sandbox = if spec.mounts.is_empty() {
            VirtiofsSandbox::Namespace
        } else {
            let sandbox = detect_virtiofs_sandbox();
            if sandbox == VirtiofsSandbox::None {
                events.emit(Event::Log {
                    level: crate::Level::Warn,
                    message: "user namespaces are unavailable; virtiofsd will use --sandbox none"
                        .to_owned(),
                })?;
            }
            sandbox
        };
        let filesystem_readiness = VirtiofsReadinessPlan::new(
            timeouts.readiness,
            Duration::from_millis(10).min(timeouts.readiness),
        )?;
        let filesystems = prepare_virtiofs_plans_with_readiness(
            paths,
            manifest,
            name,
            architecture,
            &spec.mounts,
            sandbox,
            filesystem_readiness,
        )?;

        events.emit(Event::StepStart {
            id: StepId::from("seed"),
            label: "render cloud-init seed".to_owned(),
        })?;
        if spec.cloud_init.provisioning {
            ensure_ssh_identity(paths)?;
        }
        let seed_existed = paths
            .machine_seed_image(name)?
            .try_exists()
            .map_err(|source| {
                filesystem_error(
                    ErrorKind::Generic,
                    format!("cannot inspect machine {name} cloud-init seed"),
                    source,
                )
            })?;
        let previous_instance_id = state.instance_id.clone();
        let rendered = publish_seed_with_sshd_path(
            paths,
            name,
            spec,
            &prepared_image.image.metadata.sshd_path,
        )?;
        invalidate_known_hosts_for_seed(
            paths,
            name,
            previous_instance_id.as_deref(),
            &rendered.instance_id,
        )?;
        let seed_rewritten =
            !seed_existed || previous_instance_id.as_deref() != Some(rendered.instance_id.as_str());
        if seed_rewritten {
            timeouts.launch_request = timeouts.first_boot_launch_request;
            timeouts.launch_overall = timeouts.first_boot_launch_overall;
        }
        state.instance_id = Some(rendered.instance_id.clone());
        events.emit(Event::StepDone {
            id: StepId::from("seed"),
            detail: Some(format!("instance {}", rendered.instance_id)),
            elapsed_ms: 0,
        })?;

        // §9.5: an OCI machine publishes the pinned direct-boot kernel here
        // instead of a firmware; both go through the same locked publisher.
        if let Some(artifact) = selected_pinned_boot_artifact(
            manifest,
            &spec.vmm.firmware,
            architecture,
            prepared_image.image.firmware,
            prepared_image.image.metadata.kind,
        )? {
            image_store.ensure_pinned_artifact(&artifact)?;
        }
        let config = publish_vm_config(
            paths,
            manifest,
            VmConfigInput {
                name,
                spec,
                state: &state,
                network: &network,
                filesystems: &filesystems,
                architecture,
                catalog_firmware: prepared_image.image.firmware,
                image_kind: prepared_image.image.metadata.kind,
            },
        )?;
        let (vmm_binary, vmm_binary_sha256) =
            resolve_vmm_binary(paths, manifest, architecture, name, spec)?;
        let forwards = network
            .forwards()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let mounts = filesystems
            .iter()
            .map(|plan| format!("{} -> {}", plan.host().display(), plan.guest().display()))
            .collect::<Vec<_>>();
        state.forwards.clone_from(&forwards);
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
            network: network.snapshot()?,
            filesystems: filesystems
                .iter()
                .map(VirtiofsPlan::snapshot)
                .collect::<Result<Vec<_>, _>>()?,
            control_io_timeout_ms: duration_millis(timeouts.control_io, "control_io")?,
            launch_request_timeout_ms: duration_millis(timeouts.launch_request, "launch_request")?,
            launch_overall_timeout_ms: duration_millis(timeouts.launch_overall, "launch_overall")?,
        };
        publish_launch_plan(paths, name, &plan)?;
        StateStore::new(paths.machine_state(name)?).write_from_locked_action(&state, lock)?;
        Ok(PreparedStart {
            name: name.to_owned(),
            state,
            previous_status,
            seed_rewritten,
            timeout: timeouts.launch_overall,
            forwards,
            mounts,
        })
    })();

    match prepared {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            if let Err(cleanup_error) = paths.clear_machine_runtime_dir(name, true) {
                let kind = error.kind();
                let hint = error.hint().map(str::to_owned);
                let message = format!(
                    "{}; failed to clean prepared runtime: {}",
                    error.message(),
                    cleanup_error.message()
                );
                let mut combined = FirestoneError::new(kind, message).with_source(error);
                if let Some(hint) = hint {
                    combined = combined.with_hint(hint);
                }
                return Err(combined);
            }
            Err(error)
        }
    }
}

/// Rolls a prepared-but-unlaunched start back to its prior lifecycle state.
pub fn cancel_prepared(
    paths: &Paths,
    mut prepared: PreparedStart,
    lock: &MachineLock,
) -> Result<(), FirestoneError> {
    validate_machine_lock(paths, &prepared.name, lock)?;
    prepared.state.status = prepared.previous_status;
    prepared.state.shim_pid = None;
    prepared.state.vmm_pid = None;
    prepared.state.started_at = None;
    prepared.state.sidecar_pids.clear();
    prepared.state.degraded.clear();
    StateStore::new(paths.machine_state(&prepared.name)?)
        .write_from_locked_action(&prepared.state, lock)?;
    paths.clear_machine_runtime_dir(&prepared.name, true)
}

fn start_interrupted_error() -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Interrupted,
        "machine start interrupted by SIGINT",
    )
    .with_hint("retry the start command when ready")
}

struct ShimChildReaper {
    sender: std::sync::mpsc::SyncSender<ManagedProcess>,
}

impl ShimChildReaper {
    fn start() -> Result<Self, FirestoneError> {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<ManagedProcess>(1);
        thread::Builder::new()
            .name("firestone-shim-reaper".to_owned())
            .spawn(move || {
                if let Ok(mut process) = receiver.recv() {
                    let _ = process.wait();
                }
            })
            .map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot start shim reaper thread")
                    .with_source(source)
            })?;
        Ok(Self { sender })
    }

    fn submit(self, process: ManagedProcess) -> Result<(), FirestoneError> {
        self.sender.send(process).map_err(|_| {
            FirestoneError::new(
                ErrorKind::Generic,
                "shim reaper thread ended before taking child ownership",
            )
        })
    }
}

/// Spawns `firestone _shim NAME`, hands state ownership to it, and launches.
///
/// The supplied lock is consumed. It protects the two `starting` writes and is
/// released only after the shim pid is durable; the shim then acquires and owns
/// the same lock until its one final state write.
pub fn launch_prepared(
    paths: &Paths,
    shim_program: &Path,
    prepared: PreparedStart,
    lock: MachineLock,
    events: &mut dyn EventSink,
) -> Result<ShimStatus, FirestoneError> {
    let cancellation = AtomicBool::new(false);
    launch_prepared_cancellable(paths, shim_program, prepared, lock, events, &cancellation)
}

/// Launches a prepared machine while allowing the caller to cancel with SIGINT semantics.
pub fn launch_prepared_cancellable(
    paths: &Paths,
    shim_program: &Path,
    mut prepared: PreparedStart,
    lock: MachineLock,
    events: &mut dyn EventSink,
    cancellation: &AtomicBool,
) -> Result<ShimStatus, FirestoneError> {
    validate_machine_lock(paths, &prepared.name, &lock)?;
    if cancellation.load(Ordering::Relaxed) {
        cancel_prepared(paths, prepared, &lock)?;
        return Err(start_interrupted_error());
    }
    let shim_reaper = ShimChildReaper::start()?;
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
    if let Err(error) = wait_for_shim_socket(
        paths,
        &prepared.name,
        &socket,
        &mut process,
        wait_deadline,
        Some(cancellation),
    ) {
        if error.kind() == ErrorKind::Interrupted {
            terminate_cancelled_shim(paths, prepared, &mut process)?;
        } else {
            terminate_unready_shim(paths, &prepared.name, &mut process, &prepared.state, &error)?;
        }
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

    let client_timeout = plan_timeouts
        .launch_overall
        .checked_add(plan_timeouts.control_io)
        .ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "launch client deadline is out of range")
        })?;
    let client = ShimClient::new(socket, client_timeout);
    let launch_result = client.launch_cancellable(events, cancellation);
    if launch_result
        .as_ref()
        .is_err_and(|error| error.kind() == ErrorKind::Interrupted)
        || cancellation.load(Ordering::Relaxed)
    {
        let error = start_interrupted_error();
        terminate_cancelled_shim(paths, prepared, &mut process)?;
        return Err(error);
    }
    shim_reaper.submit(process)?;
    launch_result?;
    let status = ShimClient::new(
        paths.machine_shim_socket(&prepared.name)?,
        plan_timeouts.control_io,
    )
    .status()?;
    Ok(status)
}

/// Starts a replacement shim that strictly adopts an existing VMM.
///
/// The replacement does not rewrite VMM state before the child has verified the
/// previous shim is gone and reconstructed a full process identity.
pub fn recover_shim(
    paths: &Paths,
    name: &str,
    shim_program: &Path,
    events: &mut dyn EventSink,
) -> Result<ShimStatus, FirestoneError> {
    paths.validate_machine_data_directory(name)?;
    paths.validate_machine_runtime_dir(name)?;
    let mut lock_events = Vec::new();
    let lock = MachineLock::acquire(name, &paths.machine_lock(name)?, &mut lock_events)?;
    let state = StateStore::new(paths.machine_state(name)?).read()?;
    if !matches!(
        state.status,
        MachineStatus::Starting | MachineStatus::Running
    ) {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` is not in a recoverable state"),
        ));
    }
    let plan = read_launch_plan(paths, name)?;
    let timeouts = plan.timeouts()?;
    let shim_program = validate_shim_program(shim_program)?;
    let shim_log = paths.machine_shim_log(name)?;
    events.emit(Event::StepStart {
        id: StepId::from("shim-recover"),
        label: "recover machine supervisor".to_owned(),
    })?;
    let reaper = ShimChildReaper::start()?;
    let mut process =
        shim_command(paths, &shim_program, name, &shim_log).spawn_session_candidate()?;
    let shim_pid = process.id();
    drop(lock);
    let socket = paths.machine_shim_socket(name)?;
    let deadline = Instant::now()
        .checked_add(timeouts.launch_request)
        .ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "shim recovery deadline is out of range")
        })?;
    if let Err(error) = wait_for_shim_socket(paths, name, &socket, &mut process, deadline, None) {
        terminate_recovery_candidate(&mut process)?;
        return Err(error);
    }
    if let Err(error) = process.confirm_session() {
        terminate_recovery_candidate(&mut process)?;
        return Err(error);
    }
    reaper.submit(process)?;
    events.emit(Event::StepDone {
        id: StepId::from("shim-recover"),
        detail: Some(format!("pid {shim_pid}")),
        elapsed_ms: 0,
    })?;
    ShimClient::new(socket, timeouts.control_io).status()
}

fn terminate_recovery_candidate(process: &mut ManagedProcess) -> Result<(), FirestoneError> {
    if process.observe_exit()? {
        let _ = process.wait()?;
        return Ok(());
    }
    process.signal_process(ProcessSignal::Kill)?;
    if process.wait_timeout(CHILD_TERM_GRACE)?.is_some() {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!("cannot reap recovery shim pid {}", process.id()),
        ))
    }
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
        let terminal = self.request(br#"{"op":"ping"}"#, None, self.timeout, None)?;
        require_ok_terminal(&terminal)
    }

    pub fn launch(&self, events: &mut dyn EventSink) -> Result<(), FirestoneError> {
        let terminal = self.request(br#"{"op":"launch"}"#, Some(events), self.timeout, None)?;
        require_ok_terminal(&terminal)
    }

    fn launch_cancellable(
        &self,
        events: &mut dyn EventSink,
        cancellation: &AtomicBool,
    ) -> Result<(), FirestoneError> {
        let terminal = self.request(
            br#"{"op":"launch"}"#,
            Some(events),
            self.timeout,
            Some(cancellation),
        )?;
        require_ok_terminal(&terminal)
    }

    pub fn status(&self) -> Result<ShimStatus, FirestoneError> {
        let terminal = self.request(br#"{"op":"status"}"#, None, self.timeout, None)?;
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
        let total = stop_overall_timeout(timeout, self.timeout)?;
        let terminal = self.request(&request, Some(events), total, None)?;
        require_ok_terminal(&terminal)
    }

    fn request(
        &self,
        request: &[u8],
        mut events: Option<&mut dyn EventSink>,
        timeout: Duration,
        cancellation: Option<&AtomicBool>,
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
            let frame = read_frame(
                &mut stream,
                MAX_CONTROL_RESPONSE_BYTES,
                deadline,
                cancellation,
            )?;
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
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
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
    let _lock = MachineLock::acquire(name, &paths.machine_lock(name)?, &mut lock_events)?;
    let mut state = StateStore::new(paths.machine_state(name)?).read()?;
    let pid = std::process::id();

    let plan = read_launch_plan(paths, name)?;
    if plan.name != name {
        return Err(protocol_error(
            "launch plan machine name does not match shim argv",
        ));
    }
    let timeouts = plan.timeouts()?;
    let prior_identity = read_process_identity_optional(paths, name)?;
    let shim_executable = current_executable()?;
    let shim_record = process_record(
        pid,
        pid,
        shim_executable,
        None,
        env::args_os().collect(),
        None,
    )?;
    let normal_launch = state.status == MachineStatus::Starting && state.shim_pid == Some(pid);

    let (
        mut identity,
        recovered_vmm,
        mut recovery_api_pending,
        recovered_from_starting,
        mut recovered_sidecars,
        missing_sidecars,
    ) = if normal_launch {
        (
            ProcessIdentity {
                version: IDENTITY_VERSION,
                shim: shim_record,
                vmm: None,
                sidecars: BTreeMap::new(),
            },
            None,
            false,
            false,
            Vec::new(),
            Vec::new(),
        )
    } else {
        if !matches!(
            state.status,
            MachineStatus::Starting | MachineStatus::Running
        ) {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` is not in a shim-recoverable state"),
            ));
        }
        let prior_status = state.status;
        let (record, api_ready) =
            recover_vmm_record(paths, name, &plan, &state, prior_identity.as_ref(), pid)?;
        let RecoveredSidecarSet {
            records: sidecar_records,
            processes: recovered_sidecars,
            missing: missing_sidecars,
        } = recover_sidecar_processes(&plan, &mut state, prior_identity.as_ref(), paths.uid())?;
        state.shim_pid = Some(pid);
        state.vmm_pid = Some(record.pid);
        if api_ready {
            state.status = MachineStatus::Running;
            set_vmm_api_degraded(&mut state, false);
            if state.started_at.is_none() {
                state.started_at = Some(now_timestamp());
            }
        } else if prior_status == MachineStatus::Running {
            state.status = MachineStatus::Running;
            set_vmm_api_degraded(&mut state, true);
        } else {
            state.status = MachineStatus::Starting;
            set_vmm_api_degraded(&mut state, false);
        }
        if prior_status == MachineStatus::Running {
            for sidecar in &missing_sidecars {
                let marker = format!("{sidecar} exited (status unavailable)");
                if !state.degraded.iter().any(|entry| entry == &marker) {
                    state.degraded.push(marker);
                }
            }
        }
        (
            ProcessIdentity {
                version: IDENTITY_VERSION,
                shim: shim_record,
                vmm: Some(record.clone()),
                sidecars: sidecar_records,
            },
            Some(RecoveredProcess::new("VMM", record, Vec::new())?),
            std::ops::Not::not(api_ready),
            prior_status == MachineStatus::Starting,
            recovered_sidecars,
            missing_sidecars,
        )
    };

    publish_pid_and_identity(paths, name, &identity)?;
    if recovered_vmm.is_some() {
        StateStore::new(paths.machine_state(name)?).write_from_shim(&state)?;
    }

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
    let mut launched = recovered_vmm.is_some();
    let mut vmm: Option<OwnedVmm> = None;
    let mut sidecars: Vec<OwnedSidecar> = Vec::new();
    let recovery_readiness_deadline = if recovery_api_pending && recovered_from_starting {
        Instant::now().checked_add(timeouts.readiness)
    } else {
        None
    };
    let supervisor_result = (|| -> Result<(), FirestoneError> {
        if recovered_from_starting && !missing_sidecars.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "starting recovery lost required sidecar(s): {}",
                    missing_sidecars.join(", ")
                ),
            )
            .with_hint("the incomplete launch will be stopped before retrying"));
        }
        loop {
            if terminating.load(Ordering::Relaxed) {
                let reason = if launched {
                    "shim received a termination signal"
                } else {
                    "shim terminated before launch"
                };
                if let Some(process) = vmm.as_mut() {
                    let mut sink = Vec::new();
                    let operation_deadline = Instant::now()
                        .checked_add(
                            stop_overall_timeout(Duration::from_secs(30), timeouts.control_io)?
                                .saturating_sub(timeouts.control_io),
                        )
                        .ok_or_else(|| {
                            FirestoneError::new(
                                ErrorKind::Usage,
                                "signal stop deadline is out of range",
                            )
                        })?;
                    if let Err(error) = stop_owned_vmm(
                        paths,
                        name,
                        &plan,
                        &mut state,
                        &mut identity,
                        process,
                        &mut sidecars,
                        Duration::from_secs(30),
                        false,
                        operation_deadline,
                        &mut sink,
                    ) {
                        match process.terminate_and_reap_before(
                            true,
                            CHILD_TERM_GRACE
                                .min(operation_deadline.saturating_duration_since(Instant::now())),
                            operation_deadline,
                        ) {
                            Ok(status) => {
                                identity.vmm = None;
                                state.vmm_pid = None;
                                stop_owned_sidecars(
                                    paths,
                                    name,
                                    &mut state,
                                    &mut identity,
                                    &mut sidecars,
                                    operation_deadline,
                                )?;
                                write_final_state(
                                    paths,
                                    name,
                                    &mut state,
                                    MachineStatus::Failed,
                                    Some(status),
                                    ExitReason::Failure(error.message().to_owned()),
                                )?;
                                merge_console_log(paths, name)?;
                                cleanup_after_shim(paths, name)?;
                            }
                            Err(cleanup_error) => {
                                write_recoverable_failed_state(
                                    paths,
                                    name,
                                    &mut state,
                                    process.record(),
                                    &error,
                                )?;
                                write_shim_log(&format!(
                                    "machine `{name}` cleanup failed ({}); runtime recovery evidence retained",
                                    cleanup_error.kind()
                                ));
                                return Err(cleanup_error);
                            }
                        }
                    } else {
                        merge_console_log(paths, name)?;
                        cleanup_after_shim(paths, name)?;
                    }
                } else if let Some(recovered) = recovered_vmm.as_ref() {
                    let record = recovered.record().clone();
                    let total = stop_overall_timeout(Duration::from_secs(30), timeouts.control_io)?;
                    let operation_deadline = Instant::now()
                        .checked_add(total.saturating_sub(timeouts.control_io))
                        .ok_or_else(|| {
                            FirestoneError::new(
                                ErrorKind::Usage,
                                "signal stop deadline is out of range",
                            )
                        })?;
                    let mut sink = Vec::new();
                    if let Err(error) = stop_recovered_vmm(
                        paths,
                        name,
                        &plan,
                        &mut state,
                        &mut identity,
                        &record,
                        &mut recovered_sidecars,
                        Duration::from_secs(30),
                        false,
                        operation_deadline,
                        &mut sink,
                    ) {
                        let cleanup = signal_verified_tree(&record, ProcessSignal::Kill)
                            .and_then(|()| wait_for_record_exit(&record, CHILD_TERM_GRACE));
                        match cleanup {
                            Ok(()) => {
                                identity.vmm = None;
                                state.vmm_pid = None;
                                stop_recovered_sidecars(
                                    paths,
                                    name,
                                    &mut state,
                                    &mut identity,
                                    &mut recovered_sidecars,
                                    operation_deadline,
                                )?;
                                write_final_state(
                                    paths,
                                    name,
                                    &mut state,
                                    MachineStatus::Failed,
                                    None,
                                    ExitReason::Failure(error.message().to_owned()),
                                )?;
                                merge_console_log(paths, name)?;
                                cleanup_after_shim(paths, name)?;
                            }
                            Err(cleanup_error) => {
                                write_recoverable_failed_state(
                                    paths, name, &mut state, &record, &error,
                                )?;
                                return Err(cleanup_error);
                            }
                        }
                    } else {
                        merge_console_log(paths, name)?;
                        cleanup_after_shim(paths, name)?;
                    }
                } else {
                    write_final_state(
                        paths,
                        name,
                        &mut state,
                        MachineStatus::Failed,
                        None,
                        ExitReason::Failure(reason.to_owned()),
                    )?;
                    cleanup_after_shim(paths, name)?;
                }

                return Ok(());
            }

            if recovery_api_pending {
                let recovered = recovered_vmm.as_ref().ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Generic,
                        "pending recovery lost its process identity",
                    )
                })?;
                let record = recovered.record();
                if recovered_from_starting
                    && recovery_readiness_deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    let timeout_error = FirestoneError::new(
                        ErrorKind::Timeout,
                        format!(
                            "recovered VMM API for machine `{name}` did not become ready before its deadline"
                        ),
                    );
                    match terminate_recovered_process(record) {
                        Ok(()) => {
                            identity.vmm = None;
                            state.vmm_pid = None;
                            let sidecar_deadline = Instant::now()
                                .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
                                .unwrap_or_else(Instant::now);
                            stop_recovered_sidecars(
                                paths,
                                name,
                                &mut state,
                                &mut identity,
                                &mut recovered_sidecars,
                                sidecar_deadline,
                            )?;
                            write_final_state(
                                paths,
                                name,
                                &mut state,
                                MachineStatus::Failed,
                                None,
                                ExitReason::Failure(timeout_error.message().to_owned()),
                            )?;
                            merge_console_log(paths, name)?;
                            cleanup_after_shim(paths, name)?;
                            return Err(timeout_error);
                        }
                        Err(cleanup_error) => {
                            identity.vmm = Some(record.clone());
                            publish_process_identity(paths, name, &identity)?;
                            write_recoverable_failed_state(
                                paths,
                                name,
                                &mut state,
                                record,
                                &timeout_error,
                            )?;
                            return Err(cleanup_error);
                        }
                    }
                }
                let mut api_timeout = timeouts.api.min(Duration::from_millis(100));
                if let Some(deadline) = recovery_readiness_deadline {
                    api_timeout =
                        api_timeout.min(deadline.saturating_duration_since(Instant::now()));
                }
                if !api_timeout.is_zero() {
                    if let Ok(ping) =
                        VmmApi::new(&paths.machine_api_socket(name)?, api_timeout).vmm_ping()
                    {
                        if ping.pid == i64::from(record.pid) {
                            recovery_api_pending = false;
                            state.status = MachineStatus::Running;
                            set_vmm_api_degraded(&mut state, false);
                            if state.started_at.is_none() {
                                state.started_at = Some(now_timestamp());
                            }
                            StateStore::new(paths.machine_state(name)?).write_from_shim(&state)?;
                            write_shim_log(&format!(
                                "machine `{name}` recovered VMM API became ready"
                            ));
                        }
                    }
                }
            }

            if !launched && Instant::now() >= launch_deadline {
                let error = FirestoneError::new(
                    ErrorKind::Timeout,
                    format!(
                        "shim for machine `{name}` received no launch request before its deadline"
                    ),
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
                return Err(error);
            }

            let owned_exited = match vmm.as_ref() {
                Some(process) => process.observe_exit()?,
                None => false,
            };
            if owned_exited {
                let process = vmm.as_mut().ok_or_else(|| {
                    FirestoneError::new(ErrorKind::Generic, "observed VMM owner disappeared")
                })?;
                let status = process.reap_exited_group()?;
                write_shim_log(&format!("machine `{name}` VMM exited unexpectedly"));
                let reason = safe_vmm_failure_reason(paths, name, "VMM exited unexpectedly");
                identity.vmm = None;
                state.vmm_pid = None;
                let sidecar_deadline = Instant::now()
                    .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
                    .unwrap_or_else(Instant::now);
                stop_owned_sidecars(
                    paths,
                    name,
                    &mut state,
                    &mut identity,
                    &mut sidecars,
                    sidecar_deadline,
                )?;
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
                return Ok(());
            }

            let recovered_exited = match recovered_vmm.as_ref() {
                Some(recovered) => !recovered.is_alive()?,
                None => false,
            };
            if recovered_exited {
                write_shim_log(&format!(
                    "machine `{name}` recovered VMM exited unexpectedly"
                ));
                let reason =
                    safe_vmm_failure_reason(paths, name, "recovered VMM exited unexpectedly");
                identity.vmm = None;
                state.vmm_pid = None;
                let sidecar_deadline = Instant::now()
                    .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
                    .unwrap_or_else(Instant::now);
                stop_owned_sidecars(
                    paths,
                    name,
                    &mut state,
                    &mut identity,
                    &mut sidecars,
                    sidecar_deadline,
                )?;
                stop_recovered_sidecars(
                    paths,
                    name,
                    &mut state,
                    &mut identity,
                    &mut recovered_sidecars,
                    sidecar_deadline,
                )?;
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
                return Ok(());
            }

            if state.status == MachineStatus::Running {
                reconcile_owned_sidecar_exits(
                    paths,
                    name,
                    &mut state,
                    &mut identity,
                    &mut sidecars,
                )?;
                reconcile_recovered_sidecar_exits(
                    paths,
                    name,
                    &mut state,
                    &mut identity,
                    &mut recovered_sidecars,
                )?;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(source) = stream.set_nonblocking(true) {
                        let error = filesystem_error(
                            ErrorKind::Generic,
                            format!("cannot make machine `{name}` control connection nonblocking"),
                            source,
                        );
                        write_shim_log(&format!(
                            "machine `{name}` control connection setup failed ({}); connection dropped",
                            error.kind()
                        ));
                        continue;
                    }
                    if let Err(error) = authorize_peer(&stream, paths.uid()) {
                        isolate_client_write(
                            name,
                            "authorization error",
                            write_terminal_error(stream, &error, timeouts.control_io),
                        );
                        continue;
                    }
                    let request = match read_request(&stream, timeouts.control_io) {
                        Ok(request) => request,
                        Err(error) => {
                            isolate_client_write(
                                name,
                                "request error",
                                write_terminal_error(stream, &error, timeouts.control_io),
                            );
                            continue;
                        }
                    };
                    match request {
                        ControlRequest::Ping => {
                            isolate_client_write(
                                name,
                                "ping",
                                write_terminal_ok(stream, timeouts.control_io),
                            );
                        }
                        ControlRequest::Status => {
                            let pids = status_pids(&state, pid);
                            isolate_client_write(
                                name,
                                "status",
                                write_status(stream, &state, &pids, timeouts.control_io),
                            );
                        }
                        ControlRequest::Launch if launched => {
                            let error = FirestoneError::new(
                                ErrorKind::AlreadyRunning,
                                format!("machine `{name}` launch was already requested"),
                            );
                            isolate_client_write(
                                name,
                                "duplicate launch",
                                write_terminal_error(stream, &error, timeouts.control_io),
                            );
                        }
                        ControlRequest::Launch => {
                            launched = true;
                            let operation_deadline = Instant::now()
                                .checked_add(timeouts.launch_overall)
                                .ok_or_else(|| {
                                    FirestoneError::new(
                                        ErrorKind::Usage,
                                        "overall launch deadline is out of range",
                                    )
                                })?;
                            let terminal_deadline = operation_deadline
                                .checked_add(timeouts.control_io)
                                .ok_or_else(|| {
                                    FirestoneError::new(
                                        ErrorKind::Usage,
                                        "launch response deadline is out of range",
                                    )
                                })?;
                            let mut sink =
                                ProtocolSink::with_deadline(stream, terminal_deadline, name);
                            let launch_result = catch_unwind(AssertUnwindSafe(|| {
                                launch_vmm(
                                    paths,
                                    name,
                                    &plan,
                                    &mut state,
                                    &mut identity,
                                    &terminating,
                                    &mut sink,
                                    operation_deadline,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(FirestoneError::new(
                                    ErrorKind::Generic,
                                    format!("machine `{name}` launch panicked"),
                                ))
                            });
                            match launch_result {
                                Ok((process, launched_sidecars)) => {
                                    sink.terminal_ok();
                                    vmm = Some(process);
                                    sidecars = launched_sidecars;
                                }

                                Err(error) => {
                                    let sidecar_survivors = match find_launch_sidecar_survivors(
                                        &plan,
                                        &identity,
                                        paths.uid(),
                                    ) {
                                        Ok(survivors) => survivors,
                                        Err(probe_error) => {
                                            write_ambiguous_failed_state(
                                                paths, name, &mut state, &error,
                                            )?;
                                            write_shim_log(&format!(
                                                "machine `{name}` launch cleanup could not prove sidecar absence ({}); runtime retained",
                                                probe_error.kind()
                                            ));
                                            sink.terminal_error(&error);
                                            return Err(error);
                                        }
                                    };
                                    if !sidecar_survivors.is_empty() {
                                        identity.sidecars = sidecar_survivors;
                                        state.sidecar_pids = identity
                                            .sidecars
                                            .iter()
                                            .map(|(sidecar, record)| (sidecar.clone(), record.pid))
                                            .collect();
                                        publish_process_identity(paths, name, &identity)?;
                                        write_ambiguous_failed_state(
                                            paths, name, &mut state, &error,
                                        )?;
                                        write_shim_log(&format!(
                                            "machine `{name}` launch failed with verified live sidecars; runtime evidence retained"
                                        ));
                                        sink.terminal_error(&error);
                                        return Err(error);
                                    }
                                    match find_launch_survivor(
                                        paths, name, &plan, &state, &identity,
                                    ) {
                                        Ok(Some(record)) => {
                                            identity.vmm = Some(record.clone());
                                            publish_process_identity(paths, name, &identity)?;
                                            write_recoverable_failed_state(
                                                paths, name, &mut state, &record, &error,
                                            )?;
                                            write_shim_log(&format!(
                                                "machine `{name}` launch failed with live VMM pid {}; runtime evidence retained",
                                                record.pid
                                            ));
                                        }
                                        Ok(None) => {
                                            let reason = safe_vmm_failure_reason(
                                                paths,
                                                name,
                                                error.message(),
                                            );
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
                                        }
                                        Err(probe_error) => {
                                            write_ambiguous_failed_state(
                                                paths, name, &mut state, &error,
                                            )?;
                                            write_shim_log(&format!(
                                                "machine `{name}` launch cleanup could not prove VMM absence ({}); runtime retained",
                                                probe_error.kind()
                                            ));
                                        }
                                    }
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
                                isolate_client_write(
                                    name,
                                    "invalid stop",
                                    write_terminal_error(stream, &error, timeouts.control_io),
                                );
                                continue;
                            }

                            let guest_timeout = Duration::from_secs(timeout_s);
                            let total = stop_overall_timeout(guest_timeout, timeouts.control_io)?;
                            let terminal_deadline =
                                Instant::now().checked_add(total).ok_or_else(|| {
                                    FirestoneError::new(
                                        ErrorKind::Usage,
                                        "stop deadline is out of range",
                                    )
                                })?;
                            let operation_deadline = terminal_deadline
                                .checked_sub(timeouts.control_io)
                                .ok_or_else(|| {
                                    FirestoneError::new(
                                        ErrorKind::Usage,
                                        "stop operation deadline is out of range",
                                    )
                                })?;
                            let mut sink =
                                ProtocolSink::with_deadline(stream, terminal_deadline, name);
                            let status_before_stop = state.status;
                            let result = if let Some(process) = vmm.as_mut() {
                                stop_owned_vmm(
                                    paths,
                                    name,
                                    &plan,
                                    &mut state,
                                    &mut identity,
                                    process,
                                    &mut sidecars,
                                    guest_timeout,
                                    force,
                                    operation_deadline,
                                    &mut sink,
                                )
                            } else if let Some(recovered) = recovered_vmm.as_ref() {
                                stop_recovered_vmm(
                                    paths,
                                    name,
                                    &plan,
                                    &mut state,
                                    &mut identity,
                                    recovered.record(),
                                    &mut recovered_sidecars,
                                    guest_timeout,
                                    force,
                                    operation_deadline,
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
                                    sink.terminal_ok();
                                    return Ok(());
                                }

                                Err(error) => {
                                    let mut runtime_alive = false;
                                    let mut completed_status = None;
                                    let mut completed_without_status = false;
                                    if let Some(process) = vmm.as_mut() {
                                        if process.is_fully_reaped() {
                                            completed_status = process.retained_status();
                                        } else if process.observe_exit()? {
                                            completed_status = Some(process.reap_exited_group()?);
                                        } else {
                                            runtime_alive = true;
                                        }
                                    } else if let Some(recovered) = recovered_vmm.as_ref() {
                                        if recovered.is_alive()? {
                                            runtime_alive = true;
                                        } else {
                                            completed_without_status = true;
                                        }
                                    }
                                    if runtime_alive {
                                        state.status = status_before_stop;
                                        StateStore::new(paths.machine_state(name)?)
                                            .write_from_shim(&state)?;
                                        sink.terminal_error(&error);
                                    } else if completed_status.is_some() || completed_without_status
                                    {
                                        identity.vmm = None;
                                        state.vmm_pid = None;
                                        stop_owned_sidecars(
                                            paths,
                                            name,
                                            &mut state,
                                            &mut identity,
                                            &mut sidecars,
                                            operation_deadline,
                                        )?;
                                        stop_recovered_sidecars(
                                            paths,
                                            name,
                                            &mut state,
                                            &mut identity,
                                            &mut recovered_sidecars,
                                            operation_deadline,
                                        )?;
                                        write_final_state(
                                            paths,
                                            name,
                                            &mut state,
                                            MachineStatus::Stopped,
                                            completed_status,
                                            ExitReason::GuestShutdown,
                                        )?;
                                        merge_console_log(paths, name)?;
                                        cleanup_after_shim(paths, name)?;
                                        sink.terminal_ok();
                                        return Ok(());
                                    } else {
                                        sink.terminal_error(&error);
                                    }
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
    })();
    finalize_supervisor_result(
        paths,
        name,
        &mut state,
        &mut identity,
        &mut vmm,
        &mut sidecars,
        &mut recovered_sidecars,
        recovered_vmm.as_ref(),
        supervisor_result,
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_supervisor_result(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    owned_vmm: &mut Option<OwnedVmm>,
    owned_sidecars: &mut Vec<OwnedSidecar>,
    recovered_sidecars: &mut Vec<RecoveredProcess>,
    recovered_vmm: Option<&RecoveredProcess>,
    result: Result<(), FirestoneError>,
) -> Result<(), FirestoneError> {
    let primary = match result {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if state.shim_pid != Some(std::process::id()) {
        return Err(primary);
    }
    let runtime = paths.machine_runtime_dir(name)?;
    if !runtime.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect machine `{name}` runtime during final cleanup"),
            source,
        )
    })? {
        return Err(primary);
    }
    write_shim_log(&format!(
        "machine `{name}` supervisor failed ({}); entering final cleanup",
        primary.kind()
    ));

    let retained_record = owned_vmm
        .as_ref()
        .map(|vmm| vmm.record().clone())
        .or_else(|| recovered_vmm.map(|vmm| vmm.record().clone()))
        .or_else(|| identity.vmm.clone());
    let vmm_cleanup = if let Some(vmm) = owned_vmm.as_mut() {
        vmm.terminate_and_reap(true, CHILD_TERM_GRACE).map(Some)
    } else if let Some(record) = retained_record.as_ref() {
        match recorded_process_alive(record) {
            Ok(true) => signal_verified_tree(record, ProcessSignal::Kill)
                .and_then(|()| wait_for_record_exit(record, CHILD_TERM_GRACE))
                .map(|()| None),
            Ok(false) => Ok(None),
            Err(error) => Err(error),
        }
    } else {
        Ok(None)
    };

    let sidecar_deadline = Instant::now()
        .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
        .unwrap_or_else(Instant::now);
    let sidecar_cleanup = stop_owned_sidecars(
        paths,
        name,
        state,
        identity,
        owned_sidecars,
        sidecar_deadline,
    );

    let recovered_sidecar_cleanup = stop_recovered_sidecars(
        paths,
        name,
        state,
        identity,
        recovered_sidecars,
        sidecar_deadline,
    );
    let status = match vmm_cleanup {
        Ok(status) => {
            identity.vmm = None;
            state.vmm_pid = None;
            status
        }
        Err(cleanup_error) => {
            if let Some(record) = retained_record.as_ref() {
                identity.vmm = Some(record.clone());
                publish_process_identity(paths, name, identity)?;
                write_recoverable_failed_state(paths, name, state, record, &primary)?;
            } else {
                write_ambiguous_failed_state(paths, name, state, &primary)?;
            }
            write_shim_log(&format!(
                "machine `{name}` final VMM cleanup failed ({}); recovery evidence retained",
                cleanup_error.kind()
            ));
            return Err(cleanup_error);
        }
    };
    if let Err(cleanup_error) = sidecar_cleanup {
        publish_process_identity(paths, name, identity)?;
        write_ambiguous_failed_state(paths, name, state, &primary)?;
        write_shim_log(&format!(
            "machine `{name}` final sidecar cleanup failed ({}); recovery evidence retained",
            cleanup_error.kind()
        ));
        return Err(cleanup_error);
    }

    if let Err(cleanup_error) = recovered_sidecar_cleanup {
        publish_process_identity(paths, name, identity)?;
        write_ambiguous_failed_state(paths, name, state, &primary)?;
        write_shim_log(&format!(
            "machine `{name}` final recovered-sidecar cleanup failed ({}); recovery evidence retained",
            cleanup_error.kind()
        ));
        return Err(cleanup_error);
    }

    write_final_state(
        paths,
        name,
        state,
        MachineStatus::Failed,
        status,
        ExitReason::Failure(primary.message().to_owned()),
    )?;
    let merge_error = merge_console_log(paths, name).err();
    cleanup_after_shim(paths, name)?;
    if let Some(error) = merge_error {
        return Err(error);
    }
    Err(primary)
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
    let plan = read_launch_plan(paths, name)?;
    let prior_identity = read_process_identity_optional(paths, name)?;
    if let Some(shim_pid) = state.shim_pid {
        #[cfg(target_os = "linux")]
        {
            if matches!(process_state(shim_pid)?, Some(process_state) if process_state != 'Z') {
                let shim_record = prior_identity
                    .as_ref()
                    .map(|identity| &identity.shim)
                    .filter(|record| record.pid == shim_pid)
                    .ok_or_else(|| {
                        FirestoneError::new(
                            ErrorKind::Conflict,
                            format!("cannot prove recorded shim pid {shim_pid} stale"),
                        )
                    })?;
                verify_linux_process(shim_record)?;
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

    #[cfg(target_os = "linux")]
    let (record, mut recovery_identity, mut recovered_sidecars) = {
        let mut identity = prior_identity.clone().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has no durable process identity"),
            )
            .with_hint("refusing unsupervised signals without exact process evidence")
        })?;
        let recovered = recover_sidecar_processes(&plan, &mut state, Some(&identity), paths.uid())?;
        let mut recovered_sidecars = recovered.processes;
        identity.sidecars = recovered.records;
        let prior_vmm_dead = match identity.vmm.as_ref() {
            Some(record) => !recorded_process_alive(record)?,
            None => false,
        };
        if prior_vmm_dead {
            let deadline = Instant::now()
                .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
                .unwrap_or_else(Instant::now);
            stop_recovered_sidecars(
                paths,
                name,
                &mut state,
                &mut identity,
                &mut recovered_sidecars,
                deadline,
            )?;
            state.status = MachineStatus::Stopped;
            state.shim_pid = None;
            state.vmm_pid = None;
            state.sidecar_pids.clear();
            state.started_at = None;
            state.degraded.clear();
            state.last_exit = Some(last_exit(None, ExitReason::GuestShutdown));
            StateStore::new(paths.machine_state(name)?).write_from_locked_action(&state, lock)?;
            paths.clear_machine_runtime_dir(name, true)?;
            return Ok(state);
        }
        let record = recover_linux_vmm_record(paths, name, &plan, &state, Some(&identity), 0)?.0;
        (record, identity, recovered_sidecars)
    };
    #[cfg(not(target_os = "linux"))]
    let (record, mut recovery_identity, mut recovered_sidecars) = {
        let identity = prior_identity.ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has no recorded process identity"),
            )
        })?;
        let record = identity.vmm.clone().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` has no recorded VMM identity"),
            )
        })?;
        (record, identity, Vec::<RecoveredProcess>::new())
    };

    let preserved_sidecars =
        preserved_process_roots(recovered_sidecars.iter().map(RecoveredProcess::record));

    if state
        .vmm_pid
        .is_some_and(|recorded_pid| recorded_pid != record.pid)
    {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` VMM pid does not match its process identity"),
        ));
    }
    state.vmm_pid = Some(record.pid);
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
    let plan_timeouts = plan.timeouts()?;
    let total = stop_overall_timeout(timeout, plan_timeouts.control_io)?;
    let overall_deadline = Instant::now()
        .checked_add(total.saturating_sub(plan_timeouts.control_io))
        .ok_or_else(|| FirestoneError::new(ErrorKind::Usage, "stop deadline is out of range"))?;
    let api_cap = plan_timeouts.api.min(STOP_API_PHASE_CAP);
    let mut reason = ExitReason::GuestShutdown;
    if force {
        signal_verified_group(&record, ProcessSignal::Kill, &preserved_sidecars)?;
        reason = ExitReason::Failure("forced stop".to_owned());
    } else {
        let power_timeout = stop_phase_timeout(overall_deadline, api_cap)?;
        let graceful = VmmApi::new(&api_socket, power_timeout).vm_power_button();
        let guest_deadline = Instant::now()
            .checked_add(timeout)
            .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
        if graceful.is_ok() {
            while Instant::now() < guest_deadline && recorded_process_alive(&record)? {
                let Ok(phase_timeout) = stop_phase_timeout(overall_deadline, api_cap) else {
                    break;
                };
                match VmmApi::new(&api_socket, phase_timeout).vm_info() {
                    Ok(info) if info.state == VmState::Shutdown => {
                        if let Ok(shutdown_timeout) = stop_phase_timeout(overall_deadline, api_cap)
                        {
                            let _ = VmmApi::new(&api_socket, shutdown_timeout).vmm_shutdown();
                        }
                    }
                    Ok(_) | Err(_) => {}
                }
                thread::sleep(
                    LOOP_INTERVAL.min(guest_deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        if recorded_process_alive(&record)? {
            signal_verified_group(&record, ProcessSignal::Terminate, &preserved_sidecars)?;
            let term_deadline = Instant::now()
                .checked_add(CHILD_TERM_GRACE)
                .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
            while Instant::now() < term_deadline && recorded_process_alive(&record)? {
                thread::sleep(
                    LOOP_INTERVAL.min(term_deadline.saturating_duration_since(Instant::now())),
                );
            }
            reason = ExitReason::Failure(if graceful.is_err() {
                "VMM API failed during graceful stop".to_owned()
            } else {
                "graceful stop timed out".to_owned()
            });
        }
        if recorded_process_alive(&record)? {
            signal_verified_group(&record, ProcessSignal::Kill, &preserved_sidecars)?;
        }
    }
    wait_for_record_exit(
        &record,
        overall_deadline.saturating_duration_since(Instant::now()),
    )?;

    recovery_identity.vmm = None;
    state.vmm_pid = None;
    stop_recovered_sidecars(
        paths,
        name,
        &mut state,
        &mut recovery_identity,
        &mut recovered_sidecars,
        overall_deadline,
    )?;
    state.status = MachineStatus::Stopped;
    state.shim_pid = None;
    state.vmm_pid = None;
    state.sidecar_pids.clear();
    state.started_at = None;
    state.degraded.clear();
    state.last_exit = Some(last_exit(None, reason));
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

fn vmm_launch_binding(plan: &LaunchPlan) -> String {
    format!(
        "{}:{}:{}",
        plan.name, plan.vmm_binary_sha256, plan.vmconfig_sha256
    )
}

fn vmm_argv(paths: &Paths, name: &str, plan: &LaunchPlan) -> Result<Vec<OsString>, FirestoneError> {
    let mut argv = Vec::with_capacity(5 + plan.vmm_extra_args.len());
    argv.push(plan.vmm_binary.as_os_str().to_os_string());
    argv.push(OsString::from("--api-socket"));
    argv.push(paths.machine_api_socket(name)?.into_os_string());
    argv.push(OsString::from("--log-file"));
    argv.push(paths.machine_vmm_log(name)?.into_os_string());
    argv.extend(plan.vmm_extra_args.iter().map(OsString::from));
    Ok(argv)
}

fn preflight_launch_argv(argv: &[OsString]) -> Result<(), FirestoneError> {
    let encoded_bytes = argv.iter().try_fold(0_usize, |total, argument| {
        total.checked_add(argument.as_os_str().as_bytes().len().saturating_mul(2) + 8)
    });
    let Some(encoded_bytes) = encoded_bytes else {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "VMM argv size is out of range",
        ));
    };
    let identity_limit = usize::try_from(MAX_PROCESS_IDENTITY_BYTES).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Generic,
            "process identity limit does not fit usize",
        )
    })?;
    if encoded_bytes.saturating_add(64 * 1024) > identity_limit {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "VMM argv cannot fit the durable process identity record",
        ));
    }
    Ok(())
}

fn capture_launch_process_record(
    vmm: &mut OwnedVmm,
    plan: &LaunchPlan,
    launch_argv: &[OsString],
    launch_binding: &str,
    work_deadline: Instant,
) -> Result<ProcessRecord, FirestoneError> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(250))
        .map_or(work_deadline, |deadline| deadline.min(work_deadline));
    loop {
        match process_record(
            vmm.id(),
            vmm.record.process_group,
            plan.vmm_binary.clone(),
            Some(plan.vmm_binary_sha256.clone()),
            launch_argv.to_vec(),
            Some(launch_binding.to_owned()),
        ) {
            Ok(record) => return Ok(record),
            Err(error) => {
                if vmm.observe_exit()? {
                    let status = vmm.reap_exited_group()?;
                    return Err(vmm.exit_error(
                        status,
                        &format!(
                            "cloud-hypervisor for machine `{}` exited before process identity capture",
                            plan.name
                        ),
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
fn sidecar_argv(command: &Cmd) -> Vec<OsString> {
    std::iter::once(command.program().to_os_string())
        .chain(command.arguments().map(OsStr::to_os_string))
        .collect()
}

fn sidecar_launch_binding(plan: &LaunchPlan, sidecar: &str, executable_sha256: &str) -> String {
    format!(
        "{}:{}:{}:{}",
        plan.name, sidecar, plan.vmconfig_sha256, executable_sha256
    )
}

#[cfg(target_os = "linux")]
struct SidecarExpectation {
    name: String,
    program: PathBuf,
    argv: Vec<OsString>,
    executable_sha256: String,
    launch_binding: String,
    artifacts: Vec<RuntimeArtifact>,
}

#[cfg(target_os = "linux")]
fn sidecar_expectations(
    plan: &LaunchPlan,
    uid: u32,
) -> Result<Vec<SidecarExpectation>, FirestoneError> {
    let network = plan.network_plan()?;
    let filesystems = plan.filesystem_plans()?;
    let mut expectations = Vec::with_capacity(
        usize::from(matches!(network, NetworkPlan::Passt(_))).saturating_add(filesystems.len()),
    );
    if let NetworkPlan::Passt(passt) = &network {
        let program = PathBuf::from(passt.command().program());
        let executable_sha256 = hash_file(&program, MAX_EXECUTABLE_BYTES, "passt executable")?;
        expectations.push(SidecarExpectation {
            name: "passt".to_owned(),
            argv: sidecar_argv(passt.command()),
            launch_binding: sidecar_launch_binding(plan, "passt", &executable_sha256),
            artifacts: passt_artifacts(passt),
            program,
            executable_sha256,
        });
    }
    for filesystem in &filesystems {
        let name = format!("virtiofsd-{}", filesystem.index());
        let program = filesystem.program().to_path_buf();
        let executable_sha256 = hash_file(&program, MAX_EXECUTABLE_BYTES, "virtiofsd executable")?;
        expectations.push(SidecarExpectation {
            argv: sidecar_argv(&filesystem.command()),
            launch_binding: sidecar_launch_binding(plan, &name, &executable_sha256),
            artifacts: virtiofs_artifacts(filesystem, uid),
            name,
            program,
            executable_sha256,
        });
    }
    Ok(expectations)
}

#[cfg(target_os = "linux")]
fn process_record_matches_sidecar(
    record: &ProcessRecord,
    expectation: &SidecarExpectation,
) -> bool {
    record.launch_artifact.as_deref() == Some(expectation.program.as_path())
        && record.launch_sha256.as_deref() == Some(expectation.executable_sha256.as_str())
        && record.launch_binding.as_deref() == Some(expectation.launch_binding.as_str())
        && record.launch_argv_hex.as_deref() == Some(encode_os_argv(&expectation.argv).as_slice())
}

#[cfg(target_os = "linux")]
fn scan_linux_sidecar_candidates(
    expectation: &SidecarExpectation,
) -> Result<Vec<ProcessRecord>, FirestoneError> {
    let mut records = Vec::new();
    let entries = fs::read_dir("/proc").map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            "cannot enumerate Linux processes for sidecar recovery".to_owned(),
            source,
        )
    })?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.uid() != nix::unistd::getuid().as_raw() {
            continue;
        }
        let binding_matches = match linux_process_environment_has(
            pid,
            "FIRESTONE_LAUNCH_BINDING",
            &expectation.launch_binding,
        ) {
            Ok(matches) => matches,
            Err(_) => continue,
        };
        if !binding_matches {
            continue;
        }
        let expected_group = match i32::try_from(pid) {
            Ok(group) => group,
            Err(_) => continue,
        };
        let group = match nix::unistd::getpgid(Some(pid_from_u32(pid)?)) {
            Ok(group) if group.as_raw() == expected_group => pid,
            _ => continue,
        };
        let record = match process_record(
            pid,
            group,
            expectation.program.clone(),
            Some(expectation.executable_sha256.clone()),
            expectation.argv.clone(),
            Some(expectation.launch_binding.clone()),
        ) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if process_record_matches_sidecar(&record, expectation)
            && verify_linux_process(&record).is_ok()
        {
            records.push(record);
        }
    }
    records.sort_by_key(|record| record.pid);
    Ok(records)
}

struct RecoveredSidecarSet {
    records: BTreeMap<String, ProcessRecord>,
    processes: Vec<RecoveredProcess>,
    missing: Vec<String>,
}

#[cfg(target_os = "linux")]
fn recover_sidecar_processes(
    plan: &LaunchPlan,
    state: &mut MachineState,
    prior_identity: Option<&ProcessIdentity>,
    uid: u32,
) -> Result<RecoveredSidecarSet, FirestoneError> {
    let expectations = sidecar_expectations(plan, uid)?;
    let expected_names = expectations
        .iter()
        .map(|expectation| (expectation.name.clone(), ()))
        .collect::<BTreeMap<_, _>>();
    for name in state.sidecar_pids.keys() {
        if !expected_names.contains_key(name.as_str()) {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("state records unexpected sidecar `{name}`"),
            ));
        }
    }
    if let Some(identity) = prior_identity {
        for name in identity.sidecars.keys() {
            if !expected_names.contains_key(name.as_str()) {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("process identity records unexpected sidecar `{name}`"),
                ));
            }
        }
    }

    let previous_pids = state.sidecar_pids.clone();
    let mut records = BTreeMap::new();
    let mut recovered = Vec::new();
    let mut missing = Vec::new();
    for expectation in expectations {
        let mut candidates = Vec::new();
        if let Some(record) =
            prior_identity.and_then(|identity| identity.sidecars.get(&expectation.name))
        {
            if recorded_process_alive(record)? {
                if !process_record_matches_sidecar(record, &expectation) {
                    return Err(FirestoneError::new(
                        ErrorKind::Conflict,
                        format!(
                            "recorded {} process does not match its launch plan",
                            expectation.name
                        ),
                    ));
                }
                verify_linux_process(record)?;
                candidates.push(record.clone());
            }
        }
        for record in scan_linux_sidecar_candidates(&expectation)? {
            if !candidates
                .iter()
                .any(|candidate| candidate.pid == record.pid)
            {
                candidates.push(record);
            }
        }
        if let Some(recorded_pid) = previous_pids.get(&expectation.name) {
            candidates.retain(|record| record.pid == *recorded_pid);
        }
        if candidates.len() > 1 {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "sidecar recovery found {} matching {} processes",
                    candidates.len(),
                    expectation.name
                ),
            ));
        }
        if let Some(record) = candidates.pop() {
            verify_linux_process(&record)?;
            state
                .sidecar_pids
                .insert(expectation.name.clone(), record.pid);
            records.insert(expectation.name.clone(), record.clone());
            recovered.push(RecoveredProcess::new(
                expectation.name,
                record,
                expectation.artifacts,
            )?);
        } else {
            state.sidecar_pids.remove(&expectation.name);
            if previous_pids.contains_key(&expectation.name)
                || prior_identity
                    .is_some_and(|identity| identity.sidecars.contains_key(&expectation.name))
            {
                missing.push(expectation.name);
            }
        }
    }
    Ok(RecoveredSidecarSet {
        records,
        processes: recovered,
        missing,
    })
}

#[cfg(not(target_os = "linux"))]
fn recover_sidecar_processes(
    plan: &LaunchPlan,
    state: &mut MachineState,
    prior_identity: Option<&ProcessIdentity>,
    uid: u32,
) -> Result<RecoveredSidecarSet, FirestoneError> {
    let _ = (state, prior_identity, uid);
    let has_sidecars = matches!(plan.network_plan()?, NetworkPlan::Passt(_))
        || !plan.filesystem_plans()?.is_empty();
    if has_sidecars {
        Err(FirestoneError::new(
            ErrorKind::Dependency,
            "sidecar recovery requires the audited Linux process identity backend",
        ))
    } else {
        Ok(RecoveredSidecarSet {
            records: BTreeMap::new(),
            processes: Vec::new(),
            missing: Vec::new(),
        })
    }
}

#[cfg(target_os = "linux")]
fn process_executable_access_denied(pid: u32) -> bool {
    fs::canonicalize(PathBuf::from("/proc").join(pid.to_string()).join("exe"))
        .is_err_and(|source| source.kind() == io::ErrorKind::PermissionDenied)
}

/// Records the immutable spawn identity after a managed sidecar has deliberately
/// made its procfs executable link unreadable. Recovery still fails closed unless
/// it can independently verify the live process against this conservative record.
#[cfg(target_os = "linux")]
fn launch_bound_sidecar_record(
    pid: u32,
    process_group: u32,
    program: &Path,
    executable_sha256: &str,
    launch_argv: &[OsString],
    launch_binding: &str,
) -> Result<ProcessRecord, FirestoneError> {
    let executable = fs::canonicalize(program).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot resolve sidecar executable {}", program.display()),
            source,
        )
    })?;
    let metadata = fs::metadata(&executable).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot inspect sidecar executable {}", executable.display()),
            source,
        )
    })?;
    let start_time_ticks = process_start_time(pid)?.ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("sidecar process {pid} disappeared during identity capture"),
        )
    })?;
    let argv_hex = encode_os_argv(launch_argv);
    Ok(ProcessRecord {
        pid,
        process_group,
        executable,
        executable_dev: metadata.dev(),
        executable_ino: metadata.ino(),
        argv_hex: argv_hex.clone(),
        launch_artifact: Some(program.to_path_buf()),
        launch_argv_hex: Some(argv_hex),
        launch_binding: Some(launch_binding.to_owned()),
        launch_sha256: Some(executable_sha256.to_owned()),
        uid: nix::unistd::getuid().as_raw(),
        start_time_ticks: Some(start_time_ticks),
    })
}

fn capture_sidecar_process_record(
    sidecar: &mut OwnedSidecar,
    program: &Path,
    executable_sha256: &str,
    launch_argv: &[OsString],
    launch_binding: &str,
    work_deadline: Instant,
) -> Result<ProcessRecord, FirestoneError> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(250))
        .map_or(work_deadline, |deadline| deadline.min(work_deadline));
    loop {
        match process_record(
            sidecar.id(),
            sidecar.record.process_group,
            program.to_path_buf(),
            Some(executable_sha256.to_owned()),
            launch_argv.to_vec(),
            Some(launch_binding.to_owned()),
        ) {
            Ok(record) => return Ok(record),
            Err(error) => {
                if sidecar.observe_exit()? {
                    let context = format!(
                        "sidecar {} exited before process identity capture",
                        sidecar.name()
                    );
                    let status = sidecar.reap_exited()?;
                    return Err(sidecar.exit_error(status, &context));
                }
                #[cfg(target_os = "linux")]
                if process_executable_access_denied(sidecar.id()) {
                    return launch_bound_sidecar_record(
                        sidecar.id(),
                        sidecar.record.process_group,
                        program,
                        executable_sha256,
                        launch_argv,
                        launch_binding,
                    );
                }
                if Instant::now() >= deadline {
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_sidecar(
    paths: &Paths,
    machine: &str,
    plan: &LaunchPlan,
    sidecar_name: String,
    command: Cmd,
    program: &Path,
    artifacts: Vec<RuntimeArtifact>,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    work_deadline: Instant,
) -> Result<OwnedSidecar, FirestoneError> {
    ensure_launch_before_deadline(work_deadline, "validating sidecar executable")?;
    validate_executable(program, None)?;
    let executable_sha256 = hash_file(program, MAX_EXECUTABLE_BYTES, "sidecar executable")?;
    let launch_argv = sidecar_argv(&command);
    preflight_launch_argv(&launch_argv)?;
    let launch_binding = sidecar_launch_binding(plan, &sidecar_name, &executable_sha256);
    ensure_launch_before_deadline(work_deadline, "spawning sidecar")?;
    let process = command
        .env("FIRESTONE_LAUNCH_BINDING", launch_binding.clone())
        .spawn_process_group()?;
    let mut sidecar = OwnedSidecar::from_spawn(
        sidecar_name.clone(),
        process,
        program.to_path_buf(),
        artifacts,
    );
    state
        .sidecar_pids
        .insert(sidecar_name.clone(), sidecar.id());
    StateStore::new(paths.machine_state(machine)?).write_from_shim(state)?;
    let record = capture_sidecar_process_record(
        &mut sidecar,
        program,
        &executable_sha256,
        &launch_argv,
        &launch_binding,
        work_deadline,
    )?;
    sidecar.bind_record(record.clone());
    identity.sidecars.insert(sidecar_name, record);
    publish_process_identity(paths, machine, identity)?;
    Ok(sidecar)
}

fn remove_runtime_artifact(artifact: &RuntimeArtifact) -> Result<(), FirestoneError> {
    let metadata = match fs::symlink_metadata(&artifact.path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Generic,
                format!(
                    "cannot inspect sidecar artifact {}",
                    artifact.path.display()
                ),
                source,
            ));
        }
    };
    let type_matches = match artifact.kind {
        RuntimeArtifactKind::Socket => metadata.file_type().is_socket(),
        RuntimeArtifactKind::Regular => metadata.file_type().is_file(),
    };
    let mode = metadata.mode() & 0o7777;
    if !type_matches || metadata.uid() != artifact.uid || mode != artifact.mode {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "refusing to remove unverified sidecar artifact {}",
                artifact.path.display()
            ),
        )
        .with_hint("inspect the retained private runtime directory before retrying"));
    }
    fs::remove_file(&artifact.path).map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot remove sidecar artifact {}", artifact.path.display()),
            source,
        )
    })
}

fn cleanup_sidecar_artifacts(sidecar: &OwnedSidecar) -> Result<(), FirestoneError> {
    for artifact in sidecar.artifacts() {
        remove_runtime_artifact(artifact)?;
    }
    Ok(())
}

fn passt_artifacts(plan: &crate::PasstPlan) -> Vec<RuntimeArtifact> {
    vec![RuntimeArtifact {
        path: plan.socket().path().to_path_buf(),
        uid: plan.socket().uid(),
        mode: plan.socket().mode(),
        kind: RuntimeArtifactKind::Socket,
    }]
}

fn virtiofs_artifacts(plan: &VirtiofsPlan, uid: u32) -> Vec<RuntimeArtifact> {
    vec![
        RuntimeArtifact {
            path: plan.socket().to_path_buf(),
            uid,
            mode: 0o700,
            kind: RuntimeArtifactKind::Socket,
        },
        RuntimeArtifact {
            path: plan.pid_file().to_path_buf(),
            uid,
            mode: 0o600,
            kind: RuntimeArtifactKind::Regular,
        },
    ]
}

fn ensure_sidecar_alive(sidecar: &mut OwnedSidecar) -> Result<(), FirestoneError> {
    if !sidecar.observe_exit()? {
        return Ok(());
    }
    let context = format!("sidecar {} exited before VMM launch", sidecar.name());
    let status = sidecar.reap_exited()?;
    Err(sidecar.exit_error(status, &context))
}

fn passt_step_detail(plan: &crate::PasstPlan) -> String {
    if plan.forwards().is_empty() {
        "passt".to_owned()
    } else {
        format!(
            "passt · {}",
            plan.forwards()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_sidecars(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    terminating: &AtomicBool,
    events: &mut dyn EventSink,
    work_deadline: Instant,
    cleanup_limit: Instant,
) -> Result<Vec<OwnedSidecar>, FirestoneError> {
    let network = plan.network_plan()?;
    let filesystems = plan.filesystem_plans()?;
    let capacity =
        usize::from(matches!(network, NetworkPlan::Passt(_))).saturating_add(filesystems.len());
    let mut sidecars = Vec::with_capacity(capacity);

    let launch_result = (|| -> Result<(), FirestoneError> {
        match &network {
            NetworkPlan::None => events.emit(Event::StepSkip {
                id: StepId::from("net"),
                reason: "network.mode is none".to_owned(),
            })?,
            NetworkPlan::Tap(tap) => events.emit(Event::StepSkip {
                id: StepId::from("net"),
                reason: format!("tap {} uses no sidecar", tap.name()),
            })?,
            NetworkPlan::Passt(passt) => {
                events.emit(Event::StepStart {
                    id: StepId::from("net"),
                    label: "start passt".to_owned(),
                })?;
                let program = PathBuf::from(passt.command().program());
                let sidecar = spawn_sidecar(
                    paths,
                    name,
                    plan,
                    "passt".to_owned(),
                    passt.command().clone(),
                    &program,
                    passt_artifacts(passt),
                    state,
                    identity,
                    work_deadline,
                )?;
                sidecars.push(sidecar);
                let sidecar = sidecars.last_mut().ok_or_else(|| {
                    FirestoneError::new(ErrorKind::Generic, "passt owner disappeared")
                })?;
                passt.wait_ready_while(work_deadline, terminating, || {
                    ensure_sidecar_alive(sidecar)
                })?;
                events.emit(Event::StepDone {
                    id: StepId::from("net"),
                    detail: Some(passt_step_detail(passt)),
                    elapsed_ms: 0,
                })?;
            }
        }

        if filesystems.is_empty() {
            events.emit(Event::StepSkip {
                id: StepId::from("fs"),
                reason: "no mounts".to_owned(),
            })?;
        } else {
            for filesystem in &filesystems {
                events.emit(Event::StepStart {
                    id: StepId::from("fs"),
                    label: format!("start virtiofsd {}", filesystem.tag()),
                })?;
                let sidecar_name = format!("virtiofsd-{}", filesystem.index());
                let sidecar = spawn_sidecar(
                    paths,
                    name,
                    plan,
                    sidecar_name,
                    filesystem.command(),
                    filesystem.program(),
                    virtiofs_artifacts(filesystem, paths.uid()),
                    state,
                    identity,
                    work_deadline,
                )?;
                sidecars.push(sidecar);
                let sidecar = sidecars.last_mut().ok_or_else(|| {
                    FirestoneError::new(ErrorKind::Generic, "virtiofsd owner disappeared")
                })?;
                filesystem.wait_ready_while(work_deadline, terminating, || {
                    ensure_sidecar_alive(sidecar)
                })?;
                let readonly = if filesystem.readonly() {
                    " · read-only"
                } else {
                    ""
                };
                events.emit(Event::StepDone {
                    id: StepId::from("fs"),
                    detail: Some(format!(
                        "{} -> {}{readonly}",
                        filesystem.host().display(),
                        filesystem.guest().display()
                    )),
                    elapsed_ms: 0,
                })?;
            }
        }
        Ok(())
    })();

    if let Err(error) = launch_result {
        let cleanup_deadline = Instant::now()
            .checked_add(CHILD_TERM_GRACE.saturating_mul(2))
            .map_or(cleanup_limit, |deadline| deadline.min(cleanup_limit));
        if let Err(cleanup_error) = stop_owned_sidecars(
            paths,
            name,
            state,
            identity,
            &mut sidecars,
            cleanup_deadline,
        ) {
            write_shim_log(&format!(
                "sidecar launch rollback failed ({}); process identity retained",
                cleanup_error.kind()
            ));
        }
        return Err(error);
    }
    Ok(sidecars)
}

fn stop_owned_sidecars(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    sidecars: &mut Vec<OwnedSidecar>,
    deadline: Instant,
) -> Result<(), FirestoneError> {
    for sidecar in sidecars.iter() {
        if !sidecar.observe_exit()? {
            sidecar
                .process
                .as_ref()
                .ok_or_else(|| {
                    FirestoneError::new(ErrorKind::NotRunning, "sidecar child disappeared")
                })?
                .signal_group(ProcessSignal::Terminate)?;
        }
    }
    let term_deadline = Instant::now()
        .checked_add(CHILD_TERM_GRACE)
        .map_or(deadline, |candidate| candidate.min(deadline));
    while Instant::now() < term_deadline {
        let mut alive = false;
        for sidecar in sidecars.iter() {
            alive |= !sidecar.observe_exit()?;
        }
        if !alive {
            break;
        }
        thread::sleep(LOOP_INTERVAL.min(term_deadline.saturating_duration_since(Instant::now())));
    }

    for sidecar in sidecars.iter() {
        if !sidecar.observe_exit()? {
            sidecar
                .process
                .as_ref()
                .ok_or_else(|| {
                    FirestoneError::new(ErrorKind::NotRunning, "sidecar child disappeared")
                })?
                .signal_group(ProcessSignal::Kill)?;
        }
    }
    let kill_deadline = Instant::now()
        .checked_add(CHILD_TERM_GRACE)
        .map_or(deadline, |candidate| candidate.min(deadline));
    while Instant::now() < kill_deadline {
        let mut alive = false;
        for sidecar in sidecars.iter() {
            alive |= !sidecar.observe_exit()?;
        }
        if !alive {
            break;
        }
        thread::sleep(LOOP_INTERVAL.min(kill_deadline.saturating_duration_since(Instant::now())));
    }
    if let Some(sidecar) = sidecars
        .iter()
        .find(|sidecar| sidecar.observe_exit().is_ok_and(|exited| !exited))
    {
        return Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!(
                "sidecar {} pid {} did not exit after SIGKILL",
                sidecar.name(),
                sidecar.id()
            ),
        ));
    }

    let mut artifact_error = None;
    while let Some(mut sidecar) = sidecars.pop() {
        sidecar.reap_exited()?;
        if let Err(error) = cleanup_sidecar_artifacts(&sidecar) {
            if artifact_error.is_none() {
                artifact_error = Some(error);
            }
        }
        identity.sidecars.remove(sidecar.name());
        state.sidecar_pids.remove(sidecar.name());
    }
    publish_process_identity(paths, name, identity)?;
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    match artifact_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn ensure_owned_sidecars_alive(sidecars: &mut [OwnedSidecar]) -> Result<(), FirestoneError> {
    for sidecar in sidecars {
        ensure_sidecar_alive(sidecar)?;
    }
    Ok(())
}

fn sidecar_exit_marker(name: &str, status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("{name} exited (code {code})")
    } else if let Some(signal) = status.signal() {
        format!("{name} exited (signal {signal})")
    } else {
        format!("{name} exited (unknown status)")
    }
}

fn reconcile_owned_sidecar_exits(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    sidecars: &mut Vec<OwnedSidecar>,
) -> Result<(), FirestoneError> {
    let mut index = 0;
    while index < sidecars.len() {
        if !sidecars[index].observe_exit()? {
            index += 1;
            continue;
        }
        let mut sidecar = sidecars.remove(index);
        let status = sidecar.reap_exited()?;
        if let Err(error) = cleanup_sidecar_artifacts(&sidecar) {
            write_shim_log(&format!(
                "machine `{name}` could not clean {} artifacts ({}); runtime entry retained",
                sidecar.name(),
                error.kind()
            ));
        }
        identity.sidecars.remove(sidecar.name());
        state.sidecar_pids.remove(sidecar.name());
        let marker = sidecar_exit_marker(sidecar.name(), status);
        if !state.degraded.iter().any(|entry| entry == &marker) {
            state.degraded.push(marker.clone());
        }
        write_shim_log(&format!("machine `{name}` {marker}"));
        publish_process_identity(paths, name, identity)?;
        StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    }
    Ok(())
}

fn cleanup_recovered_sidecar_artifacts(sidecar: &RecoveredProcess) -> Result<(), FirestoneError> {
    for artifact in sidecar.artifacts() {
        remove_runtime_artifact(artifact)?;
    }
    Ok(())
}

fn reconcile_recovered_sidecar_exits(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    sidecars: &mut Vec<RecoveredProcess>,
) -> Result<(), FirestoneError> {
    let mut index = 0;
    while index < sidecars.len() {
        if sidecars[index].is_alive()? {
            index += 1;
            continue;
        }
        let sidecar = sidecars.remove(index);
        if let Err(error) = cleanup_recovered_sidecar_artifacts(&sidecar) {
            write_shim_log(&format!(
                "machine `{name}` could not clean recovered {} artifacts ({}); runtime entry retained",
                sidecar.label(),
                error.kind()
            ));
        }
        identity.sidecars.remove(sidecar.label());
        state.sidecar_pids.remove(sidecar.label());
        let marker = format!("{} exited (status unavailable)", sidecar.label());
        if !state.degraded.iter().any(|entry| entry == &marker) {
            state.degraded.push(marker.clone());
        }
        write_shim_log(&format!("machine `{name}` {marker}"));
        publish_process_identity(paths, name, identity)?;
        StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    }
    Ok(())
}

fn stop_recovered_sidecars(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    sidecars: &mut Vec<RecoveredProcess>,
    deadline: Instant,
) -> Result<(), FirestoneError> {
    for sidecar in sidecars.iter() {
        if sidecar.is_alive()? {
            signal_verified_tree(sidecar.record(), ProcessSignal::Terminate)?;
        }
    }
    let term_deadline = Instant::now()
        .checked_add(CHILD_TERM_GRACE)
        .map_or(deadline, |candidate| candidate.min(deadline));
    while Instant::now() < term_deadline {
        let mut alive = false;
        for sidecar in sidecars.iter() {
            alive |= sidecar.is_alive()?;
        }
        if !alive {
            break;
        }
        thread::sleep(LOOP_INTERVAL.min(term_deadline.saturating_duration_since(Instant::now())));
    }
    for sidecar in sidecars.iter() {
        if sidecar.is_alive()? {
            signal_verified_tree(sidecar.record(), ProcessSignal::Kill)?;
        }
    }
    let kill_deadline = Instant::now()
        .checked_add(CHILD_TERM_GRACE)
        .map_or(deadline, |candidate| candidate.min(deadline));
    while Instant::now() < kill_deadline {
        let mut alive = false;
        for sidecar in sidecars.iter() {
            alive |= sidecar.is_alive()?;
        }
        if !alive {
            break;
        }
        thread::sleep(LOOP_INTERVAL.min(kill_deadline.saturating_duration_since(Instant::now())));
    }
    for sidecar in sidecars.iter() {
        if sidecar.is_alive()? {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "recovered sidecar {} pid {} did not exit after SIGKILL",
                    sidecar.label(),
                    sidecar.record().pid
                ),
            ));
        }
    }
    let mut artifact_error = None;
    while let Some(sidecar) = sidecars.pop() {
        if let Err(error) = cleanup_recovered_sidecar_artifacts(&sidecar) {
            if artifact_error.is_none() {
                artifact_error = Some(error);
            }
        }
        identity.sidecars.remove(sidecar.label());
        state.sidecar_pids.remove(sidecar.label());
    }
    publish_process_identity(paths, name, identity)?;
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    match artifact_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
#[allow(clippy::too_many_arguments)]
fn launch_vmm(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    terminating: &AtomicBool,
    events: &mut dyn EventSink,
    launch_deadline: Instant,
) -> Result<(OwnedVmm, Vec<OwnedSidecar>), FirestoneError> {
    let work_deadline = launch_deadline
        .checked_sub(CHILD_TERM_GRACE.saturating_mul(2))
        .unwrap_or_else(Instant::now);
    let mut sidecars = launch_sidecars(
        paths,
        name,
        plan,
        state,
        identity,
        terminating,
        events,
        work_deadline,
        launch_deadline,
    )?;
    ensure_owned_sidecars_alive(&mut sidecars)?;
    write_shim_log(&format!("machine `{name}` launching cloud-hypervisor"));
    events.emit(Event::StepStart {
        id: StepId::from("vmm"),
        label: "start cloud-hypervisor".to_owned(),
    })?;
    ensure_launch_before_deadline(work_deadline, "validating VMM executable")?;
    validate_plan_executable(plan)?;
    let vmconfig = read_exact_vmconfig(paths, name, plan)?;
    rotate_console_log(paths, name)?;
    ensure_launch_before_deadline(work_deadline, "spawning cloud-hypervisor")?;
    let vmm_log = paths.machine_vmm_log(name)?;

    let api_socket = paths.machine_api_socket(name)?;
    let launch_argv = vmm_argv(paths, name, plan)?;
    preflight_launch_argv(&launch_argv)?;
    let launch_binding = vmm_launch_binding(plan);
    let command = vmm_environment(
        Cmd::new(launch_argv[0].clone())
            .args(launch_argv.iter().skip(1).cloned())
            .env("FIRESTONE_LAUNCH_BINDING", launch_binding.clone())
            .cwd("/")
            .stdin_null()
            .stdout_append(&vmm_log)
            .stderr_append(&vmm_log)
            .error_kind(ErrorKind::Dependency),
    );
    let process = command.spawn_process_group()?;
    let mut vmm = OwnedVmm::from_spawn(
        process,
        plan.vmm_binary.clone(),
        preserved_process_roots(identity.sidecars.values()),
    );
    state.vmm_pid = Some(vmm.id());
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    let record = capture_launch_process_record(
        &mut vmm,
        plan,
        &launch_argv,
        &launch_binding,
        work_deadline,
    )?;
    vmm.bind_record(record.clone());
    identity.vmm = Some(record);
    publish_process_identity(paths, name, identity)?;

    let timeouts = plan.timeouts()?;
    let phase_deadline = Instant::now()
        .checked_add(timeouts.readiness)
        .map_or(work_deadline, |deadline| deadline.min(work_deadline));
    let ping = loop {
        if let Err(error) = ensure_owned_sidecars_alive(&mut sidecars) {
            terminate_launch_vmm(&mut vmm, launch_deadline)?;
            return Err(error);
        }
        if terminating.load(Ordering::Relaxed) {
            terminate_launch_vmm(&mut vmm, launch_deadline)?;
            return Err(FirestoneError::new(
                ErrorKind::Interrupted,
                format!("machine `{name}` launch interrupted by signal"),
            ));
        }
        if vmm.observe_exit()? {
            let status = vmm.reap_exited_group()?;
            return Err(vmm.exit_error(
                status,
                &format!("cloud-hypervisor for machine `{name}` exited before API readiness"),
            ));
        }
        if Instant::now() >= phase_deadline {
            terminate_launch_vmm(&mut vmm, launch_deadline)?;
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "cloud-hypervisor API for machine `{name}` was not ready within the launch deadline"
                ),
            )
            .with_hint(format!("inspect {}", vmm_log.display())));
        }
        let remaining = phase_deadline.saturating_duration_since(Instant::now());
        let api_timeout = timeouts.api.min(remaining);
        match VmmApi::new(&api_socket, api_timeout).vmm_ping() {
            Ok(ping) => break ping,
            Err(error) if matches!(error.kind(), ErrorKind::NotRunning | ErrorKind::Timeout) => {
                thread::sleep(LOOP_INTERVAL.min(remaining));
            }
            Err(error) => {
                terminate_launch_vmm(&mut vmm, launch_deadline)?;
                return Err(error);
            }
        }
    };
    if ping.pid != i64::from(vmm.id()) {
        terminate_launch_vmm(&mut vmm, launch_deadline)?;
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "VMM API pid {} does not match spawned cloud-hypervisor pid {}",
                ping.pid,
                vmm.id()
            ),
        ));
    }

    let record = process_record(
        vmm.id(),
        vmm.record.process_group,
        plan.vmm_binary.clone(),
        Some(plan.vmm_binary_sha256.clone()),
        launch_argv,
        Some(launch_binding),
    )?;
    vmm.bind_record(record.clone());
    identity.vmm = Some(record);
    publish_process_identity(paths, name, identity)?;

    if let Err(error) = ensure_owned_sidecars_alive(&mut sidecars) {
        terminate_launch_vmm(&mut vmm, launch_deadline)?;
        return Err(error);
    }
    let create_timeout = launch_phase_timeout(work_deadline, timeouts.api, "vm.create")?;
    if let Err(error) = VmmApi::new(&api_socket, create_timeout).vm_create(&vmconfig) {
        terminate_launch_vmm(&mut vmm, launch_deadline)?;
        return Err(error);
    }
    secure_console_log(paths, name)?;
    if let Err(error) = ensure_owned_sidecars_alive(&mut sidecars) {
        terminate_launch_vmm(&mut vmm, launch_deadline)?;
        return Err(error);
    }
    let boot_timeout = launch_phase_timeout(work_deadline, timeouts.api, "vm.boot")?;
    if let Err(error) = VmmApi::new(&api_socket, boot_timeout).vm_boot() {
        if let Ok(shutdown_timeout) =
            launch_phase_timeout(work_deadline, timeouts.api, "failed vm.boot cleanup")
        {
            let _ = VmmApi::new(&api_socket, shutdown_timeout).vmm_shutdown();
        }
        terminate_launch_vmm(&mut vmm, launch_deadline)?;
        return Err(error);
    }
    let console_result = (|| {
        let info_timeout = launch_phase_timeout(work_deadline, timeouts.api, "vm.info console")?;
        let info = VmmApi::new(&api_socket, info_timeout).vm_info()?;
        let pty_path = console_pty_path(name, &info.config)?;
        ConsoleBroker::start(paths, name, &pty_path)
    })();
    match console_result {
        Ok(console) => vmm.attach_console(console),
        Err(error) => {
            terminate_launch_vmm(&mut vmm, launch_deadline)?;
            return Err(error);
        }
    }
    ensure_launch_before_deadline(work_deadline, "publishing running state")?;
    state.status = MachineStatus::Running;
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    write_shim_log(&format!("machine `{name}` is running"));
    events.emit(Event::StepDone {
        id: StepId::from("vmm"),
        detail: Some(format!("cloud-hypervisor {}", ping.build_version)),
        elapsed_ms: 0,
    })?;
    if let Err(error) = ensure_owned_sidecars_alive(&mut sidecars) {
        terminate_launch_vmm(&mut vmm, launch_deadline)?;
        return Err(error);
    }
    OWNER_EVIDENCE_STATE.store(0, Ordering::Release);
    Ok((vmm, sidecars))
}

fn console_pty_path(name: &str, config: &Value) -> Result<PathBuf, FirestoneError> {
    let mode = config
        .pointer("/console/mode")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("cloud-hypervisor did not report a console mode for machine {name}"),
            )
            .with_hint("verify the pinned Cloud Hypervisor v53 console contract")
        })?;
    if mode != "Pty" {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "cloud-hypervisor reported console mode {mode:?} for machine {name}, expected Pty"
            ),
        )
        .with_hint("remove VMM console overrides and restart the machine"));
    }
    let path = config
        .pointer("/console/file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("cloud-hypervisor did not publish the console PTY path for machine {name}"),
            )
            .with_hint("verify the pinned Cloud Hypervisor v53 vm.info response")
        })?;
    Ok(PathBuf::from(path))
}

#[allow(clippy::too_many_arguments)]
fn stop_recovered_vmm(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    record: &ProcessRecord,
    recovered_sidecars: &mut Vec<RecoveredProcess>,
    guest_timeout: Duration,
    force: bool,
    overall_deadline: Instant,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    let preserved_sidecars =
        preserved_process_roots(recovered_sidecars.iter().map(RecoveredProcess::record));
    verify_recovered_process(record)?;
    state.status = MachineStatus::Stopping;
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)?;
    events.emit(Event::StepStart {
        id: StepId::from("stop"),
        label: if force {
            "force stop recovered VMM".to_owned()
        } else {
            "ACPI power button for recovered VMM".to_owned()
        },
    })?;
    let api_socket = paths.machine_api_socket(name)?;
    let api_cap = plan.timeouts()?.api.min(STOP_API_PHASE_CAP);
    let mut reason = ExitReason::GuestShutdown;
    if force {
        signal_verified_tree_preserving(record, ProcessSignal::Kill, &preserved_sidecars)?;
        reason = ExitReason::Failure("forced stop".to_owned());
    } else {
        let power_timeout = stop_phase_timeout(overall_deadline, api_cap)?;
        let power_result = VmmApi::new(&api_socket, power_timeout).vm_power_button();
        let guest_deadline = Instant::now()
            .checked_add(guest_timeout)
            .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
        if power_result.is_ok() {
            while Instant::now() < guest_deadline && recorded_process_alive(record)? {
                let phase_timeout = stop_phase_timeout(overall_deadline, api_cap)?;
                match VmmApi::new(&api_socket, phase_timeout).vm_info() {
                    Ok(info) if info.state == VmState::Shutdown => {
                        let shutdown_timeout = stop_phase_timeout(overall_deadline, api_cap)?;
                        let _ = VmmApi::new(&api_socket, shutdown_timeout).vmm_shutdown();
                    }
                    Ok(_) | Err(_) => {}
                }
                thread::sleep(
                    LOOP_INTERVAL.min(overall_deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        if recorded_process_alive(record)? {
            signal_verified_tree_preserving(record, ProcessSignal::Terminate, &preserved_sidecars)?;
            let term_deadline = Instant::now()
                .checked_add(CHILD_TERM_GRACE)
                .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
            while Instant::now() < term_deadline && recorded_process_alive(record)? {
                thread::sleep(
                    LOOP_INTERVAL.min(term_deadline.saturating_duration_since(Instant::now())),
                );
            }
            reason = ExitReason::Failure(if power_result.is_err() {
                "VMM API failed during graceful stop".to_owned()
            } else {
                "graceful stop timed out".to_owned()
            });
        }
        if recorded_process_alive(record)? {
            signal_verified_tree_preserving(record, ProcessSignal::Kill, &preserved_sidecars)?;
        }
    }
    let remaining = overall_deadline.saturating_duration_since(Instant::now());
    wait_for_record_exit(record, remaining)?;
    identity.vmm = None;
    state.vmm_pid = None;
    stop_recovered_sidecars(
        paths,
        name,
        state,
        identity,
        recovered_sidecars,
        overall_deadline,
    )?;
    write_final_state(paths, name, state, MachineStatus::Stopped, None, reason)?;
    events.emit(Event::StepDone {
        id: StepId::from("stop"),
        detail: Some("recovered VMM stopped".to_owned()),
        elapsed_ms: 0,
    })?;
    Ok(())
}

fn verify_recovered_process(record: &ProcessRecord) -> Result<(), FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        verify_linux_process(record)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = record;
        Err(FirestoneError::new(
            ErrorKind::Dependency,
            "recovered process verification requires Linux",
        ))
    }
}

fn signal_verified_tree(
    record: &ProcessRecord,
    signal: ProcessSignal,
) -> Result<(), FirestoneError> {
    signal_verified_tree_preserving(record, signal, &PreservedProcessRoots::new())
}

#[cfg(target_os = "linux")]
fn signal_verified_tree_preserving(
    record: &ProcessRecord,
    signal: ProcessSignal,
    preserved_roots: &PreservedProcessRoots,
) -> Result<(), FirestoneError> {
    verify_linux_process(record)?;
    let descendants = snapshot_owned_descendants_preserving(record.pid, preserved_roots)?;
    let leader_pid = rustix::process::Pid::from_raw(record.pid as _)
        .ok_or_else(|| FirestoneError::new(ErrorKind::Conflict, "recorded VMM pid is invalid"))?;
    let leader = rustix::process::pidfd_open(leader_pid, rustix::process::PidfdFlags::empty())
        .map_err(|source| {
            FirestoneError::new(ErrorKind::Conflict, "cannot pin recovered VMM pid")
                .with_source(io::Error::from_raw_os_error(source.raw_os_error()))
        })?;
    verify_linux_process(record)?;
    let rustix_signal = match signal {
        ProcessSignal::Hangup => rustix::process::Signal::HUP,
        ProcessSignal::Interrupt => rustix::process::Signal::INT,
        ProcessSignal::Quit => rustix::process::Signal::QUIT,
        ProcessSignal::Terminate => rustix::process::Signal::TERM,
        ProcessSignal::Kill => rustix::process::Signal::KILL,
        // `SIGWINCH` belongs to terminal transports, not to the VMM
        // supervision tree; the mapping keeps the translation total.
        ProcessSignal::WindowChange => rustix::process::Signal::WINCH,
    };
    match rustix::process::pidfd_send_signal(&leader, rustix_signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(source) => {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot signal recovered VMM pid {}", record.pid),
            )
            .with_source(io::Error::from_raw_os_error(source.raw_os_error())));
        }
    }

    signal_descendants(&descendants, signal)?;
    if signal == ProcessSignal::Kill {
        let deadline = Instant::now() + CHILD_TERM_GRACE;
        while Instant::now() < deadline {
            let mut alive = false;
            for descendant in descendants.values() {
                let path = PathBuf::from("/proc").join(descendant.pid.to_string());
                match fs::metadata(&path) {
                    Ok(metadata)
                        if metadata.uid() == descendant.uid
                            && process_start_time(descendant.pid)?
                                == Some(descendant.start_time_ticks)
                            && process_state(descendant.pid)?.is_some_and(|state| state != 'Z') =>
                    {
                        alive = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(filesystem_error(
                            ErrorKind::Conflict,
                            format!("cannot verify descendant pid {} exit", descendant.pid),
                            source,
                        ));
                    }
                }
            }
            if !alive {
                return Ok(());
            }
            thread::sleep(LOOP_INTERVAL);
        }
        return Err(FirestoneError::new(
            ErrorKind::Timeout,
            "recovered VMM descendants did not exit after SIGKILL",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn signal_verified_tree_preserving(
    _record: &ProcessRecord,
    _signal: ProcessSignal,
    _preserved_roots: &PreservedProcessRoots,
) -> Result<(), FirestoneError> {
    Err(FirestoneError::new(
        ErrorKind::Dependency,
        "recovered process signalling requires Linux",
    ))
}

fn terminate_recovered_process(record: &ProcessRecord) -> Result<(), FirestoneError> {
    if !recorded_process_alive(record)? {
        return Ok(());
    }
    signal_verified_tree(record, ProcessSignal::Terminate)?;
    let term_deadline = Instant::now()
        .checked_add(CHILD_TERM_GRACE)
        .ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "recovery TERM deadline is invalid")
        })?;
    while Instant::now() < term_deadline && recorded_process_alive(record)? {
        thread::sleep(LOOP_INTERVAL);
    }
    if recorded_process_alive(record)? {
        signal_verified_tree(record, ProcessSignal::Kill)?;
    }
    wait_for_record_exit(record, CHILD_TERM_GRACE)
}

#[allow(clippy::too_many_arguments)]
fn stop_owned_vmm(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &mut MachineState,
    identity: &mut ProcessIdentity,
    vmm: &mut OwnedVmm,
    sidecars: &mut Vec<OwnedSidecar>,
    timeout: Duration,
    force: bool,
    overall_deadline: Instant,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    state.status = MachineStatus::Stopping;
    events.emit(Event::StepStart {
        id: StepId::from("stop"),
        label: if force {
            "force stop VMM".to_owned()
        } else {
            "ACPI power button".to_owned()
        },
    })?;
    write_shim_log(&format!("machine `{name}` stopping; force={force}"));
    let recorded = identity.vmm.as_ref().ok_or_else(|| {
        FirestoneError::new(ErrorKind::Conflict, "running VMM has no process identity")
    })?;
    if recorded != vmm.record() {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            "running VMM identity does not match its owned child",
        ));
    }

    let api_socket = paths.machine_api_socket(name)?;
    let api_cap = plan.timeouts()?.api.min(STOP_API_PHASE_CAP);

    let (status, reason) = if force {
        (
            vmm.terminate_and_reap_before(false, Duration::ZERO, overall_deadline)?,
            ExitReason::Failure("forced stop".to_owned()),
        )
    } else {
        let power_timeout = stop_phase_timeout(overall_deadline, api_cap)?;
        let power_result = VmmApi::new(&api_socket, power_timeout).vm_power_button();
        let guest_deadline = Instant::now()
            .checked_add(timeout)
            .map_or(overall_deadline, |deadline| deadline.min(overall_deadline));
        let mut vm_shutdown_observed = false;
        if power_result.is_ok() {
            while Instant::now() < guest_deadline && !vmm.observe_exit()? {
                let Ok(phase_timeout) = stop_phase_timeout(overall_deadline, api_cap) else {
                    break;
                };
                match VmmApi::new(&api_socket, phase_timeout).vm_info() {
                    Ok(info) if info.state == VmState::Shutdown => {
                        vm_shutdown_observed = true;
                        if let Ok(shutdown_timeout) = stop_phase_timeout(overall_deadline, api_cap)
                        {
                            let _ = VmmApi::new(&api_socket, shutdown_timeout).vmm_shutdown();
                        }
                    }
                    Ok(_) | Err(_) => {}
                }
                thread::sleep(
                    LOOP_INTERVAL.min(guest_deadline.saturating_duration_since(Instant::now())),
                );
            }
        }

        if vmm.observe_exit()? {
            (vmm.reap_exited_group()?, ExitReason::GuestShutdown)
        } else {
            events.emit(Event::StepUpdate {
                id: StepId::from("stop"),
                detail: "graceful stop did not complete; sending SIGTERM then SIGKILL if needed"
                    .to_owned(),
            })?;
            let term_grace =
                CHILD_TERM_GRACE.min(overall_deadline.saturating_duration_since(Instant::now()));
            let status = vmm.terminate_and_reap_before(true, term_grace, overall_deadline)?;
            let reason = if power_result.is_err() {
                ExitReason::Failure("VMM API failed during graceful stop".to_owned())
            } else if vm_shutdown_observed {
                ExitReason::Failure("VMM shutdown API did not terminate the process".to_owned())
            } else {
                ExitReason::Failure("graceful stop timed out".to_owned())
            };
            (status, reason)
        }
    };

    identity.vmm = None;
    stop_owned_sidecars(paths, name, state, identity, sidecars, overall_deadline)?;
    write_final_state(
        paths,
        name,
        state,
        MachineStatus::Stopped,
        Some(status),
        reason,
    )?;
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

fn set_vmm_api_degraded(state: &mut MachineState, unresponsive: bool) {
    state
        .degraded
        .retain(|marker| marker != VMM_API_UNRESPONSIVE);
    if unresponsive {
        state.degraded.push(VMM_API_UNRESPONSIVE.to_owned());
    }
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

fn write_recoverable_failed_state(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    record: &ProcessRecord,
    error: &FirestoneError,
) -> Result<(), FirestoneError> {
    state.status = MachineStatus::Failed;
    state.shim_pid = None;
    state.vmm_pid = Some(record.pid);
    state.degraded.clear();
    state.last_exit = Some(LastExit {
        at: now_timestamp(),
        code: None,
        signal: None,
        reason: ExitReason::Failure(format!(
            "{}; VMM recovery evidence retained",
            error.message()
        )),
    });
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

fn write_ambiguous_failed_state(
    paths: &Paths,
    name: &str,
    state: &mut MachineState,
    error: &FirestoneError,
) -> Result<(), FirestoneError> {
    state.status = MachineStatus::Failed;
    state.shim_pid = None;
    state.last_exit = Some(LastExit {
        at: now_timestamp(),
        code: None,
        signal: None,
        reason: ExitReason::Failure(format!(
            "{}; process absence was not proven and runtime evidence was retained",
            error.message()
        )),
    });
    StateStore::new(paths.machine_state(name)?).write_from_shim(state)
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
        Err(error) if error.kind() == ErrorKind::NotRunning => {}
        Err(error) => {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("cannot prove stale shim socket {}", shim_socket.display()),
            )
            .with_hint("inspect the existing runtime directory before retrying")
            .with_source(error));
        }
    }
    ensure_recorded_vmm_absent(paths, name)?;
    let api_socket = paths.machine_api_socket(name)?;
    match VmmApiLivenessProbe::new(timeout).ping(&api_socket) {
        Ok(true) => Err(FirestoneError::new(
            ErrorKind::AlreadyRunning,
            format!("machine `{name}` already has a live VMM"),
        )),
        Ok(false) => Ok(()),
        Err(error) => Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("cannot prove stale VMM socket {}", api_socket.display()),
        )
        .with_hint("inspect the existing runtime directory before retrying")
        .with_source(error)),
    }
}

fn ensure_recorded_vmm_absent(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    let identity_path = paths.machine_process_identity(name)?;
    let identity = if identity_path.try_exists().map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!(
                "cannot inspect process identity {}",
                identity_path.display()
            ),
            source,
        )
    })? {
        Some(read_process_identity(paths, name).map_err(|error| {
            FirestoneError::new(
                ErrorKind::Conflict,
                format!("cannot prove stale process identity for machine `{name}`"),
            )
            .with_hint("preserve the runtime directory until identity is repaired")
            .with_source(error)
        })?)
    } else {
        None
    };

    #[cfg(target_os = "linux")]
    let mut verified_dead_vmm = None;
    #[cfg(not(target_os = "linux"))]
    let verified_dead_vmm = None;
    if let Some(identity) = identity.as_ref() {
        if let Some(record) = identity.vmm.as_ref() {
            #[cfg(target_os = "linux")]
            {
                if recorded_process_alive(record)? {
                    verify_linux_process(record)?;
                    return Err(FirestoneError::new(
                        ErrorKind::AlreadyRunning,
                        format!("machine `{name}` has a verified live or hung VMM"),
                    )
                    .with_hint("recover or stop the recorded VMM; do not start a duplicate"));
                }
                verified_dead_vmm = Some(record.pid);
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = record;
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("machine `{name}` retains VMM identity on this platform"),
                )
                .with_hint("do not clear recovery evidence without a supported identity backend"));
            }
        }
        #[cfg(target_os = "linux")]
        for (sidecar_name, record) in &identity.sidecars {
            if recorded_process_alive(record)? {
                verify_linux_process(record)?;
                terminate_recovered_process(record).map_err(|error| {
                    FirestoneError::new(
                        error.kind(),
                        format!(
                            "cannot terminate stale verified sidecar `{sidecar_name}`: {}",
                            error.message()
                        ),
                    )
                })?;
            }
        }
        #[cfg(not(target_os = "linux"))]
        if !identity.sidecars.is_empty() {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("machine `{name}` retains sidecar identity on this platform"),
            ));
        }
    }

    let state = StateStore::new(paths.machine_state(name)?).read()?;
    if state.vmm_pid.is_some() && state.vmm_pid != verified_dead_vmm {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` retains a VMM pid without matching absence evidence"),
        )
        .with_hint("recover the process identity before starting another VMM"));
    }
    for (sidecar_name, pid) in &state.sidecar_pids {
        let record = identity
            .as_ref()
            .and_then(|identity| identity.sidecars.get(sidecar_name))
            .filter(|record| record.pid == *pid)
            .ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Conflict,
                    format!(
                        "machine `{name}` retains sidecar pid {pid} without matching absence evidence"
                    ),
                )
                .with_hint("preserve the runtime directory and recover the process identity")
            })?;
        if recorded_process_alive(record)? {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("verified sidecar `{sidecar_name}` pid {pid} remained alive"),
            ));
        }
    }
    Ok(())
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

fn find_program_on_path(program: &str) -> Result<Option<PathBuf>, FirestoneError> {
    let Some(search_path) = env::var_os("PATH") else {
        return Ok(None);
    };
    for directory in env::split_paths(&search_path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(program);
        let metadata = match fs::metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Dependency,
                    format!("cannot inspect {program} candidate {}", candidate.display()),
                    source,
                ));
            }
        };
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            continue;
        }
        let canonical = fs::canonicalize(&candidate).map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!(
                    "cannot resolve {program} executable {}",
                    candidate.display()
                ),
                source,
            )
        })?;
        validate_executable(&canonical, None)?;
        return Ok(Some(canonical));
    }
    Ok(None)
}

fn find_required_program(program: &str, hint: &str) -> Result<PathBuf, FirestoneError> {
    find_program_on_path(program)?.ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Dependency,
            format!("required program `{program}` is not installed on PATH"),
        )
        .with_hint(hint)
    })
}

#[cfg(target_os = "linux")]
fn detect_virtiofs_sandbox() -> VirtiofsSandbox {
    let namespaces_enabled = fs::read_to_string("/proc/sys/user/max_user_namespaces")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|maximum| maximum > 0);
    if !namespaces_enabled {
        return VirtiofsSandbox::None;
    }
    let Some(unshare) = find_program_on_path("unshare").ok().flatten() else {
        return VirtiofsSandbox::None;
    };
    match Cmd::new(unshare.as_os_str())
        .args(["--user", "--map-root-user", "true"])
        .reduced_environment()
        .stdin_null()
        .timeout(Duration::from_secs(2))
        .error_kind(ErrorKind::Dependency)
        .output()
    {
        Ok(output) if output.success() => VirtiofsSandbox::Namespace,
        Ok(_) | Err(_) => VirtiofsSandbox::None,
    }
}

#[cfg(not(target_os = "linux"))]
fn detect_virtiofs_sandbox() -> VirtiofsSandbox {
    VirtiofsSandbox::None
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
    name: &str,
    spec: &MachineSpec,
) -> Result<(PathBuf, String), FirestoneError> {
    if let Some(binary) = &spec.vmm.binary {
        return import_custom_vmm(paths, name, binary);
    }

    let artifact = manifest.artifact("cloud-hypervisor", architecture.as_str())?;
    let binary = materialize_embedded_helper(paths, InternalHelper::CloudHypervisor)?
        .unwrap_or(paths.binary_file(&artifact.install_name)?);
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

fn import_custom_vmm(
    paths: &Paths,
    name: &str,
    source_path: &Path,
) -> Result<(PathBuf, String), FirestoneError> {
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(source_path)
        .map_err(|error| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot open custom VMM {}", source_path.display()),
                error,
            )
        })?;
    let before = source.metadata().map_err(|error| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect custom VMM {}", source_path.display()),
            error,
        )
    })?;
    let mode = before.mode() & 0o7777;
    if !before.is_file()
        || (before.uid() != 0 && before.uid() != paths.uid())
        || mode & 0o111 == 0
        || mode & 0o022 != 0
        || before.len() > MAX_EXECUTABLE_BYTES
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "custom VMM {} must be a bounded, non-writable executable owned by the user or root",
                source_path.display()
            ),
        )
        .with_hint(
            "use an owner/root-owned regular executable without group/world write access",
        ));
    }

    paths.validate_machine_data_directory(name)?;
    let target = paths.machine_vmm_executable(name)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    atomic::write_stream_with_mode(&target, 0o700, |output| {
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let remaining = MAX_EXECUTABLE_BYTES.saturating_add(1).saturating_sub(total);
            if remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "custom VMM exceeds executable size limit",
                ));
            }
            let read_limit = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| io::Error::other("custom VMM read limit does not fit usize"))?;
            let read = source.read(&mut buffer[..read_limit])?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > MAX_EXECUTABLE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "custom VMM exceeds executable size limit",
                ));
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        let after = source.metadata()?;
        if !same_file_snapshot(&before, &after) || total != after.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "custom VMM changed while it was imported",
            ));
        }
        Ok(())
    })?;
    paths.validate_owned_data_file(&target, "imported custom VMM", 0o700, false)?;
    let digest = format!("{:x}", hasher.finalize());
    let published = hash_file(&target, MAX_EXECUTABLE_BYTES, "imported custom VMM")?;
    if published != digest {
        return Err(FirestoneError::new(
            ErrorKind::Checksum,
            "imported custom VMM changed after atomic publication",
        ));
    }
    Ok((target, digest))
}

fn validate_plan_executable(plan: &LaunchPlan) -> Result<(), FirestoneError> {
    validate_executable(&plan.vmm_binary, Some(nix::unistd::getuid().as_raw()))?;
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
    if metadata.mode() & 0o022 != 0 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("executable {} is group/world writable", path.display()),
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
    Cmd::new(program.as_os_str())
        .arg("_shim")
        .arg(name)
        .cwd("/")
        .stdin_null()
        .stdout_append(log)
        .stderr_append(log)
        .error_kind(ErrorKind::Dependency)
        .reduced_environment()
        .env("FIRESTONE_CONFIG_DIR", paths.config_dir().as_os_str())
        .env("FIRESTONE_DATA_DIR", paths.data_dir().as_os_str())
        .env("FIRESTONE_RUNTIME_DIR", paths.runtime_dir().as_os_str())
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
    let bytes = serde_json::to_vec(identity).map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot serialize process identity")
            .with_source(source)
    })?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_PROCESS_IDENTITY_BYTES) {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "process identity exceeds its durable read bound",
        ));
    }
    atomic::write_with_mode(&paths.machine_process_identity(name)?, &bytes, 0o600)
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

fn read_process_identity_optional(
    paths: &Paths,
    name: &str,
) -> Result<Option<ProcessIdentity>, FirestoneError> {
    let path = paths.machine_process_identity(name)?;
    match path.try_exists() {
        Ok(false) => Ok(None),
        Ok(true) => read_process_identity(paths, name)
            .map(Some)
            .map_err(|error| {
                FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("cannot trust process identity for machine `{name}`"),
                )
                .with_hint("preserve the runtime directory for recovery")
                .with_source(error)
            }),
        Err(source) => Err(filesystem_error(
            ErrorKind::Conflict,
            format!("cannot inspect process identity {}", path.display()),
            source,
        )),
    }
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
    let before = file.metadata().map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot inspect {label} {}", path.display()),
            source,
        )
    })?;
    if !before.is_file() || before.len() > limit {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!("{label} {} is not a bounded regular file", path.display()),
        ));
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let remaining = limit.saturating_add(1).saturating_sub(total);
        if remaining == 0 {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} {} exceeds the {limit} byte limit", path.display()),
            ));
        }
        let read_limit = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| {
            FirestoneError::new(
                ErrorKind::Generic,
                "executable hash limit does not fit usize",
            )
        })?;
        let read = file.read(&mut buffer[..read_limit]).map_err(|source| {
            filesystem_error(
                ErrorKind::Dependency,
                format!("cannot hash {label} {}", path.display()),
                source,
            )
        })?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(FirestoneError::new(
                ErrorKind::Dependency,
                format!("{label} {} exceeds the {limit} byte limit", path.display()),
            ));
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata().map_err(|source| {
        filesystem_error(
            ErrorKind::Dependency,
            format!("cannot re-inspect {label} {}", path.display()),
            source,
        )
    })?;
    if !same_file_snapshot(&before, &after) || total != after.len() {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("{label} {} changed while it was read", path.display()),
        )
        .with_hint("retry with an immutable executable file"));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn same_file_snapshot(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
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

fn ensure_launch_before_deadline(
    deadline: Instant,
    phase: &'static str,
) -> Result<(), FirestoneError> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(FirestoneError::new(
            ErrorKind::Timeout,
            format!("overall shim launch deadline expired while {phase}"),
        ))
    }
}

fn launch_phase_timeout(
    deadline: Instant,
    cap: Duration,
    phase: &'static str,
) -> Result<Duration, FirestoneError> {
    ensure_launch_before_deadline(deadline, phase)?;
    Ok(cap.min(deadline.saturating_duration_since(Instant::now())))
}

fn terminate_launch_vmm(
    vmm: &mut OwnedVmm,
    launch_deadline: Instant,
) -> Result<ExitStatus, FirestoneError> {
    let grace = CHILD_TERM_GRACE.min(launch_deadline.saturating_duration_since(Instant::now()));
    vmm.terminate_and_reap_before(true, grace, launch_deadline)
}

fn stop_overall_timeout(
    guest_timeout: Duration,
    control_timeout: Duration,
) -> Result<Duration, FirestoneError> {
    guest_timeout
        .checked_add(STOP_API_PHASE_CAP.saturating_mul(3))
        .and_then(|total| total.checked_add(CHILD_TERM_GRACE.saturating_mul(2)))
        .and_then(|total| total.checked_add(control_timeout))
        .ok_or_else(|| FirestoneError::new(ErrorKind::Usage, "stop deadline is out of range"))
}

fn stop_phase_timeout(deadline: Instant, cap: Duration) -> Result<Duration, FirestoneError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(FirestoneError::new(
            ErrorKind::Timeout,
            "overall shim stop deadline expired",
        ))
    } else {
        Ok(cap.min(remaining))
    }
}

fn enter_shim_session() -> Result<(), FirestoneError> {
    match setsid() {
        Ok(_) => Ok(()),
        Err(Errno::EPERM)
            if getpgrp() == getpid()
                && getsid(Some(getpid())).is_ok_and(|session| session == getpid()) =>
        {
            Ok(())
        }
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
    launch_artifact: PathBuf,
    launch_sha256: Option<String>,
    launch_argv: Vec<OsString>,
    launch_binding: Option<String>,
) -> Result<ProcessRecord, FirestoneError> {
    #[cfg(target_os = "linux")]
    let (executable, metadata, argv_hex) = {
        let proc_dir = PathBuf::from("/proc").join(pid.to_string());
        let executable_link = proc_dir.join("exe");
        let executable = fs::canonicalize(&executable_link).map_err(|source| {
            filesystem_error(
                ErrorKind::Conflict,
                format!(
                    "cannot resolve process executable {}",
                    executable_link.display()
                ),
                source,
            )
        })?;
        let metadata = fs::metadata(&executable_link).map_err(|source| {
            filesystem_error(
                ErrorKind::Conflict,
                format!(
                    "cannot inspect process executable {}",
                    executable_link.display()
                ),
                source,
            )
        })?;
        let argv_hex = linux_process_argv_hex(pid)?;
        if let Some(binding) = launch_binding.as_deref() {
            if !linux_process_environment_has(pid, "FIRESTONE_LAUNCH_BINDING", binding)? {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("process {pid} did not preserve its launch binding"),
                ));
            }
        }
        (executable, metadata, argv_hex)
    };
    #[cfg(not(target_os = "linux"))]
    let (executable, metadata, argv_hex) = {
        let executable = fs::canonicalize(&launch_artifact).map_err(|source| {
            filesystem_error(
                ErrorKind::Conflict,
                format!(
                    "cannot resolve process executable {}",
                    launch_artifact.display()
                ),
                source,
            )
        })?;
        let metadata = fs::metadata(&executable).map_err(|source| {
            filesystem_error(
                ErrorKind::Conflict,
                format!("cannot inspect process executable {}", executable.display()),
                source,
            )
        })?;
        (executable, metadata, encode_os_argv(&launch_argv))
    };
    let launch_argv_hex = launch_sha256.as_ref().map(|_| encode_os_argv(&launch_argv));
    let argv_mismatch = launch_argv_hex
        .as_deref()
        .is_some_and(|expected| !argv_matches_launch(&argv_hex, expected));
    if argv_mismatch {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("process {pid} argv does not match the immutable launch plan"),
        ));
    }
    Ok(ProcessRecord {
        pid,
        process_group,
        executable,
        executable_dev: metadata.dev(),
        executable_ino: metadata.ino(),
        argv_hex,
        launch_artifact: launch_sha256.as_ref().map(|_| launch_artifact),
        launch_argv_hex,
        launch_binding,
        launch_sha256,
        uid: nix::unistd::getuid().as_raw(),
        start_time_ticks: process_start_time(pid)?,
    })
}

fn encode_os_argv(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|argument| hex_bytes(argument.as_os_str().as_bytes()))
        .collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn argv_matches_launch(actual: &[String], launch: &[String]) -> bool {
    actual == launch
        || (launch.len() > 1
            && actual.len() >= launch.len() - 1
            && actual[actual.len() - (launch.len() - 1)..] == launch[1..])
}

#[cfg(target_os = "linux")]
fn linux_process_argv_hex(pid: u32) -> Result<Vec<String>, FirestoneError> {
    let path = PathBuf::from("/proc").join(pid.to_string()).join("cmdline");
    let bytes = fs::read(&path).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot read process argv from {}", path.display()),
            source,
        )
    })?;
    if bytes.is_empty() || bytes.len() > MAX_LAUNCH_PLAN_BYTES as usize * 2 {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "process argv {} is empty or exceeds its bound",
                path.display()
            ),
        ));
    }
    let mut argv = bytes
        .split(|byte| *byte == 0)
        .map(hex_bytes)
        .collect::<Vec<_>>();
    if argv.last().is_some_and(String::is_empty) {
        let _ = argv.pop();
    }
    if argv.is_empty() || argv.iter().any(String::is_empty) {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("process argv {} is malformed", path.display()),
        ));
    }
    Ok(argv)
}

#[cfg(target_os = "linux")]
fn linux_process_environment_has(
    pid: u32,
    key: &str,
    expected: &str,
) -> Result<bool, FirestoneError> {
    let path = PathBuf::from("/proc").join(pid.to_string()).join("environ");
    let bytes = fs::read(&path).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot read process environment from {}", path.display()),
            source,
        )
    })?;
    if bytes.len() > MAX_PROCESS_IDENTITY_BYTES as usize {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("process environment {} exceeds its bound", path.display()),
        ));
    }
    let mut binding = Vec::with_capacity(key.len() + expected.len() + 1);
    binding.extend_from_slice(key.as_bytes());
    binding.push(b'=');
    binding.extend_from_slice(expected.as_bytes());
    Ok(bytes.split(|byte| *byte == 0).any(|entry| entry == binding))
}

fn recover_vmm_record(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &MachineState,
    prior_identity: Option<&ProcessIdentity>,
    current_shim_pid: u32,
) -> Result<(ProcessRecord, bool), FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        recover_linux_vmm_record(paths, name, plan, state, prior_identity, current_shim_pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (paths, name, plan, state, prior_identity, current_shim_pid);
        Err(FirestoneError::new(
            ErrorKind::Dependency,
            "VMM adoption requires the audited Linux process identity backend",
        ))
    }
}

#[cfg(target_os = "linux")]
fn recover_linux_vmm_record(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &MachineState,
    prior_identity: Option<&ProcessIdentity>,
    current_shim_pid: u32,
) -> Result<(ProcessRecord, bool), FirestoneError> {
    if !matches!(
        state.status,
        MachineStatus::Starting
            | MachineStatus::Running
            | MachineStatus::Stopping
            | MachineStatus::Failed
    ) {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!("machine `{name}` is not in a recoverable active state"),
        ));
    }
    if let Some(old_shim_pid) = state.shim_pid.filter(|old| *old != current_shim_pid) {
        if matches!(
            process_state(old_shim_pid)?,
            Some(process_state) if process_state != 'Z'
        ) {
            let old_shim = prior_identity
                .map(|identity| &identity.shim)
                .filter(|record| record.pid == old_shim_pid)
                .ok_or_else(|| {
                    FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("cannot prove recorded shim pid {old_shim_pid} stale"),
                    )
                })?;
            verify_linux_process(old_shim)?;
            return Err(FirestoneError::new(
                ErrorKind::AlreadyRunning,
                format!("machine `{name}` still has a verified live shim"),
            ));
        }
    }

    let launch_argv = vmm_argv(paths, name, plan)?;
    let launch_binding = vmm_launch_binding(plan);
    let mut candidates = Vec::new();
    if let Some(record) = prior_identity.and_then(|identity| identity.vmm.as_ref()) {
        if recorded_process_alive(record)? {
            verify_linux_process(record)?;
            candidates.push(record.clone());
        }
    }
    for record in scan_linux_vmm_candidates(plan, &launch_argv, &launch_binding)? {
        if !candidates
            .iter()
            .any(|candidate| candidate.pid == record.pid)
        {
            candidates.push(record);
        }
    }
    if let Some(recorded_pid) = state.vmm_pid {
        candidates.retain(|record| record.pid == recorded_pid);
    }
    if candidates.len() != 1 {
        return Err(FirestoneError::new(
            ErrorKind::Conflict,
            format!(
                "machine `{name}` recovery found {} strictly matching VMM processes",
                candidates.len()
            ),
        )
        .with_hint("refusing to launch a duplicate or guess between process identities"));
    }
    let record = candidates.remove(0);
    verify_linux_process(&record)?;
    let api_ready = match VmmApi::new(&paths.machine_api_socket(name)?, plan.timeouts()?.api)
        .vmm_ping()
    {
        Ok(ping) => {
            if ping.pid != i64::from(record.pid) {
                return Err(FirestoneError::new(
                    ErrorKind::Conflict,
                    format!(
                        "VMM API pid {} does not match recovered pid {}",
                        ping.pid, record.pid
                    ),
                ));
            }
            true
        }
        Err(error) if error.kind() == ErrorKind::NotRunning => false,
        Err(error) => {
            write_shim_log(&format!(
                "machine `{name}` adopted verified VMM pid {} while API liveness remained ambiguous ({})",
                record.pid,
                error.kind()
            ));
            false
        }
    };
    Ok((record, api_ready))
}

#[cfg(target_os = "linux")]
fn scan_linux_vmm_candidates(
    plan: &LaunchPlan,
    launch_argv: &[OsString],
    launch_binding: &str,
) -> Result<Vec<ProcessRecord>, FirestoneError> {
    let mut records = Vec::new();
    let entries = fs::read_dir("/proc").map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            "cannot enumerate Linux processes for VMM recovery".to_owned(),
            source,
        )
    })?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.uid() != nix::unistd::getuid().as_raw() {
            continue;
        }
        let binding_matches =
            match linux_process_environment_has(pid, "FIRESTONE_LAUNCH_BINDING", launch_binding) {
                Ok(matches) => matches,
                Err(_) => continue,
            };
        if !binding_matches {
            continue;
        }
        let expected_group = match i32::try_from(pid) {
            Ok(group) => group,
            Err(_) => continue,
        };
        let group = match nix::unistd::getpgid(Some(pid_from_u32(pid)?)) {
            Ok(group) if group.as_raw() == expected_group => pid,
            _ => continue,
        };

        let record = match process_record(
            pid,
            group,
            plan.vmm_binary.clone(),
            Some(plan.vmm_binary_sha256.clone()),
            launch_argv.to_vec(),
            Some(launch_binding.to_owned()),
        ) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if verify_linux_process(&record).is_ok() {
            records.push(record);
        }
    }
    records.sort_by_key(|record| record.pid);
    Ok(records)
}

fn find_launch_sidecar_survivors(
    plan: &LaunchPlan,
    identity: &ProcessIdentity,
    uid: u32,
) -> Result<BTreeMap<String, ProcessRecord>, FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        let mut survivors = BTreeMap::new();
        for expectation in sidecar_expectations(plan, uid)? {
            let mut candidates = Vec::new();
            if let Some(record) = identity.sidecars.get(&expectation.name) {
                if recorded_process_alive(record)? {
                    if !process_record_matches_sidecar(record, &expectation) {
                        return Err(FirestoneError::new(
                            ErrorKind::Conflict,
                            format!(
                                "launched {} identity does not match its exact plan",
                                expectation.name
                            ),
                        ));
                    }
                    verify_linux_process(record)?;
                    candidates.push(record.clone());
                }
            }
            for record in scan_linux_sidecar_candidates(&expectation)? {
                if !candidates
                    .iter()
                    .any(|candidate| candidate.pid == record.pid)
                {
                    candidates.push(record);
                }
            }
            match candidates.len() {
                0 => {}
                1 => {
                    let record = candidates.remove(0);
                    survivors.insert(expectation.name, record);
                }
                count => {
                    return Err(FirestoneError::new(
                        ErrorKind::Conflict,
                        format!(
                            "launch cleanup found {count} matching {} processes",
                            expectation.name
                        ),
                    ));
                }
            }
        }
        Ok(survivors)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (plan, uid);
        if identity.sidecars.is_empty() {
            Ok(BTreeMap::new())
        } else {
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                "cannot prove sidecar absence after launch failure on this platform",
            ))
        }
    }
}

fn find_launch_survivor(
    paths: &Paths,
    name: &str,
    plan: &LaunchPlan,
    state: &MachineState,
    identity: &ProcessIdentity,
) -> Result<Option<ProcessRecord>, FirestoneError> {
    if state.vmm_pid.is_some_and(take_owner_reap_proof) {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(record) = identity.vmm.as_ref() {
            if recorded_process_alive(record)? {
                verify_linux_process(record)?;
                return Ok(Some(record.clone()));
            }
        }
        let launch_argv = vmm_argv(paths, name, plan)?;
        let binding = vmm_launch_binding(plan);
        let mut records = scan_linux_vmm_candidates(plan, &launch_argv, &binding)?;
        match records.len() {
            0 if state.vmm_pid.is_none() => Ok(None),
            0 => Err(FirestoneError::new(
                ErrorKind::Conflict,
                "launch cleanup could not prove the spawned VMM was reaped",
            )),
            1 => Ok(records.pop()),
            count => Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("launch cleanup found {count} matching VMM processes"),
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (paths, name, plan, identity);
        if state.vmm_pid.is_none() {
            Ok(None)
        } else {
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                "cannot prove VMM absence after launch failure on this platform",
            ))
        }
    }
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

#[cfg(target_os = "linux")]
fn verify_linux_process(record: &ProcessRecord) -> Result<(), FirestoneError> {
    let proc_dir = PathBuf::from("/proc").join(record.pid.to_string());
    let owner = fs::metadata(&proc_dir).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot verify process pid {} owner", record.pid),
            source,
        )
    })?;
    if owner.uid() != record.uid || record.start_time_ticks.is_none() {
        return Err(reused_pid_error(record.pid));
    }
    let executable_link = proc_dir.join("exe");
    let executable = fs::canonicalize(&executable_link).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!("cannot verify process pid {} executable", record.pid),
            source,
        )
    })?;
    let executable_metadata = fs::metadata(&executable_link).map_err(|source| {
        filesystem_error(
            ErrorKind::Conflict,
            format!(
                "cannot verify process pid {} executable identity",
                record.pid
            ),
            source,
        )
    })?;
    if executable != record.executable
        || executable_metadata.dev() != record.executable_dev
        || executable_metadata.ino() != record.executable_ino
        || linux_process_argv_hex(record.pid)? != record.argv_hex
        || process_start_time(record.pid)? != record.start_time_ticks
    {
        return Err(reused_pid_error(record.pid));
    }
    let group = nix::unistd::getpgid(Some(pid_from_u32(record.pid)?)).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("cannot verify process pid {} process group", record.pid),
        )
        .with_source(io::Error::from_raw_os_error(source as i32))
    })?;
    let expected_group =
        i32::try_from(record.process_group).map_err(|_| reused_pid_error(record.pid))?;
    if group.as_raw() != expected_group {
        return Err(reused_pid_error(record.pid));
    }
    match (
        record.launch_artifact.as_deref(),
        record.launch_sha256.as_deref(),
        record.launch_argv_hex.as_deref(),
        record.launch_binding.as_deref(),
    ) {
        (Some(artifact), Some(digest), Some(launch_argv), Some(binding)) => {
            let artifact_metadata = fs::metadata(artifact).map_err(|source| {
                filesystem_error(
                    ErrorKind::Conflict,
                    format!("cannot verify launch artifact {}", artifact.display()),
                    source,
                )
            })?;
            let artifact_mode = artifact_metadata.mode() & 0o7777;
            if !artifact_metadata.is_file()
                || artifact_metadata.uid() != record.uid
                || !matches!(artifact_mode, 0o700 | 0o755)
                || launch_argv
                    .first()
                    .ne(&encode_os_argv(&[artifact.as_os_str().to_os_string()]).first())
                || !argv_matches_launch(&record.argv_hex, launch_argv)
                || !linux_process_environment_has(record.pid, "FIRESTONE_LAUNCH_BINDING", binding)?
            {
                return Err(reused_pid_error(record.pid));
            }
            let actual_digest = hash_file(artifact, MAX_EXECUTABLE_BYTES, "launch artifact")
                .map_err(|error| {
                    FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("cannot verify launch artifact {}", artifact.display()),
                    )
                    .with_source(error)
                })?;
            if actual_digest != digest {
                return Err(reused_pid_error(record.pid));
            }
        }
        (None, None, None, None) => {}
        _ => return Err(reused_pid_error(record.pid)),
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

fn recorded_process_alive(record: &ProcessRecord) -> Result<bool, FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        if !matches!(process_state(record.pid)?, Some(state) if state != 'Z') {
            return Ok(false);
        }
        let current_start = process_parent_and_start(record.pid)?.map(|(_, start)| start);
        if current_start != record.start_time_ticks {
            return Ok(false);
        }
        match verify_linux_process(record) {
            Ok(()) => Ok(true),

            Err(mut error) => {
                for _ in 0..3 {
                    thread::sleep(Duration::from_millis(1));
                    let exited = matches!(process_state(record.pid)?, None | Some('Z'));
                    let reused = process_parent_and_start(record.pid)?
                        .map(|(_, start)| start)
                        .ne(&record.start_time_ticks);
                    if exited || reused {
                        return Ok(false);
                    }
                    match verify_linux_process(record) {
                        Ok(()) => return Ok(true),
                        Err(next_error) => error = next_error,
                    }
                }
                Err(error)
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = record;
        Err(FirestoneError::new(
            ErrorKind::Conflict,
            "unsupervised process observation requires Linux /proc identity",
        )
        .with_hint("restore supervision or stop the verified VMM manually"))
    }
}

#[cfg(target_os = "linux")]
fn signal_verified_group(
    record: &ProcessRecord,
    signal: ProcessSignal,
    preserved_roots: &PreservedProcessRoots,
) -> Result<(), FirestoneError> {
    signal_verified_tree_preserving(record, signal, preserved_roots)
}

#[cfg(not(target_os = "linux"))]
fn signal_verified_group(
    _record: &ProcessRecord,
    _signal: ProcessSignal,
    _preserved_roots: &PreservedProcessRoots,
) -> Result<(), FirestoneError> {
    Err(FirestoneError::new(
        ErrorKind::Conflict,
        "refusing to signal an unsupervised VMM without Linux process identity",
    ))
}

fn wait_for_record_exit(record: &ProcessRecord, timeout: Duration) -> Result<(), FirestoneError> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        FirestoneError::new(ErrorKind::Usage, "process wait deadline is out of range")
    })?;
    while Instant::now() < deadline {
        if !recorded_process_alive(record)? {
            return Ok(());
        }
        thread::sleep(LOOP_INTERVAL);
    }
    if recorded_process_alive(record)? {
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

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct DescendantIdentity {
    pid: u32,
    uid: u32,
    start_time_ticks: u64,
    pidfd: rustix::fd::OwnedFd,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
struct DescendantIdentity;

fn snapshot_owned_descendants_preserving(
    root_pid: u32,
    preserved_roots: &PreservedProcessRoots,
) -> Result<BTreeMap<u32, DescendantIdentity>, FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        let shim_pid = std::process::id();
        let shim_start = process_parent_and_start(shim_pid)?
            .map(|(_, start)| start)
            .ok_or_else(|| {
                FirestoneError::new(
                    ErrorKind::Conflict,
                    "cannot identify shim process start time",
                )
            })?;
        let mut descendants = BTreeMap::new();
        let mut pending = vec![(shim_pid, shim_start)];
        if root_pid != 0 && root_pid != shim_pid {
            if let Some((_, root_start)) = process_parent_and_start(root_pid)? {
                pending.push((root_pid, root_start));
            }
        }
        while let Some((parent, parent_start)) = pending.pop() {
            let current_start = process_parent_and_start(parent)?.map(|(_, start)| start);
            if current_start != Some(parent_start) {
                continue;
            }
            for pid in linux_child_pids(parent)? {
                if pid == root_pid || pid == shim_pid || descendants.contains_key(&pid) {
                    continue;
                }
                let raw = i32::try_from(pid).map_err(|_| {
                    FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("descendant pid {pid} does not fit pid_t"),
                    )
                })?;
                let Some(rustix_pid) = rustix::process::Pid::from_raw(raw) else {
                    continue;
                };
                let pidfd = match rustix::process::pidfd_open(
                    rustix_pid,
                    rustix::process::PidfdFlags::empty(),
                ) {
                    Ok(pidfd) => pidfd,
                    Err(source) if source == rustix::io::Errno::SRCH => continue,
                    Err(source) => {
                        return Err(FirestoneError::new(
                            ErrorKind::Conflict,
                            format!("cannot open pidfd for descendant pid {pid}"),
                        )
                        .with_source(io::Error::from_raw_os_error(source.raw_os_error())));
                    }
                };
                let Some((actual_parent, start_time_ticks)) = process_parent_and_start(pid)? else {
                    continue;
                };
                if preserved_roots
                    .get(&pid)
                    .is_some_and(|expected_start| match expected_start {
                        Some(expected_start) => *expected_start == start_time_ticks,
                        None => true,
                    })
                {
                    continue;
                }
                if actual_parent != parent && actual_parent != shim_pid {
                    continue;
                }
                let proc_dir = PathBuf::from("/proc").join(pid.to_string());
                let metadata = match fs::metadata(&proc_dir) {
                    Ok(metadata) => metadata,
                    Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(filesystem_error(
                            ErrorKind::Conflict,
                            format!("cannot inspect descendant pid {pid}"),
                            source,
                        ));
                    }
                };
                if metadata.uid() != nix::unistd::getuid().as_raw()
                    || process_parent_and_start(pid)?
                        .as_ref()
                        .is_none_or(|identity| identity != &(actual_parent, start_time_ticks))
                {
                    continue;
                }
                descendants.insert(
                    pid,
                    DescendantIdentity {
                        pid,
                        uid: metadata.uid(),
                        start_time_ticks,
                        pidfd,
                    },
                );
                pending.push((pid, start_time_ticks));
            }
        }
        Ok(descendants)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root_pid, preserved_roots);
        Ok(BTreeMap::new())
    }
}

#[cfg(target_os = "linux")]
fn linux_child_pids(parent: u32) -> Result<Vec<u32>, FirestoneError> {
    let task_dir = PathBuf::from("/proc").join(parent.to_string()).join("task");
    let threads = match fs::read_dir(&task_dir) {
        Ok(threads) => threads,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Conflict,
                format!("cannot enumerate threads from {}", task_dir.display()),
                source,
            ));
        }
    };
    let mut children = Vec::new();
    for thread in threads {
        let thread = match thread {
            Ok(thread) => thread,
            Err(_) => continue,
        };
        let path = thread.path().join("children");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(filesystem_error(
                    ErrorKind::Conflict,
                    format!("cannot enumerate children from {}", path.display()),
                    source,
                ));
            }
        };
        for value in text.split_whitespace() {
            children.push(value.parse::<u32>().map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Conflict,
                    format!("invalid child pid in {}", path.display()),
                )
                .with_source(source)
            })?);
        }
    }
    children.sort_unstable();
    children.dedup();
    Ok(children)
}

#[cfg(target_os = "linux")]
fn process_parent_and_start(pid: u32) -> Result<Option<(u32, u64)>, FirestoneError> {
    let path = PathBuf::from("/proc").join(pid.to_string()).join("stat");
    let stat = match fs::read_to_string(&path) {
        Ok(stat) => stat,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(filesystem_error(
                ErrorKind::Conflict,
                format!("cannot read process identity from {}", path.display()),
                source,
            ));
        }
    };
    let close = stat.rfind(") ").ok_or_else(|| {
        FirestoneError::new(
            ErrorKind::Conflict,
            format!("process stat {} is malformed", path.display()),
        )
    })?;
    let fields = stat[close + 2..].split_whitespace().collect::<Vec<_>>();
    let parent = fields
        .get(1)
        .ok_or_else(|| reused_pid_error(pid))?
        .parse::<u32>()
        .map_err(|source| reused_pid_error(pid).with_source(source))?;
    let start = fields
        .get(19)
        .ok_or_else(|| reused_pid_error(pid))?
        .parse::<u64>()
        .map_err(|source| reused_pid_error(pid).with_source(source))?;
    Ok(Some((parent, start)))
}

fn signal_descendants(
    descendants: &BTreeMap<u32, DescendantIdentity>,
    signal: ProcessSignal,
) -> Result<(), FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        let signal = match signal {
            ProcessSignal::Hangup => rustix::process::Signal::HUP,
            ProcessSignal::Interrupt => rustix::process::Signal::INT,
            ProcessSignal::Quit => rustix::process::Signal::QUIT,
            ProcessSignal::Terminate => rustix::process::Signal::TERM,
            ProcessSignal::Kill => rustix::process::Signal::KILL,
            // `SIGWINCH` belongs to terminal transports, not to the VMM
            // supervision tree; the mapping keeps the translation total.
            ProcessSignal::WindowChange => rustix::process::Signal::WINCH,
        };
        for identity in descendants.values() {
            // The pidfd was opened before identity inspection and remains the
            // signal authority even if the numeric pid exits and is reused.
            if let Ok(metadata) =
                fs::metadata(PathBuf::from("/proc").join(identity.pid.to_string()))
            {
                if metadata.uid() != identity.uid
                    || process_start_time(identity.pid)? != Some(identity.start_time_ticks)
                {
                    return Err(FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("descendant pid {} changed identity", identity.pid),
                    ));
                }
            }
            match rustix::process::pidfd_send_signal(&identity.pidfd, signal) {
                Ok(()) => {}
                Err(source) if source == rustix::io::Errno::SRCH => {}
                Err(source) => {
                    return Err(FirestoneError::new(
                        ErrorKind::Generic,
                        format!("cannot signal descendant pid {}", identity.pid),
                    )
                    .with_source(io::Error::from_raw_os_error(source.raw_os_error())));
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (descendants, signal);
    }
    Ok(())
}

fn drain_adopted_children_preserving(
    timeout: Duration,
    preserved_roots: &PreservedProcessRoots,
) -> Result<(), FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Usage, "child drain deadline is out of range")
        })?;
        loop {
            let descendants = snapshot_owned_descendants_preserving(0, preserved_roots)?;
            if descendants.is_empty() {
                return Ok(());
            }
            signal_descendants(&descendants, ProcessSignal::Kill)?;
            let mut reaped_any = false;
            for descendant in descendants.values() {
                let raw = i32::try_from(descendant.pid).map_err(|_| {
                    FirestoneError::new(
                        ErrorKind::Conflict,
                        format!("descendant pid {} does not fit pid_t", descendant.pid),
                    )
                })?;
                match waitpid(Pid::from_raw(raw), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => {}
                    Ok(_) => reaped_any = true,
                    Err(source) => {
                        return Err(FirestoneError::new(
                            ErrorKind::Generic,
                            format!("cannot reap adopted VMM descendant {}", descendant.pid),
                        )
                        .with_source(io::Error::from_raw_os_error(source as i32)));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(FirestoneError::new(
                    ErrorKind::Timeout,
                    "VMM descendants did not terminate before the drain deadline",
                )
                .with_hint("runtime recovery evidence was preserved"));
            }
            if !reaped_any {
                thread::sleep(
                    LOOP_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (timeout, preserved_roots);
        Ok(())
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

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const PEER_CREDENTIAL_BACKEND_SUPPORTED: bool = true;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
const PEER_CREDENTIAL_BACKEND_SUPPORTED: bool = false;

fn authorize_peer(stream: &UnixStream, expected_uid: u32) -> Result<(), FirestoneError> {
    if !PEER_CREDENTIAL_BACKEND_SUPPORTED {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            "this Unix target has no audited shim peer-credential backend",
        ));
    }
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
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        let _ = (stream, expected_uid);
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            "this Unix target has no audited shim peer-credential backend",
        ));
    }
    Ok(())
}

fn read_request(
    mut stream: &UnixStream,
    timeout: Duration,
) -> Result<ControlRequest, FirestoneError> {
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
    let frame = read_frame(stream, MAX_CONTROL_REQUEST_BYTES, deadline, None)?;
    let mut trailing = [0_u8; 1];
    match stream.read(&mut trailing) {
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
            ErrorKind::InvalidSpec,
            format!("cannot address shim socket {}", path.display()),
        )
        .with_hint("set FIRESTONE_RUNTIME_DIR to a shorter absolute path")
        .with_source(io::Error::from(source))
    })?;
    loop {
        if Instant::now() >= deadline {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!("timed out connecting to shim socket {}", path.display()),
            )
            .with_hint("start the machine or inspect its shim log, then retry"));
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
                    ErrorKind::Generic,
                    format!("cannot connect to shim socket {}", path.display()),
                )
                .with_hint("inspect the private runtime directory and shim socket permissions")
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
        )
        .with_hint("start the machine or inspect its shim log, then retry"));
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
        )
        .with_hint("start the machine or inspect its shim log, then retry"));
    }
    let pending = getsockopt(stream, SocketError).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot inspect shim socket {} connection", path.display()),
        )
        .with_source(io::Error::from(source))
    })?;
    if pending == 0 {
        return Ok(());
    }
    let stale = matches!(
        pending,
        value if value == Errno::ENOENT as i32
            || value == Errno::ECONNREFUSED as i32
            || value == Errno::EAGAIN as i32
    );
    let (kind, hint) = if stale {
        (
            ErrorKind::NotRunning,
            "start the machine or wait for its shim socket, then retry",
        )
    } else {
        (
            ErrorKind::Generic,
            "inspect the private runtime directory and shim socket permissions",
        )
    };
    Err(FirestoneError::new(
        kind,
        format!("cannot connect to shim socket {}", path.display()),
    )
    .with_hint(hint)
    .with_source(io::Error::from_raw_os_error(pending)))
}
fn read_frame(
    mut stream: impl Read,
    limit: usize,
    deadline: Instant,
    cancellation: Option<&AtomicBool>,
) -> Result<Vec<u8>, FirestoneError> {
    let mut frame = Vec::with_capacity(limit.min(4096));
    let mut byte = [0_u8; 1];
    loop {
        if cancellation.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(start_interrupted_error());
        }
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

fn isolate_client_write(name: &str, operation: &'static str, result: Result<(), FirestoneError>) {
    if let Err(error) = result {
        write_shim_log(&format!(
            "machine `{name}` {operation} response detached ({}); supervisor continues",
            error.kind()
        ));
    }
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
    deadline: Option<Instant>,
    machine: String,
    write_failure_logged: bool,
}

impl ProtocolSink {
    fn with_deadline(stream: UnixStream, deadline: Instant, machine: &str) -> Self {
        Self {
            stream: Some(stream),
            timeout: Duration::ZERO,
            deadline: Some(deadline),
            machine: machine.to_owned(),
            write_failure_logged: false,
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
                let deadline = match self.deadline {
                    Some(deadline) => {
                        ensure_launch_before_deadline(deadline, "writing launch response")?;
                        deadline
                    }
                    None => Instant::now()
                        .checked_add(self.timeout)
                        .ok_or_else(|| protocol_error("shim event deadline is out of range"))?,
                };
                write_frame(&mut stream, &bytes, deadline)
            });
        match result {
            Ok(()) => self.stream = Some(stream),
            Err(error) => self.log_write_failure(error.kind()),
        }
    }

    fn log_write_failure(&mut self, kind: ErrorKind) {
        if self.write_failure_logged {
            return;
        }
        write_shim_log(&format!(
            "machine `{}` control response detached ({kind}); lifecycle continues",
            self.machine
        ));
        self.write_failure_logged = true;
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
    paths: &Paths,
    name: &str,
    socket: &Path,
    process: &mut ManagedProcess,
    deadline: Instant,
    cancellation: Option<&AtomicBool>,
) -> Result<(), FirestoneError> {
    loop {
        if cancellation.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(start_interrupted_error());
        }
        if process.observe_exit()? {
            let status = process.wait()?;
            return Err(process.exit_error(
                status,
                &format!("shim for machine `{name}` exited before its control socket was ready"),
                "inspect the machine shim log and retry the machine start",
            ));
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
        let identity_matches = read_process_identity_optional(paths, name)?
            .is_some_and(|identity| identity.shim.pid == process.id());
        if identity_matches {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match ShimClient::new(socket, remaining.min(Duration::from_millis(100))).ping() {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotRunning | ErrorKind::Timeout) => {}
                Err(error) => return Err(error),
            }
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

fn terminate_recovered_identity_processes(
    identity: &ProcessIdentity,
) -> Result<(), FirestoneError> {
    #[cfg(target_os = "linux")]
    {
        if let Some(vmm) = identity.vmm.as_ref() {
            terminate_recovered_process(vmm)?;
        }
        for sidecar in identity.sidecars.values() {
            terminate_recovered_process(sidecar)?;
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if identity.vmm.is_some() || !identity.sidecars.is_empty() {
            Err(FirestoneError::new(
                ErrorKind::Dependency,
                "orphan process cleanup requires the audited Linux identity backend",
            ))
        } else {
            Ok(())
        }
    }
}

fn terminate_cancelled_shim(
    paths: &Paths,
    prepared: PreparedStart,
    process: &mut ManagedProcess,
) -> Result<(), FirestoneError> {
    if process.observe_exit()? {
        let _ = process.wait()?;
    } else {
        process.signal_process(ProcessSignal::Terminate)?;
        if process.wait_timeout(CHILD_TERM_GRACE)?.is_none() {
            process.signal_process(ProcessSignal::Kill)?;
            if process.wait_timeout(CHILD_TERM_GRACE)?.is_none() {
                return Err(FirestoneError::new(
                    ErrorKind::Timeout,
                    format!(
                        "cannot reap cancelled shim pid {} after SIGKILL",
                        process.id()
                    ),
                ));
            }
        }
    }
    if let Some(identity) = read_process_identity_optional(paths, &prepared.name)? {
        terminate_recovered_identity_processes(&identity)?;
    }
    let mut events = Vec::new();
    let lock = MachineLock::acquire(
        &prepared.name,
        &paths.machine_lock(&prepared.name)?,
        &mut events,
    )?;
    cancel_prepared(paths, prepared, &lock)
}

fn terminate_unready_shim(
    paths: &Paths,
    name: &str,
    process: &mut ManagedProcess,
    state: &MachineState,
    error: &FirestoneError,
) -> Result<(), FirestoneError> {
    if process.observe_exit()? {
        let _ = process.wait()?;
    } else {
        process.signal_process(ProcessSignal::Kill)?;
        if process.wait_timeout(CHILD_TERM_GRACE)?.is_none() {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "cannot reap unready shim pid {} after SIGKILL",
                    process.id()
                ),
            ));
        }
    }
    if let Some(identity) = read_process_identity_optional(paths, name)? {
        terminate_recovered_identity_processes(&identity)?;
    }
    let mut events = Vec::new();
    let lock = MachineLock::acquire(name, &paths.machine_lock(name)?, &mut events)?;
    let mut failed = state.clone();
    failed.status = MachineStatus::Failed;
    failed.shim_pid = None;
    failed.vmm_pid = None;
    failed.sidecar_pids.clear();
    failed.started_at = None;
    failed.degraded.clear();
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

fn safe_vmm_failure_reason(paths: &Paths, name: &str, fallback: &str) -> String {
    match vmm_failure_reason(paths, name, fallback) {
        Ok(reason) => reason,
        Err(error) => {
            write_shim_log(&format!(
                "machine `{name}` diagnostic log was unavailable ({}); preserving lifecycle result",
                error.kind()
            ));
            fallback.to_owned()
        }
    }
}

fn vmm_failure_reason(paths: &Paths, name: &str, fallback: &str) -> Result<String, FirestoneError> {
    let path = paths.machine_vmm_log(name)?;
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(&path)
    {
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
    let metadata = file.metadata().map_err(|source| {
        filesystem_error(
            ErrorKind::Generic,
            format!("cannot inspect VMM log {}", path.display()),
            source,
        )
    })?;
    if !metadata.is_file() || metadata.uid() != paths.uid() || metadata.mode() & 0o7777 != 0o600 {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "VMM log {} is not an owned mode-0600 regular file",
                path.display()
            ),
        ));
    }
    let length = metadata.len();
    file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)))
        .map_err(|source| {
            filesystem_error(
                ErrorKind::Generic,
                format!("cannot seek VMM log {}", path.display()),
                source,
            )
        })?;
    let mut tail = Vec::with_capacity(LOG_TAIL_BYTES as usize);
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
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        time::Duration,
    };

    #[cfg(target_os = "linux")]
    use super::{ProcessRecord, launch_bound_sidecar_record, process_record, verify_linux_process};
    use super::{authorize_peer, import_custom_vmm, resolve_vmm_binary};
    use crate::{
        Arch, DependencyManifest, ErrorKind, FirestoneError, MachineSpec, PathInputs, Paths,
    };
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;

    fn require_error<T>(result: Result<T, FirestoneError>, label: &str) -> FirestoneError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("{label}"),
        }
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
        let record = process_record(
            pid,
            u32::try_from(group.as_raw())?,
            executable.clone(),
            None,
            std::env::args_os().collect(),
            None,
        )?;
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

    #[cfg(target_os = "linux")]
    #[test]
    fn launch_bound_sidecar_record_preserves_immutable_spawn_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let pid = std::process::id();
        let group = nix::unistd::getpgid(Some(super::pid_from_u32(pid)?))?;
        let executable = std::fs::canonicalize(std::env::current_exe()?)?;
        let argv = std::env::args_os().collect::<Vec<_>>();
        let record = launch_bound_sidecar_record(
            pid,
            u32::try_from(group.as_raw())?,
            &executable,
            "immutable-sha256",
            &argv,
            "launch-binding",
        )?;

        assert_eq!(
            record.launch_artifact.as_deref(),
            Some(executable.as_path())
        );
        assert_eq!(record.launch_sha256.as_deref(), Some("immutable-sha256"));
        assert_eq!(record.launch_binding.as_deref(), Some("launch-binding"));
        assert_eq!(record.argv_hex, super::encode_os_argv(&argv));
        assert_eq!(record.launch_argv_hex.as_ref(), Some(&record.argv_hex));
        assert!(record.start_time_ticks.is_some());
        Ok(())
    }

    #[test]
    fn custom_vmm_import_is_descriptor_bound_and_rejects_unsafe_nodes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let mut inputs = PathInputs::capture()?;
        inputs.firestone_home = Some(fs::canonicalize(root.path())?.join("home"));
        let paths = Paths::from_inputs(&inputs)?;
        paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
        paths.ensure_owned_data_directory(&paths.machines_dir(), "machines directory", false)?;
        paths.ensure_owned_data_directory(&paths.machine_dir("vm")?, "machine directory", false)?;

        let script = root.path().join("vmm-wrapper");
        fs::write(
            &script,
            b"#!/bin/sh
exec /bin/true \"$@\"
",
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        let mut spec = MachineSpec::default();
        spec.vmm.binary = Some(script.clone());
        let manifest = DependencyManifest::bundled()?;
        let (published, digest) = resolve_vmm_binary(&paths, &manifest, Arch::X86_64, "vm", &spec)?;
        assert_eq!(published, paths.machine_vmm_executable("vm")?);
        assert_eq!(fs::read(&published)?, fs::read(&script)?);
        assert_eq!(digest, super::sha256_hex(&fs::read(&published)?));
        assert!(!paths.bin_dir().exists());

        let link = root.path().join("vmm-link");
        symlink(&script, &link)?;
        assert!(import_custom_vmm(&paths, "vm", &link).is_err());

        let fifo = root.path().join("vmm-fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IXUSR,
        )?;
        let started = std::time::Instant::now();
        assert!(import_custom_vmm(&paths, "vm", &fifo).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));

        let writable = root.path().join("vmm-writable");
        fs::write(&writable, b"#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o722))?;
        assert!(import_custom_vmm(&paths, "vm", &writable).is_err());

        let oversized = root.path().join("vmm-oversized");
        let oversized_file = fs::File::create(&oversized)?;
        oversized_file.set_len(super::MAX_EXECUTABLE_BYTES + 1)?;
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o700))?;
        assert!(import_custom_vmm(&paths, "vm", &oversized).is_err());

        let racing = root.path().join("vmm-racing");
        let replacement = root.path().join("vmm-replacement");
        fs::write(&racing, vec![b'a'; 8 * 1024 * 1024])?;
        fs::write(&replacement, vec![b'b'; 8 * 1024 * 1024])?;
        fs::set_permissions(&racing, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700))?;
        let moved = root.path().join("vmm-original");
        let racing_for_swap = racing.clone();
        let swap = std::thread::spawn(move || -> std::io::Result<()> {
            std::thread::sleep(Duration::from_millis(1));
            fs::rename(&racing_for_swap, moved)?;
            fs::rename(replacement, racing_for_swap)
        });

        let imported = import_custom_vmm(&paths, "vm", &racing);
        swap.join()
            .map_err(|_| std::io::Error::other("VMM swap thread panicked"))??;
        if let Ok((published, digest)) = imported {
            let bytes = fs::read(&published)?;
            assert!(
                bytes.iter().all(|byte| *byte == b'a') || bytes.iter().all(|byte| *byte == b'b')
            );
            assert_eq!(digest, super::sha256_hex(&bytes));
        }

        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shim_child_reaper_drains_many_children_for_long_lived_callers()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let log = directory.path().join("children.log");
        let mut pids = Vec::new();
        for _ in 0..32 {
            let process = crate::Cmd::new("/bin/true")
                .stdin_null()
                .stdout_append(&log)
                .stderr_append(&log)
                .spawn_process_group()?;
            pids.push(process.id());
            super::ShimChildReaper::start()?.submit(process)?;
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && pids
                .iter()
                .any(|pid| std::path::Path::new("/proc").join(pid.to_string()).exists())
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(pids.iter().all(|pid| {
            std::fs::metadata(std::path::Path::new("/proc").join(pid.to_string())).is_err()
        }));
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
