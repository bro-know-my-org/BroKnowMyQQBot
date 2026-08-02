//! Adapter boundary implemented by platform integrations.

use async_trait::async_trait;
use std::time::Instant;
use thiserror::Error;

use tokio::sync::mpsc;

use crate::{Action, ActionResult, AdapterId, EventEnvelope, RuntimeObserver, ShutdownSignal};

pub(crate) const MAX_ADAPTER_ID_BYTES: usize = 128;

pub(crate) fn is_valid_adapter_id(adapter_id: &AdapterId) -> bool {
    let value = adapter_id.as_str();
    !value.is_empty() && value.len() <= MAX_ADAPTER_ID_BYTES && !value.chars().any(char::is_control)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "Adapter ID must be non-empty, at most {MAX_ADAPTER_ID_BYTES} bytes, and contain no control characters"
)]
pub struct EventSenderBuildError;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EventSendError {
    #[error("event belongs to Adapter `{actual}`, but sender is bound to `{expected}`")]
    AdapterMismatch {
        expected: AdapterId,
        actual: AdapterId,
    },
    #[error("runtime event queue is full")]
    QueueFull,
    #[error("runtime event queue is closed")]
    QueueClosed,
}

/// Adapter-facing sender that observes queue backpressure and rejected events.
#[derive(Debug, Clone)]
pub struct EventSender {
    inner: mpsc::Sender<EventEnvelope>,
    adapter_id: AdapterId,
    observer: RuntimeObserver,
}

/// Reserved capacity in the runtime event queue.
pub struct EventPermit<'a> {
    permit: mpsc::Permit<'a, EventEnvelope>,
    sender: &'a EventSender,
    started: Instant,
}

impl std::fmt::Debug for EventPermit<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventPermit")
            .field("adapter_id", &self.sender.adapter_id)
            .finish_non_exhaustive()
    }
}

impl EventPermit<'_> {
    pub fn send(self, event: EventEnvelope) -> Result<(), EventSendError> {
        if event.adapter != self.sender.adapter_id {
            self.sender
                .observer
                .record_event_rejected(&self.sender.adapter_id);
            return Err(EventSendError::AdapterMismatch {
                expected: self.sender.adapter_id.clone(),
                actual: event.adapter,
            });
        }
        self.permit.send(event);
        self.sender.observer.record_event_enqueued(
            &self.sender.adapter_id,
            self.started.elapsed(),
            self.sender.queue_depth(),
        );
        Ok(())
    }
}

impl EventSender {
    /// Wraps a bounded runtime queue for one Adapter instance.
    pub fn new(
        inner: mpsc::Sender<EventEnvelope>,
        adapter_id: AdapterId,
        observer: RuntimeObserver,
    ) -> Result<Self, EventSenderBuildError> {
        if !is_valid_adapter_id(&adapter_id) {
            return Err(EventSenderBuildError);
        }
        Ok(Self {
            inner,
            adapter_id,
            observer,
        })
    }

    pub(crate) fn new_unchecked(
        inner: mpsc::Sender<EventEnvelope>,
        adapter_id: AdapterId,
        observer: RuntimeObserver,
    ) -> Self {
        Self {
            inner,
            adapter_id,
            observer,
        }
    }

    /// Waits for queue capacity before taking ownership of an event.
    ///
    /// Use this before moving a source event into a cancellation-sensitive
    /// `select!`; cancelling the reservation wait leaves the event with the
    /// caller.
    pub async fn reserve(&self) -> Result<EventPermit<'_>, EventSendError> {
        let started = Instant::now();
        let permit = self.inner.reserve().await.map_err(|_| {
            self.observer.record_event_rejected(&self.adapter_id);
            EventSendError::QueueClosed
        })?;
        Ok(EventPermit {
            permit,
            sender: self,
            started,
        })
    }

    pub async fn send(&self, event: EventEnvelope) -> Result<(), EventSendError> {
        if event.adapter != self.adapter_id {
            self.observer.record_event_rejected(&self.adapter_id);
            return Err(EventSendError::AdapterMismatch {
                expected: self.adapter_id.clone(),
                actual: event.adapter,
            });
        }
        self.reserve().await?.send(event)
    }

    /// Marks the bound Adapter as ready to receive or produce runtime traffic.
    pub fn mark_ready(&self) {
        self.observer.set_adapter_online(&self.adapter_id, true);
    }

    /// Marks the bound Adapter as temporarily unavailable without ending its task.
    pub fn mark_not_ready(&self) {
        self.observer.set_adapter_online(&self.adapter_id, false);
    }

    pub fn try_send(&self, event: EventEnvelope) -> Result<(), EventSendError> {
        if event.adapter != self.adapter_id {
            self.observer.record_event_rejected(&self.adapter_id);
            return Err(EventSendError::AdapterMismatch {
                expected: self.adapter_id.clone(),
                actual: event.adapter,
            });
        }
        let started = Instant::now();
        match self.inner.try_send(event) {
            Ok(()) => {
                self.observer.record_event_enqueued(
                    &self.adapter_id,
                    started.elapsed(),
                    self.queue_depth(),
                );
                Ok(())
            }
            Err(error) => {
                self.observer.record_event_rejected(&self.adapter_id);
                Err(match error {
                    mpsc::error::TrySendError::Full(_) => EventSendError::QueueFull,
                    mpsc::error::TrySendError::Closed(_) => EventSendError::QueueClosed,
                })
            }
        }
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    fn queue_depth(&self) -> usize {
        self.inner.max_capacity().saturating_sub(self.capacity())
    }
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("adapter configuration error: {0}")]
    Configuration(String),
    #[error("adapter transport error: {0}")]
    Transport(String),
    #[error("adapter action error: {0}")]
    Action(String),
    #[error("adapter action result is unknown: {0}")]
    ActionUnknown(String),
    #[error("runtime event queue is closed")]
    EventQueueClosed,
    #[error("event Adapter mismatch: expected `{expected}`, received `{actual}`")]
    EventAdapterMismatch {
        expected: AdapterId,
        actual: AdapterId,
    },
}

