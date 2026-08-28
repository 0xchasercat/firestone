use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{ErrorKind, FirestoneError, MachineLock, atomic};

/// The only state file version accepted by this release.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StateVersion;

impl StateVersion {
    pub const NUMBER: u32 = 1;
}

impl Serialize for StateVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(Self::NUMBER)
    }
}

impl<'de> Deserialize<'de> for StateVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = u32::deserialize(deserializer)?;
        if version == Self::NUMBER {
            Ok(Self)
        } else {
            Err(de::Error::custom(format!(
                "unsupported state version {version}; expected {}",
                Self::NUMBER
            )))
        }
    }
}

/// Persisted lifecycle state. These are the only v0.1 status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

impl MachineStatus {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

/// Base image selected for a machine.
///
/// A newly-created machine records its canonical reference before any download.
/// The immutable id and SHA-256 become available together after image pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateImage {
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub id: Option<String>,
    pub sha256: Option<String>,
}

/// Why the previous run ended.
///
/// Known lifecycle reasons have fixed strings. A VMM or supervision failure
/// keeps its concrete diagnostic as `Failure`; serde still represents the
/// reason as the string required by `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExitReason {
    GuestShutdown,
    Stale,
    HostReboot,
    Failure(String),
}

impl ExitReason {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::GuestShutdown => "guest shutdown",
            Self::Stale => "stale",
            Self::HostReboot => "host reboot",
            Self::Failure(reason) => reason,
        }
    }
}

impl Serialize for ExitReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExitReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let reason = String::deserialize(deserializer)?;
        match reason.as_str() {
            "guest shutdown" => Ok(Self::GuestShutdown),
            "stale" => Ok(Self::Stale),
            "host reboot" => Ok(Self::HostReboot),
            "" => Err(de::Error::custom("last exit reason cannot be empty")),
            _ => Ok(Self::Failure(reason)),
        }
    }
}

/// Exit details from the most recent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastExit {
    pub at: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: ExitReason,
}

impl<'de> Deserialize<'de> for LastExit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            at: String,
            code: Option<i32>,
            signal: Option<i32>,
            reason: ExitReason,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.code.is_some() && wire.signal.is_some() {
            return Err(de::Error::custom(
                "last exit cannot contain both a code and a signal",
            ));
        }
        Ok(Self {
            at: wire.at,
            code: wire.code,
            signal: wire.signal,
            reason: wire.reason,
        })
    }
}

/// Complete v0.1 contents of one `state.json` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineState {
    pub version: StateVersion,
    pub status: MachineStatus,
    pub image: StateImage,
    pub mac: Option<String>,
    pub cid: u32,
    pub instance_id: Option<String>,
    pub shim_pid: Option<u32>,
    pub vmm_pid: Option<u32>,
    pub sidecar_pids: BTreeMap<String, u32>,
    pub runtime_dir: PathBuf,
    pub started_at: Option<String>,
    pub forwards: Vec<String>,
    pub degraded: Vec<String>,
    pub last_exit: Option<LastExit>,
}

impl MachineState {
    /// Checks invariants that serde enforces while reading but public struct
    /// construction could otherwise bypass before a write.
    pub fn validate(&self) -> Result<(), FirestoneError> {
        if self.image.r#ref.is_empty() {
            return Err(invalid_state("image.ref cannot be empty"));
        }
        match (&self.image.id, &self.image.sha256) {
            (None, None) => {}
            (Some(id), Some(sha256)) if !id.is_empty() && !sha256.is_empty() => {}
            (Some(_), Some(_)) => {
                return Err(invalid_state("image.id and image.sha256 cannot be empty"));
            }
            _ => {
                return Err(invalid_state(
                    "image.id and image.sha256 must be absent or present together",
                ));
            }
        }
        if let Some(last_exit) = &self.last_exit {
            if last_exit.code.is_some() && last_exit.signal.is_some() {
                return Err(invalid_state(
                    "last_exit cannot contain both a code and a signal",
                ));
            }
            if matches!(&last_exit.reason, ExitReason::Failure(reason) if reason.is_empty()) {
                return Err(invalid_state("last_exit.reason cannot be empty"));
            }
        }
        if self.shim_pid == Some(0)
            || self.vmm_pid == Some(0)
            || self.sidecar_pids.values().any(|pid| *pid == 0)
        {
            return Err(invalid_state(
                "process ids in machine state must be nonzero",
            ));
        }
        Ok(())
    }
}

