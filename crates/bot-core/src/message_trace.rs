//! Portable message observations for the opt-in, bounded `WebUI` subscriber.
//! Console and file subscribers must exclude this target, including its content.

use crate::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, Event, EventEnvelope, MessageScope,
    MessageTarget,
};

fn scope(target: &MessageTarget) -> &'static str {
    match target.scope() {
        MessageScope::Private => "private",
        MessageScope::Group => "group",
        MessageScope::Channel => "channel",
    }
}

/// Executes one unified action and observes message outcomes in the active span.
///
/// `event_id` is the caller's stable source event ID, including for recovered or
/// scheduled commands. No replacement IDs or platform identity are generated.
pub async fn execute_traced_action(
    adapter: &dyn Adapter,
    event_id: &str,
    action: Action,
) -> Result<ActionResult, AdapterError> {
    let message = OutboundMessage::started(adapter.id(), event_id, &action);
    let result = adapter.execute(action).await;
    if let Some(message) = message {
        message.completed(adapter.id(), event_id, result.is_ok());
    }
    result
}

pub(crate) fn received(event: &EventEnvelope) {
    if let Event::Message(message) = &event.event {
        tracing::info!(
            target: "bkm::message_trace",
            record_kind = "message.received",
            adapter_id = %event.adapter,
            event_id = %event.id,
            direction = "inbound",
            message_scope = scope(&message.target),
            status = "received",
            content = %message.text,
            "received message"
        );
    }
}

struct OutboundMessage {
    action_type: &'static str,
    message_scope: &'static str,
}

impl OutboundMessage {
    fn started(adapter_id: &AdapterId, event_id: &str, action: &Action) -> Option<Self> {
        if !tracing::enabled!(target: "bkm::message_trace", tracing::Level::INFO) {
            return None;
        }
        let (action_type, content, target) = match action {
            Action::Reply(action) => ("message.reply", action.content.as_str(), &action.target),
            Action::SendMessage(action) => {
                ("message.send", action.content.as_str(), &action.target)
            }
            Action::ReplyMedia(action) => (
                "media.reply",
                action.caption.as_deref().unwrap_or_default(),
                &action.target,
            ),
            Action::SendMedia(action) => (
                "media.send",
                action.caption.as_deref().unwrap_or_default(),
                &action.target,
            ),
            // Platform payloads are opaque and can include credentials, identity,
            // media bytes or URLs. Never infer message text by serializing them.
            Action::Recall { .. } | Action::Platform { .. } => return None,
        };
        let message_scope = scope(target);
        // Observe borrowed text now: never retain an extra plaintext preview
        // across the Adapter await or expose pre-capture text to a later reader.
        tracing::info!(target: "bkm::message_trace", record_kind = "message.sending",
            adapter_id = %adapter_id, event_id, direction = "outbound", status = "sending",
            action_type, message_scope, content = %content, "sending message");
        Some(Self {
            action_type,
            message_scope,
        })
    }

    fn completed(self, adapter_id: &AdapterId, event_id: &str, succeeded: bool) {
        tracing::info!(
            target: "bkm::message_trace",
            record_kind = if succeeded { "message.sent" } else { "message.failed" },
            adapter_id = %adapter_id,
            event_id = %event_id,
            direction = "outbound",
            status = if succeeded { "succeeded" } else { "failed" },
            action_type = self.action_type,
            message_scope = self.message_scope,
            "message action completed"
        );
    }
}

#[cfg(test)]
mod tests;
