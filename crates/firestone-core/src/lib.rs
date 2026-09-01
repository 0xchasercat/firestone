//! Shared contracts and execution boundaries for Firestone.

pub mod action;
pub mod atomic;
mod bounded;
pub mod catalog;
pub mod cloudinit;
pub mod cmd;
pub mod console;
pub mod deps;
pub mod dispatcher;
pub mod doctor;
pub mod embedded_helpers;
pub mod error;
pub mod event;
pub mod image;
pub mod lock;
pub mod metrics;
pub mod network;
pub mod oci;
pub mod paths;
pub mod pty;
pub mod readiness;
pub mod result;
pub mod shim;
pub mod spec;
pub mod ssh;
pub mod state;
pub mod virtiofs;
pub mod vmm;
pub mod vmm_api;

pub use action::{Action, LogSource, ParseLogSourceError};
pub use catalog::{
    Catalog, CatalogArchSource, CatalogChecksum, CatalogEntry, CatalogFirmware, ChecksumAlgorithm,
    ImageFormat, ResolvedCatalogEntry, SshdPath,
};
pub use cloudinit::{
    RenderedCloudInit, SEED_IMAGE_SIZE, publish_seed, publish_seed_with_sshd_path,
    render_cloud_init, render_cloud_init_with_guest_ssh,
};
pub use cmd::{Cmd, CmdOutput, ManagedProcess, ProcessSignal, signal_process_group};
pub use console::{
    CONSOLE_ACK_MAX_BYTES, ConsoleAck, ConsoleBroker, ConsolePlan, ConsoleResult, RawTerminal,
    console_plan, relay_console,
};
pub use deps::{
    DIRECT_BOOT_KERNEL_DEPENDENCY, DependencyArtifact, DependencyManifest,
    PINNED_DIRECT_BOOT_KERNEL_VERSION,
};
pub use dispatcher::{DispatchFuture, Dispatcher, EventSink, block_on};
pub use doctor::{
    APPARMOR_PASST_EXECUTABLE, APPARMOR_PASST_PROFILE, APPARMOR_PASST_PROFILE_NAME, DoctorCheck,
    DoctorCheckId, DoctorContext, DoctorOptions, DoctorReport, DoctorStatus, ExtractedPasstHelper,
    VerifiedPasst, read_reconciled_machine_state_live, read_reconciled_machine_state_live_locked,
    resolve_verified_apparmor_passt, run_doctor,
};
pub use embedded_helpers::{
    EmbeddedHelper, InternalHelper, embedded_helper, materialize_embedded_helper,
};
pub use error::{ErrorInfo, ErrorKind, FirestoneError};
pub use event::{Event, Level, StepId, Unit};
pub use image::{
    ImageInspection, ImageKind, ImageMetadata, ImageMetadataVersion, ImagePruneResult,
    ImagePullRequest, ImageRemoveResult, ImageSourceLocation, ImageStore, ImageVerification,
    OverlayInfo, PreparedMachineImage, PulledImage, ResolvedImageSource, StoredImage,
    disk_shrink_error, overlay_virtual_size,
};
pub use lock::MachineLock;
pub use metrics::{
    COUNTER_SENTINEL_FLOOR, MetricsBlockDevice, MetricsCpu, MetricsMemory, MetricsNetDevice,
    MetricsResult, VmmProcessSample, counter_is_sentinel, cpu_ticks_to_nanoseconds,
    parse_proc_stat_cpu_ticks, parse_proc_status_rss_bytes, project_device_counters,
    sample_vmm_process,
};
pub use network::{
    DEFAULT_NETWORK_READINESS_POLL_INTERVAL, DEFAULT_NETWORK_READINESS_TIMEOUT, NetworkPlan,
    NetworkPlanOptions, OwnedPathExpectation, PINNED_PASST_VERSION, PasstPlan,
    ReadinessCancellation, SocketReadiness, SocketReadinessPlan, TapHost, TapOwnership, TapPlan,
    forwards_differ, passt_forward_argument, prepare_network, validate_tap,
};
pub use oci::{OciClassification, OciReference, OciTagOrDigest};
pub use paths::{PathInputs, Paths};
pub use pty::{PtyPair, set_window_size};
pub use readiness::{ReadinessOptions, wait_for_ssh_ready};
pub use result::CloneResult;
pub use result::{
    CatalogArchitectureSummary, CatalogEntrySummary, CpResult, LogsResult, MachineRecord,
    MachineSummary, MachineView, RemoveResult, ResizeResult, RunResult, ShellResult, SpecResult,
    SpecWarningPayload, SshConfigResult, StartResult, StopResult, VersionDependency,
    VersionIdentity, VersionPaths, VersionResult,
};
pub use shim::{
    PreparedStart, ShimClient, ShimPids, ShimStatus, ShimTimeouts, cancel_prepared,
    launch_prepared, launch_prepared_cancellable, prepare_start, recover_shim, run_shim,
    stop_unsupervised,
};
pub use spec::{
    Arch, ByteSize, CloudInitSpec, CloudInitSpecPatch, ColorMode, Firmware, GlobalConfig,
    HumanDuration, ImageRef, ImagesConfig, LoadedMachineSpec, MacAddr, MachineSpec,
    MachineSpecPatch, MountSpec, NetMode, NetworkSpec, NetworkSpecPatch, ParseByteSizeError,
    ParseDurationError, ParseFirmwareError, ParseMacAddrError, ParsePortForwardError,
    ParseSpecClearError, PatchMerge, PortForward, PortRange, Protocol, RealValidationHost,
    SPEC_FIELD_METADATA, SpecClear, SpecFieldMetadata, SpecWarning, StartConfig, StopConfig,
    UiConfig, ValidationContext, ValidationHost, VmmSpec, VmmSpecPatch, validate_guest_user,
    validate_machine_spec,
};
pub use ssh::{
    CpOperand, CpOperands, SshCommandPlan, SshConfigPlan, SshIdentity, VSOCK_HANDSHAKE_MAX_BYTES,
    VSOCK_HANDSHAKE_TIMEOUT, VsockConnection, VsockPort, classify_cp_operand, classify_cp_operands,
    connect_vsock, ensure_ssh_identity, invalidate_known_hosts_for_seed, machine_known_hosts_path,
    readiness_ssh_plan, run_vsock_proxy, scp_command_plan, shell_ssh_plan, ssh_config_plan,
};
pub use state::{
    ExitReason, LastExit, LiveMachineState, LivenessObservation, MAX_MACHINE_STATE_BYTES,
    MachineState, MachineStatus, ReconcileReport, ReconcileRewrite, StateImage, StateStore,
    StateVersion, Supervision, VmmPingProbe, observe_liveness, reconcile, reconciled_state,
    verify_shim_identity,
};
pub use virtiofs::{
    DEFAULT_VIRTIOFS_READINESS_POLL_INTERVAL, DEFAULT_VIRTIOFS_READINESS_TIMEOUT,
    MAX_VIRTIOFS_MOUNTS, VHOST_USER_SOCKET_MAX_BYTES, VIRTIOFS_NUM_QUEUES, VIRTIOFS_PATH_MAX_BYTES,
    VIRTIOFS_QUEUE_SIZE, VIRTIOFS_TAG_MAX_BYTES, VirtiofsCancellationPolicy, VirtiofsPlan,
    VirtiofsReadinessPlan, VirtiofsSandbox, prepare_virtiofs_plans,
    prepare_virtiofs_plans_with_readiness,
};
pub use vmm::{
    CanonicalVmConfig, FsConfig, NetConfig, VmConfigInput, canonical_vm_config, publish_vm_config,
};
pub use vmm_api::{VmInfo, VmState, VmmApi, VmmApiLivenessProbe, VmmPingResponse};