/// A ping implementation supplied by the VMM API client.
///
/// A successful return value means the socket connection and VMM ping request
/// both succeeded. The state module does not define the HTTP request shape.
pub trait VmmPingProbe {
    fn ping(&self, api_socket: &Path) -> Result<bool, FirestoneError>;
}

/// Direct observations used by the pure reconciliation state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessObservation {
    pub vmm_ping: bool,
    pub shim_verified: bool,
    pub runtime_dir_exists: bool,
}

/// Whether a pinged VMM still has its expected shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Supervision {
    Supervised,
    Unsupervised,
}

/// Effective machine state and the live VMM's current supervision status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveMachineState {
    pub state: MachineState,
    pub supervision: Option<Supervision>,
}

/// A state-file change required by reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileRewrite {
    Stopped { reason: ExitReason },
}

/// User-visible state and any state-file change required by an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub status: MachineStatus,
    pub supervision: Option<Supervision>,
    pub rewrite: Option<ReconcileRewrite>,
}

/// Reads filesystem and process observations without changing machine state.
pub fn observe_liveness(
    name: &str,
    state: &MachineState,
    runtime_dir: &Path,
    api_socket: &Path,
    proc_root: &Path,
    vmm: &dyn VmmPingProbe,
) -> Result<LivenessObservation, FirestoneError> {
    let runtime_dir_exists = runtime_dir.try_exists().map_err(|error| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot inspect runtime directory {}", runtime_dir.display()),
        )
        .with_hint("check runtime directory permissions")
        .with_source(error)
    })?;
    let shim_verified = match state.shim_pid {
        Some(pid) => verify_shim_identity(proc_root, pid, name)?,
        None => false,
    };
    let vmm_ping = if runtime_dir_exists {
        vmm.ping(api_socket)?
    } else {
        false
    };

    Ok(LivenessObservation {
        vmm_ping,
        shim_verified,
        runtime_dir_exists,
    })
}

/// Checks `/proc/<pid>/cmdline` for exactly `firestone _shim <name>`.
///
/// `proc_root` is injectable for tests. A missing or exited process returns
/// false. Other read errors are surfaced so callers never accept an
/// unverified pid.
pub fn verify_shim_identity(
    proc_root: &Path,
    pid: u32,
    name: &str,
) -> Result<bool, FirestoneError> {
    if pid == 0 {
        return Ok(false);
    }
    let cmdline_path = proc_root.join(pid.to_string()).join("cmdline");
    let cmdline = match fs::read(&cmdline_path) {
        Ok(cmdline) => cmdline,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "cannot verify shim pid {pid} using {}",
                    cmdline_path.display()
                ),
            )
            .with_hint("check /proc access for the current user")
            .with_source(error));
        }
    };

    let mut argv: Vec<&[u8]> = cmdline.split(|byte| *byte == 0).collect();
    if argv.last().is_some_and(|argument| argument.is_empty()) {
        argv.pop();
    }
    if argv.len() != 3 {
        return Ok(false);
    }

    let executable = Path::new(OsStr::from_bytes(argv[0]));
    let executable_matches = executable
        .file_name()
        .is_some_and(|file_name| file_name.as_bytes() == b"firestone");
    Ok(executable_matches && argv[1] == b"_shim" && argv[2] == name.as_bytes())
}

/// Pure reconciliation over persisted status and direct liveness evidence.
///
/// A `Running` report always comes from a successful VMM ping. With a verified
/// shim but no ping, a recorded stop remains `stopping` and a recorded failure
/// remains `failed`; every other stored status reports `starting`. No state
/// file is changed in those cases.
#[must_use]
pub fn reconcile(
    stored_status: MachineStatus,
    observation: LivenessObservation,
) -> ReconcileReport {
    if observation.vmm_ping {
        return ReconcileReport {
            status: MachineStatus::Running,
            supervision: Some(if observation.shim_verified {
                Supervision::Supervised
            } else {
                Supervision::Unsupervised
            }),
            rewrite: None,
        };
    }

    if observation.shim_verified {
        let status = match stored_status {
            MachineStatus::Stopping => MachineStatus::Stopping,
            MachineStatus::Failed => MachineStatus::Failed,
            MachineStatus::Created
            | MachineStatus::Starting
            | MachineStatus::Running
            | MachineStatus::Stopped => MachineStatus::Starting,
        };
        return ReconcileReport {
            status,
            supervision: None,
            rewrite: None,
        };
    }

    if stored_status.is_active() {
        let reason = if observation.runtime_dir_exists {
            ExitReason::Stale
        } else {
            ExitReason::HostReboot
        };
        return ReconcileReport {
            status: MachineStatus::Stopped,
            supervision: None,
            rewrite: Some(ReconcileRewrite::Stopped { reason }),
        };
    }

    ReconcileReport {
        status: stored_status,
        supervision: None,
        rewrite: None,
    }
}

