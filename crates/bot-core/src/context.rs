//! Per-event context and action routing.

use std::{fmt, sync::Arc};

use serde_json::Value;
use thiserror::Error;

use crate::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, Event, EventEnvelope, EventId,
    MessageTarget, ReplyAction,
};

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("event does not provide a reply target")]
    MissingReplyTarget,
    #[error(transparent)]
    Adapter(#[from] AdapterError),
}

#[derive(Clone)]
pub struct Context {
    adapter_id: AdapterId,
    platform: String,
    event_id: EventId,
    occurred_at_ms: Option<i64>,
    raw_event: Arc<Value>,
    source_message_id: Option<String>,
    reply_target: Option<MessageTarget>,
    adapter: Arc<dyn Adapter>,
}

impl fmt::Debug for Context {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Context")
            .field("adapter_id", &self.adapter_id)
            .field("platform", &self.platform)
            .field("event_id", &self.event_id)
            .field("occurred_at_ms", &self.occurred_at_ms)
            .field("source_message_id", &self.source_message_id)
            .field("reply_target", &self.reply_target)
            .finish_non_exhaustive()
    }
}

impl Context {
    pub(crate) fn new(envelope: &EventEnvelope, adapter: Arc<dyn Adapter>) -> Self {
        let (source_message_id, reply_target) = match &envelope.event {
            Event::Message(message) => (
                Some(message.message_id.clone()),
                Some(message.target.clone()),
            ),
            _ => (None, None),
        };
        Self {
            adapter_id: envelope.adapter.clone(),
            platform: adapter.platform().to_owned(),
            event_id: envelope.id.clone(),
            occurred_at_ms: envelope
                .timestamp
                .map(|timestamp| timestamp.timestamp_millis()),
            raw_event: Arc::new(envelope.raw.clone()),
            source_message_id,
            reply_target,
            adapter,
        }
    }

    pub const fn adapter_id(&self) -> &AdapterId {
        &self.adapter_id
    }

    pub const fn event_id(&self) -> &EventId {
        &self.event_id
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub const fn occurred_at_ms(&self) -> Option<i64> {
        self.occurred_at_ms
    }

    pub fn raw_event(&self) -> &Value {
        &self.raw_event
    }

    pub fn reply_target(&self) -> Option<&MessageTarget> {
        self.reply_target.as_ref()
    }

    pub fn source_message_id(&self) -> Option<&str> {
        self.source_message_id.as_deref()
    }

    pub async fn reply(&self, content: impl Into<String>) -> Result<ActionResult, ContextError> {
        let target = self
            .reply_target
            .clone()
            .ok_or(ContextError::MissingReplyTarget)?;
        let source_message_id = self
            .source_message_id
            .clone()
            .ok_or(ContextError::MissingReplyTarget)?;
        self.execute(Action::Reply(ReplyAction {
            target,
            source_message_id,
            content: content.into(),
        }))
        .await
    }

    pub async fn execute(&self, action: Action) -> Result<ActionResult, ContextError> {
        self.adapter
            .execute(action)
            .await
            .map_err(ContextError::from)
    }
}
