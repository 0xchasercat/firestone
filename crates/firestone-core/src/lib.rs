//! Shared contracts and execution boundaries for Firestone.

pub mod action;
pub mod atomic;
pub mod dispatcher;
pub mod error;
pub mod event;
pub mod lock;
pub mod state;

pub use action::Action;
pub use dispatcher::{DispatchFuture, Dispatcher, EventSink};
pub use error::{ErrorInfo, ErrorKind, FirestoneError};
pub use event::{Event, Level, StepId, Unit};
pub use lock::MachineLock;
pub use state::{
    ExitReason, LastExit, LivenessObservation, MachineState, MachineStatus, ReconcileReport,
    ReconcileRewrite, StateImage, StateStore, StateVersion, Supervision, VmmPingProbe,
    observe_liveness, reconcile, reconciled_state, verify_shim_identity,
};