/// Applies a pure reconciliation decision to an in-memory state value.
#[must_use]
pub fn reconciled_state(
    state: &MachineState,
    report: &ReconcileReport,
    reconciled_at: &str,
) -> Option<MachineState> {
    match &report.rewrite {
        Some(ReconcileRewrite::Stopped { reason }) => {
            let mut reconciled = state.clone();
            reconciled.status = MachineStatus::Stopped;
            reconciled.shim_pid = None;
            reconciled.vmm_pid = None;
            reconciled.sidecar_pids.clear();
            reconciled.started_at = None;
            reconciled.degraded.clear();
            reconciled.last_exit = Some(LastExit {
                at: reconciled_at.to_owned(),
                code: None,
                signal: None,
                reason: reason.clone(),
            });
            Some(reconciled)
        }
        None => None,
    }
}

/// Explicit-path access to a single machine state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> Result<MachineState, FirestoneError> {
        let bytes = fs::read(&self.path).map_err(|error| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot read machine state {}", self.path.display()),
            )
            .with_hint("check that the machine state file exists and is readable")
            .with_source(error)
        })?;
        let state: MachineState = serde_json::from_slice(&bytes).map_err(|error| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot parse machine state {}", self.path.display()),
            )
            .with_hint("inspect state.json for corruption or an unsupported version")
            .with_source(error)
        })?;
        state.validate()?;
        Ok(state)
    }

    /// Writes state while the current process is the machine shim.
    ///
    /// The shim must be the sole writer from the `starting` handoff until its
    /// final state write.
    pub fn write_from_shim(&self, state: &MachineState) -> Result<(), FirestoneError> {
        state.validate()?;
        atomic::write_json(&self.path, state)
    }

    /// Writes state from a CLI or REST action after proving it holds this
    /// machine directory's lock.
    ///
    /// The caller must first verify that no shim owns the state file.
    pub fn write_from_locked_action(
        &self,
        state: &MachineState,
        lock: &MachineLock,
    ) -> Result<(), FirestoneError> {
        let expected_lock = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("lock");
        if lock.path() != expected_lock {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "state {} requires machine lock {}",
                    self.path.display(),
                    expected_lock.display()
                ),
            )
            .with_hint("acquire the lock from the same machine directory"));
        }
        state.validate()?;
        atomic::write_json(&self.path, state)
    }

    /// Persists only the rewrite requested by a pure reconciliation decision.
    pub fn write_reconciliation(
        &self,
        state: &MachineState,
        report: &ReconcileReport,
        reconciled_at: &str,
        lock: &MachineLock,
    ) -> Result<Option<MachineState>, FirestoneError> {
        let Some(reconciled) = reconciled_state(state, report, reconciled_at) else {
            return Ok(None);
        };
        self.write_from_locked_action(&reconciled, lock)?;
        Ok(Some(reconciled))
    }
}