#[async_trait]
pub trait Adapter: Send + Sync + 'static {
    fn id(&self) -> &AdapterId;

    /// Stable platform namespace used for platform-specific event extensions.
    ///
    /// Adapter IDs identify configured instances and may be user-defined, while
    /// this value identifies the protocol family. Implementations should
    /// override it when they expose platform-specific data.
    fn platform(&self) -> &'static str;

    /// Runs the Adapter until shutdown or a terminal failure.
    ///
    /// Implementations must call [`EventSender::mark_ready`] only after their
    /// listener or authenticated session is usable, and call
    /// [`EventSender::mark_not_ready`] during reconnect backoff or another
    /// temporary outage.
    async fn run(&self, events: EventSender, shutdown: ShutdownSignal) -> Result<(), AdapterError>;

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError>;

    /// Acknowledges a successfully committed or duplicate event at its source.
    /// Implementations must make repeated acknowledgements idempotent.
    async fn event_handled(&self, _event: &EventEnvelope) -> Result<(), AdapterError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::{Future as _, poll_fn},
        task::Poll,
        time::Duration,
    };

    use serde_json::Value;
    use tokio::time::timeout;

    use super::{EventSendError, EventSender};
    use crate::{AdapterId, Event, EventEnvelope, EventId, RuntimeObserver};

    fn event(id: &str) -> EventEnvelope {
        event_for(id, "observed")
    }

    fn event_for(id: &str, adapter: &str) -> EventEnvelope {
        EventEnvelope {
            id: EventId::new(id),
            adapter: AdapterId::new(adapter),
            delivery_id: None,
            timestamp: None,
            event: Event::Lifecycle(Value::Null),
            raw: Value::Null,
        }
    }

    #[tokio::test]
    async fn sender_observes_backpressure_and_rejections() {
        let observer = RuntimeObserver::new();
        let (raw_sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let sender =
            EventSender::new(raw_sender, AdapterId::new("observed"), observer.clone()).unwrap();
        sender.try_send(event("first")).unwrap();
        assert_eq!(
            sender.try_send(event("rejected")).unwrap_err(),
            EventSendError::QueueFull
        );

        let mut waiting = Box::pin(sender.send(event("second")));
        poll_fn(|context| match waiting.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("send must wait while the bounded queue is full"),
        })
        .await;
        assert_eq!(receiver.recv().await.unwrap().id.as_str(), "first");
        timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();

        let snapshot = observer.snapshot();
        assert_eq!(snapshot.events_received, 2);
        assert_eq!(snapshot.rejected_events, 1);
        let adapter = &snapshot.adapters["observed"];
        assert_eq!(adapter.events_received, 2);
        assert_eq!(adapter.events_rejected, 1);
    }

    #[test]
    fn sender_classifies_mismatch_and_closed_queue_rejections() {
        let mismatch_observer = RuntimeObserver::new();
        let (raw_sender, _receiver) = tokio::sync::mpsc::channel(1);
        let sender = EventSender::new(
            raw_sender,
            AdapterId::new("expected"),
            mismatch_observer.clone(),
        )
        .unwrap();
        assert_eq!(
            sender
                .try_send(event_for("mismatch", "actual"))
                .unwrap_err(),
            EventSendError::AdapterMismatch {
                expected: AdapterId::new("expected"),
                actual: AdapterId::new("actual"),
            }
        );
        assert_eq!(mismatch_observer.snapshot().rejected_events, 1);

        let closed_observer = RuntimeObserver::new();
        let (raw_sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let sender = EventSender::new(
            raw_sender,
            AdapterId::new("observed"),
            closed_observer.clone(),
        )
        .unwrap();
        assert_eq!(
            sender.try_send(event("closed")).unwrap_err(),
            EventSendError::QueueClosed
        );
        assert_eq!(closed_observer.snapshot().rejected_events, 1);
    }

    #[test]
    fn sender_controls_adapter_readiness() {
        let observer = RuntimeObserver::new();
        let (raw_sender, _receiver) = tokio::sync::mpsc::channel(1);
        let sender =
            EventSender::new(raw_sender, AdapterId::new("observed"), observer.clone()).unwrap();

        sender.mark_ready();
        assert!(observer.snapshot().adapters["observed"].online);
        sender.mark_not_ready();
        assert!(!observer.snapshot().adapters["observed"].online);
    }

    #[test]
    fn sender_rejects_adapter_ids_that_cannot_be_used_as_metric_labels() {
        for invalid in [
            "",
            "invalid\nid",
            &"x".repeat(super::MAX_ADAPTER_ID_BYTES + 1),
        ] {
            let (raw_sender, _receiver) = tokio::sync::mpsc::channel(1);
            assert!(
                EventSender::new(raw_sender, AdapterId::new(invalid), RuntimeObserver::new(),)
                    .is_err()
            );
        }
    }
}
