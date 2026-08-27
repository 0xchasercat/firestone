//! Shared contracts and execution boundaries for Firestone.

pub mod action;
pub mod dispatcher;
pub mod error;
pub mod event;

pub use action::Action;
pub use dispatcher::{DispatchFuture, Dispatcher, EventSink};
pub use error::{ErrorInfo, ErrorKind, FirestoneError};
pub use event::{Event, Level, StepId, Unit};
