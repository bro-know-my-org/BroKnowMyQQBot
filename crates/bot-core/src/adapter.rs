//! Adapter boundary implemented by platform integrations.

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{Action, ActionResult, AdapterId, EventEnvelope, ShutdownSignal};

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

    async fn run(
        &self,
        events: mpsc::Sender<EventEnvelope>,
        shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError>;

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError>;

    async fn event_handled(&self, _event: &EventEnvelope) -> Result<(), AdapterError> {
        Ok(())
    }
}
