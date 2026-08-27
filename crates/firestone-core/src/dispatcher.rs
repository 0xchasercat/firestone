use std::{future::Future, pin::Pin};

use crate::{Action, Event, FirestoneError};

/// Future returned by an action dispatcher.
pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = Result<(), FirestoneError>> + Send + 'a>>;

/// Receives the event stream for one action.
pub trait EventSink: Send {
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError>;
}

impl<F> EventSink for F
where
    F: FnMut(Event) -> Result<(), FirestoneError> + Send,
{
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
        self(event)
    }
}

impl EventSink for Vec<Event> {
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
        self.push(event);
        Ok(())
    }
}

/// Runs actions for the CLI and REST adapters.
pub trait Dispatcher: Send + Sync {
    fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a>;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DispatchFuture, Dispatcher, EventSink};
    use crate::{Action, Event};

    struct VersionDispatcher;

    impl Dispatcher for VersionDispatcher {
        fn run<'a>(&'a self, action: Action, events: &'a mut dyn EventSink) -> DispatchFuture<'a> {
            Box::pin(async move {
                if action == Action::Version {
                    events.emit(Event::Result {
                        action: "version".to_owned(),
                        payload: json!({"version": "0.1.0"}),
                    })?;
                }
                Ok(())
            })
        }
    }

    #[test]
    fn dispatcher_and_event_sink_are_object_safe() {
        fn accepts_dispatcher(_: &dyn Dispatcher) {}
        fn accepts_sink(_: &mut dyn EventSink) {}

        let dispatcher = VersionDispatcher;
        let mut events = Vec::new();

        accepts_dispatcher(&dispatcher);
        accepts_sink(&mut events);
    }
}
