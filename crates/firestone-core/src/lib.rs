//! Shared contracts and execution boundaries for Firestone.

pub mod action;
pub mod atomic;
pub mod catalog;
pub mod cmd;
pub mod deps;
pub mod dispatcher;
pub mod doctor;
pub mod error;
pub mod event;
pub mod lock;
pub mod paths;
pub mod result;
pub mod spec;
pub mod state;

pub use action::Action;
pub use catalog::{
    Catalog, CatalogArchSource, CatalogChecksum, CatalogEntry, CatalogFirmware, ChecksumAlgorithm,
    ImageFormat, ResolvedCatalogEntry,
};
pub use cmd::{Cmd, CmdOutput};
pub use deps::{DependencyArtifact, DependencyManifest};
pub use dispatcher::{DispatchFuture, Dispatcher, EventSink};
pub use doctor::{
    DoctorCheck, DoctorCheckId, DoctorContext, DoctorReport, DoctorStatus,
    read_reconciled_machine_state_live, read_reconciled_machine_state_live_locked, run_doctor,
};
pub use error::{ErrorInfo, ErrorKind, FirestoneError};
pub use event::{Event, Level, StepId, Unit};
pub use lock::MachineLock;
pub use paths::{PathInputs, Paths};
pub use result::{MachineRecord, MachineSummary, MachineView, SpecResult, SpecWarningPayload};
pub use spec::{
    Arch, ByteSize, CloudInitSpec, CloudInitSpecPatch, ColorMode, Firmware, GlobalConfig,
    HumanDuration, ImageRef, ImagesConfig, LoadedMachineSpec, MacAddr, MachineSpec,
    MachineSpecPatch, MountSpec, NetMode, NetworkSpec, NetworkSpecPatch, ParseByteSizeError,
    ParseDurationError, ParseFirmwareError, ParseMacAddrError, ParsePortForwardError,
    ParseSpecClearError, PatchMerge, PortForward, PortRange, Protocol, RealValidationHost,
    SPEC_FIELD_METADATA, SpecClear, SpecFieldMetadata, SpecWarning, StartConfig, StopConfig,
    UiConfig, ValidationContext, ValidationHost, VmmSpec, VmmSpecPatch, validate_machine_spec,
};
pub use state::{
    ExitReason, LastExit, LiveMachineState, LivenessObservation, MachineState, MachineStatus,
    ReconcileReport, ReconcileRewrite, StateImage, StateStore, StateVersion, Supervision,
    VmmPingProbe, observe_liveness, reconcile, reconciled_state, verify_shim_identity,
};
