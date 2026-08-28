//! Shared contracts and execution boundaries for Firestone.

pub mod action;
pub mod atomic;
mod bounded;
pub mod catalog;
pub mod cloudinit;
pub mod cmd;
pub mod deps;
pub mod dispatcher;
pub mod doctor;
pub mod error;
pub mod event;
pub mod image;
pub mod lock;
pub mod paths;
pub mod result;
pub mod shim;
pub mod spec;
pub mod ssh;
pub mod state;
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
pub use cmd::{Cmd, CmdOutput, ManagedProcess, ProcessSignal};
pub use deps::{DependencyArtifact, DependencyManifest};
pub use dispatcher::{DispatchFuture, Dispatcher, EventSink};
pub use doctor::{
    DoctorCheck, DoctorCheckId, DoctorContext, DoctorReport, DoctorStatus,
    read_reconciled_machine_state_live, read_reconciled_machine_state_live_locked, run_doctor,
};
pub use error::{ErrorInfo, ErrorKind, FirestoneError};
pub use event::{Event, Level, StepId, Unit};
pub use image::{
    ImageInspection, ImageMetadata, ImageMetadataVersion, ImagePruneResult, ImagePullRequest,
    ImageRemoveResult, ImageSourceLocation, ImageStore, ImageVerification, OverlayInfo,
    PreparedMachineImage, PulledImage, ResolvedImageSource, StoredImage,
};
pub use lock::MachineLock;
pub use paths::{PathInputs, Paths};
pub use result::{
    LogsResult, MachineRecord, MachineSummary, MachineView, RemoveResult, SpecResult,
    SpecWarningPayload, StartResult, StopResult,
};
pub use shim::{
    PreparedStart, ShimClient, ShimPids, ShimStatus, ShimTimeouts, launch_prepared, prepare_start,
    recover_shim, run_shim, stop_unsupervised, validate_m1_start_scope,
};
pub use spec::{
    Arch, ByteSize, CloudInitSpec, CloudInitSpecPatch, ColorMode, Firmware, GlobalConfig,
    HumanDuration, ImageRef, ImagesConfig, LoadedMachineSpec, MacAddr, MachineSpec,
    MachineSpecPatch, MountSpec, NetMode, NetworkSpec, NetworkSpecPatch, ParseByteSizeError,
    ParseDurationError, ParseFirmwareError, ParseMacAddrError, ParsePortForwardError,
    ParseSpecClearError, PatchMerge, PortForward, PortRange, Protocol, RealValidationHost,
    SPEC_FIELD_METADATA, SpecClear, SpecFieldMetadata, SpecWarning, StartConfig, StopConfig,
    UiConfig, ValidationContext, ValidationHost, VmmSpec, VmmSpecPatch, validate_machine_spec,
};
pub use ssh::{
    SshIdentity, VSOCK_HANDSHAKE_MAX_BYTES, VSOCK_HANDSHAKE_TIMEOUT, VsockConnection, VsockPort,
    connect_vsock, ensure_ssh_identity, invalidate_known_hosts_for_seed, machine_known_hosts_path,
    run_vsock_proxy,
};
pub use state::{
    ExitReason, LastExit, LiveMachineState, LivenessObservation, MAX_MACHINE_STATE_BYTES,
    MachineState, MachineStatus, ReconcileReport, ReconcileRewrite, StateImage, StateStore,
    StateVersion, Supervision, VmmPingProbe, observe_liveness, reconcile, reconciled_state,
    verify_shim_identity,
};
pub use vmm::{CanonicalVmConfig, VmConfigInput, canonical_vm_config, publish_vm_config};
pub use vmm_api::{VmInfo, VmState, VmmApi, VmmApiLivenessProbe, VmmPingResponse};
