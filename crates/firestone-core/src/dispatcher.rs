use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

use crate::{Action, Event, FirestoneError};

/// Future returned by an action dispatcher.
pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = Result<(), FirestoneError>> + Send + 'a>>;

struct ThreadWake(thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Polls one future to completion without entering an asynchronous runtime.
///
/// Dispatchers may call blocking transports, so CLI and REST worker threads
/// use this executor instead of nesting them inside Tokio.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

/// Receives the event stream for one action.
pub trait EventSink: Send {
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError>;

    /// Reports that the transport no longer has a consumer.
    ///
    /// Mutating actions deliberately ignore this signal and continue to a safe
    /// point. Open-ended read actions such as log follow may stop without
    /// manufacturing an action failure.
    fn is_cancelled(&self) -> bool {
        false
    }
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