fn invalid_state(message: &'static str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("invalid machine state: {message}"),
    )
    .with_hint("do not write state until the internal invariant is restored")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use serde_json::json;

    use super::{
        ExitReason, LastExit, LivenessObservation, MachineState, MachineStatus, ReconcileRewrite,
        StateImage, StateStore, StateVersion, Supervision, VmmPingProbe, observe_liveness,
        reconcile, reconciled_state, verify_shim_identity,
    };
    use crate::{Event, FirestoneError, MachineLock};

    struct FixedPing(bool);

    impl VmmPingProbe for FixedPing {
        fn ping(&self, _api_socket: &Path) -> Result<bool, FirestoneError> {
            Ok(self.0)
        }
    }

    #[test]
    fn state_image_before_pull_serializes_null_identity() -> Result<(), Box<dyn std::error::Error>>
    {
        let image = StateImage {
            r#ref: "ubuntu:24.04".to_owned(),
            id: None,
            sha256: None,
        };

        assert_eq!(
            serde_json::to_value(image)?,
            json!({"ref": "ubuntu:24.04", "id": null, "sha256": null})
        );
        Ok(())
    }

    #[test]
    fn machine_state_partial_or_empty_image_identity_is_rejected() {
        for (id, sha256) in [
            (Some("id".to_owned()), None),
            (None, Some("sha".to_owned())),
            (Some(String::new()), Some("sha".to_owned())),
            (Some("id".to_owned()), Some(String::new())),
        ] {
            let mut state = populated_state();
            state.image.id = id;
            state.image.sha256 = sha256;
            assert!(state.validate().is_err());
        }
    }

    #[test]
    fn machine_state_serialization_models_complete_version_one_schema()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = populated_state();

        assert_eq!(
            serde_json::to_value(state)?,
            json!({
                "version": 1,
                "status": "running",
                "image": {
                    "ref": "ubuntu:24.04",
                    "id": "ubuntu-24.04-x86_64-1a2b3c4d",
                    "sha256": "abc123"
                },
                "mac": "52:54:00:9a:1f:c3",
                "cid": 3,
                "instance_id": "iid-ubuntu-5f3a9c1e2b7d",
                "shim_pid": 41200,
                "vmm_pid": 41207,
                "sidecar_pids": {"passt": 41203, "virtiofsd-0": 41205},
                "runtime_dir": "/run/user/1000/firestone/ubuntu",
                "started_at": "2026-08-28T09:12:44Z",
                "forwards": ["tcp:0.0.0.0:8080:80"],
                "degraded": [],
                "last_exit": {
                    "at": "2026-08-27T18:02:10Z",
                    "code": 0,
                    "signal": null,
                    "reason": "guest shutdown"
                }
            })
        );
        Ok(())
    }

    #[test]
    fn machine_state_unknown_version_is_rejected() {
        let value = json!({
            "version": 2,
            "status": "created",
            "image": {"ref": "ubuntu:24.04", "id": "id", "sha256": "sha"},
            "mac": null,
            "cid": 3,
            "instance_id": null,
            "shim_pid": null,
            "vmm_pid": null,
            "sidecar_pids": {},
            "runtime_dir": "/run/firestone/ubuntu",
            "started_at": null,
            "forwards": [],
            "degraded": [],
            "last_exit": null
        });

        let error = match serde_json::from_value::<MachineState>(value) {
            Err(error) => error,
            Ok(_) => panic!("state version two must be rejected"),
        };

        assert!(error.to_string().contains("unsupported state version 2"));
    }

    #[test]
    fn last_exit_code_and_signal_together_are_rejected() {
        let result = serde_json::from_value::<LastExit>(json!({
            "at": "2026-08-28T00:00:00Z",
            "code": 1,
            "signal": 9,
            "reason": "vmm process exited"
        }));
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("code and signal must be mutually exclusive"),
        };

        assert!(error.to_string().contains("both a code and a signal"));
    }

    #[test]
    fn exit_reason_failure_round_trip_preserves_diagnostic()
    -> Result<(), Box<dyn std::error::Error>> {
        let reason = ExitReason::Failure("cloud-hypervisor exited: disk error".to_owned());
        let encoded = serde_json::to_string(&reason)?;
        let decoded = serde_json::from_str(&encoded)?;

        assert_eq!(reason, decoded);
        Ok(())
    }

    #[test]
    fn shim_identity_exact_cmdline_is_verified() -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;
        let process = proc.path().join("42");
        fs::create_dir(&process)?;
        fs::write(
            process.join("cmdline"),
            b"/opt/firestone/bin/firestone\0_shim\0ubuntu\0",
        )?;

        assert!(verify_shim_identity(proc.path(), 42, "ubuntu")?);
        Ok(())
    }

    #[test]
    fn shim_identity_reused_pid_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;
        let process = proc.path().join("42");
        fs::create_dir(&process)?;

        for cmdline in [
            b"/opt/firestone/bin/firestone\0_shim\0debian\0".as_slice(),
            b"/usr/bin/other\0_shim\0ubuntu\0".as_slice(),
            b"/opt/firestone/bin/firestone\0_shim\0ubuntu\0extra\0".as_slice(),
        ] {
            fs::write(process.join("cmdline"), cmdline)?;
            assert!(!verify_shim_identity(proc.path(), 42, "ubuntu")?);
        }
        Ok(())
    }

    #[test]
    fn shim_identity_missing_process_is_not_verified() -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;

        assert!(!verify_shim_identity(proc.path(), 42, "ubuntu")?);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shim_identity_current_test_process_is_not_verified() -> Result<(), Box<dyn std::error::Error>>
    {
        assert!(!verify_shim_identity(
            Path::new("/proc"),
            std::process::id(),
            "ubuntu"
        )?);
        Ok(())
    }

    #[test]
    fn observe_liveness_uses_runtime_proc_and_ping_inputs() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let runtime = directory.path().join("runtime");
        let proc = directory.path().join("proc");
        fs::create_dir(&runtime)?;
        fs::create_dir(&proc)?;
        fs::create_dir(proc.join("41200"))?;
        fs::write(
            proc.join("41200/cmdline"),
            b"/usr/bin/firestone\0_shim\0ubuntu\0",
        )?;
        let state = populated_state();

        let observation = observe_liveness(
            "ubuntu",
            &state,
            &runtime,
            &runtime.join("api.sock"),
            &proc,
            &FixedPing(true),
        )?;

        assert_eq!(
            observation,
            LivenessObservation {
                vmm_ping: true,
                shim_verified: true,
                runtime_dir_exists: true,
            }
        );
        Ok(())
    }

    #[test]
    fn reconcile_full_matrix_reports_expected_state_and_rewrite() {
        let statuses = [
            MachineStatus::Created,
            MachineStatus::Starting,
            MachineStatus::Running,
            MachineStatus::Stopping,
            MachineStatus::Stopped,
            MachineStatus::Failed,
        ];
        let mut cases = 0;

        for stored in statuses {
            for vmm_ping in [false, true] {
                for shim_verified in [false, true] {
                    for runtime_dir_exists in [false, true] {
                        let observation = LivenessObservation {
                            vmm_ping,
                            shim_verified,
                            runtime_dir_exists,
                        };
                        let report = reconcile(stored, observation);
                        cases += 1;

                        if vmm_ping {
                            assert_eq!(report.status, MachineStatus::Running);
                            assert_eq!(
                                report.supervision,
                                Some(if shim_verified {
                                    Supervision::Supervised
                                } else {
                                    Supervision::Unsupervised
                                })
                            );
                            assert_eq!(report.rewrite, None);
                        } else if shim_verified {
                            let expected = match stored {
                                MachineStatus::Stopping => MachineStatus::Stopping,
                                MachineStatus::Failed => MachineStatus::Failed,
                                MachineStatus::Created
                                | MachineStatus::Starting
                                | MachineStatus::Running
                                | MachineStatus::Stopped => MachineStatus::Starting,
                            };
                            assert_eq!(report.status, expected);
                            assert_eq!(report.supervision, None);
                            assert_eq!(report.rewrite, None);
                        } else if stored.is_active() {
                            let reason = if runtime_dir_exists {
                                ExitReason::Stale
                            } else {
                                ExitReason::HostReboot
                            };
                            assert_eq!(report.status, MachineStatus::Stopped);
                            assert_eq!(report.rewrite, Some(ReconcileRewrite::Stopped { reason }));
                        } else {
                            assert_eq!(report.status, stored);
                            assert_eq!(report.supervision, None);
                            assert_eq!(report.rewrite, None);
                        }
                    }
                }
            }
        }

        assert_eq!(cases, 48);
    }

    #[test]
    fn reconcile_created_or_stopped_with_verified_shim_reports_starting_without_rewrite() {
        for stored in [MachineStatus::Created, MachineStatus::Stopped] {
            let report = reconcile(
                stored,
                LivenessObservation {
                    vmm_ping: false,
                    shim_verified: true,
                    runtime_dir_exists: true,
                },
            );

            assert_eq!(report.status, MachineStatus::Starting);
            assert_eq!(report.supervision, None);
            assert_eq!(report.rewrite, None);
        }
    }

    #[test]
    fn reconcile_failed_with_verified_shim_preserves_failed_without_rewrite() {
        let report = reconcile(
            MachineStatus::Failed,
            LivenessObservation {
                vmm_ping: false,
                shim_verified: true,
                runtime_dir_exists: true,
            },
        );

        assert_eq!(report.status, MachineStatus::Failed);
        assert_eq!(report.supervision, None);
        assert_eq!(report.rewrite, None);
    }

    #[test]
    fn reconciled_state_stale_active_state_clears_process_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = populated_state();
        state.degraded = vec!["passt exited (code 1)".to_owned()];
        let report = reconcile(
            state.status,
            LivenessObservation {
                vmm_ping: false,
                shim_verified: false,
                runtime_dir_exists: true,
            },
        );

        let reconciled = reconciled_state(&state, &report, "2026-08-28T10:00:00Z")
            .ok_or("active stale state did not request a rewrite")?;

        assert_eq!(reconciled.status, MachineStatus::Stopped);
        assert_eq!(reconciled.shim_pid, None);
        assert_eq!(reconciled.vmm_pid, None);
        assert!(reconciled.sidecar_pids.is_empty());
        assert_eq!(reconciled.started_at, None);
        assert!(reconciled.degraded.is_empty());
        assert_eq!(
            reconciled.last_exit,
            Some(LastExit {
                at: "2026-08-28T10:00:00Z".to_owned(),
                code: None,
                signal: None,
                reason: ExitReason::Stale,
            })
        );
        Ok(())
    }

    #[test]
    fn state_store_zero_process_pid_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let state_path = directory.path().join("state.json");
        let store = StateStore::new(&state_path);

        for (field, value) in [
            ("shim_pid", json!(0)),
            ("vmm_pid", json!(0)),
            ("sidecar_pids", json!({"passt": 0})),
        ] {
            let mut persisted = serde_json::to_value(populated_state())?;
            persisted[field] = value;
            fs::write(&state_path, serde_json::to_vec_pretty(&persisted)?)?;

            let error = match store.read() {
                Err(error) => error,
                Ok(_) => panic!("zero {field} must be rejected"),
            };
            assert_eq!(error.kind(), crate::ErrorKind::Generic);
            assert!(error.message().contains("process ids"));
        }
        Ok(())
    }

    #[test]
    fn state_store_locked_action_requires_matching_machine_lock()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let machine = directory.path().join("ubuntu");
        let other = directory.path().join("other");
        fs::create_dir(&machine)?;
        fs::create_dir(&other)?;
        let store = StateStore::new(machine.join("state.json"));
        let mut events: Vec<Event> = Vec::new();
        let wrong_lock = MachineLock::acquire("other", &other.join("lock"), &mut events)?;

        let error = match store.write_from_locked_action(&populated_state(), &wrong_lock) {
            Err(error) => error,
            Ok(()) => panic!("a lock from another machine must be rejected"),
        };

        assert_eq!(error.kind(), crate::ErrorKind::Conflict);
        assert!(!store.path().exists());
        Ok(())
    }

    #[test]
    fn state_store_reconciliation_with_matching_lock_writes_atomically()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let machine = directory.path().join("ubuntu");
        fs::create_dir(&machine)?;
        let store = StateStore::new(machine.join("state.json"));
        let mut events: Vec<Event> = Vec::new();
        let lock = MachineLock::acquire("ubuntu", &machine.join("lock"), &mut events)?;
        let state = populated_state();
        store.write_from_locked_action(&state, &lock)?;
        let report = reconcile(
            state.status,
            LivenessObservation {
                vmm_ping: false,
                shim_verified: false,
                runtime_dir_exists: false,
            },
        );

        let written = store
            .write_reconciliation(&state, &report, "2026-08-28T10:00:00Z", &lock)?
            .ok_or("host reboot did not request a rewrite")?;

        assert_eq!(written.status, MachineStatus::Stopped);
        assert_eq!(written.started_at, None);
        assert!(written.degraded.is_empty());
        assert_eq!(store.read()?, written);
        assert!(!machine.join("state.json.tmp").exists());
        Ok(())
    }

    fn populated_state() -> MachineState {
        MachineState {
            version: StateVersion,
            status: MachineStatus::Running,
            image: StateImage {
                r#ref: "ubuntu:24.04".to_owned(),
                id: Some("ubuntu-24.04-x86_64-1a2b3c4d".to_owned()),
                sha256: Some("abc123".to_owned()),
            },
            mac: Some("52:54:00:9a:1f:c3".to_owned()),
            cid: 3,
            instance_id: Some("iid-ubuntu-5f3a9c1e2b7d".to_owned()),
            shim_pid: Some(41200),
            vmm_pid: Some(41207),
            sidecar_pids: BTreeMap::from([
                ("passt".to_owned(), 41203),
                ("virtiofsd-0".to_owned(), 41205),
            ]),
            runtime_dir: "/run/user/1000/firestone/ubuntu".into(),
            started_at: Some("2026-08-28T09:12:44Z".to_owned()),
            forwards: vec!["tcp:0.0.0.0:8080:80".to_owned()],
            degraded: Vec::new(),
            last_exit: Some(LastExit {
                at: "2026-08-27T18:02:10Z".to_owned(),
                code: Some(0),
                signal: None,
                reason: ExitReason::GuestShutdown,
            }),
        }
    }
}
