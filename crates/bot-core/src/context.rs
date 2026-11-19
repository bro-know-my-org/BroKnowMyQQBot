//! Per-event context and action routing.

use std::{fmt, sync::Arc};

use serde_json::Value;
use thiserror::Error;

use crate::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, ButtonInteraction, Event,
    EventEnvelope, EventId, MessageTarget, ReplyAction,
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
    button_interaction: Option<ButtonInteraction>,
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
            button_interaction: match &envelope.event {
                Event::ButtonInteraction(button) => Some(button.clone()),
                _ => None,
            },
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

    pub fn button_interaction(&self) -> Option<&ButtonInteraction> {
        self.button_interaction.as_ref()
    }

    pub fn reply_target(&self) -> Option<&MessageTarget> {
        self.reply_target.as_ref()
    }

    pub fn source_message_id(&self) -> Option<&str> {
        self.source_message_id.as_deref()
    }

    pub async fn reply(&self, content: impl Into<String>) -> Result<ActionResult, ContextError> {
        self.reply_with_event_id(self.event_id.as_str(), content)
            .await
    }

    /// Replies to the original message while attributing a derived event's action.
    pub async fn reply_with_event_id(
        &self,
        event_id: &str,
        content: impl Into<String>,
    ) -> Result<ActionResult, ContextError> {
        let target = self
            .reply_target
            .clone()
            .ok_or(ContextError::MissingReplyTarget)?;
        let source_message_id = self
            .source_message_id
            .clone()
            .ok_or(ContextError::MissingReplyTarget)?;
        self.execute_with_event_id(
            event_id,
            Action::Reply(ReplyAction {
                target,
                source_message_id,
                content: content.into(),
            }),
        )
        .await
    }

    pub async fn execute(&self, action: Action) -> Result<ActionResult, ContextError> {
        self.execute_with_event_id(self.event_id.as_str(), action)
            .await
    }

    /// Executes on the original Adapter with the current derived source event ID.
    pub async fn execute_with_event_id(
        &self,
        event_id: &str,
        action: Action,
    ) -> Result<ActionResult, ContextError> {
        crate::execute_traced_action(self.adapter.as_ref(), event_id, action)
            .await
            .map_err(ContextError::from)
    }
}
