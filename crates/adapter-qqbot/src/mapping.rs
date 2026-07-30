//! QQ dispatch to platform-independent event mapping.

use bot_core::{
    AdapterId, CommonMessage, Event, EventEnvelope, EventId, MessageSegment, MessageTarget, Sender,
};
use chrono::{DateTime, Utc};
use qqbot_protocol::{GatewayPayload, QqMessage};
use thiserror::Error;

const GROUP_EVENTS: &[&str] = &["GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"];
const PRIVATE_EVENTS: &[&str] = &["C2C_MESSAGE_CREATE"];
const CHANNEL_EVENTS: &[&str] = &["AT_MESSAGE_CREATE", "MESSAGE_CREATE"];

#[derive(Debug, Error)]
pub(crate) enum MappingError {
    #[error("QQ message dispatch `{event_type}` could not be decoded")]
    Decode {
        event_type: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("QQ message dispatch `{event_type}` is missing {field}")]
    MissingField {
        event_type: String,
        field: &'static str,
    },
    #[error("QQ message dispatch `{event_type}` has an invalid timestamp")]
    InvalidTimestamp {
        event_type: String,
        #[source]
        source: chrono::ParseError,
    },
}

pub(crate) fn map_dispatch(
    adapter: &AdapterId,
    payload: &GatewayPayload,
) -> Result<Option<EventEnvelope>, MappingError> {
    let Some(event_type) = payload.t.as_deref() else {
        return Ok(None);
    };
    if !GROUP_EVENTS.contains(&event_type)
        && !PRIVATE_EVENTS.contains(&event_type)
        && !CHANNEL_EVENTS.contains(&event_type)
    {
        return Ok(None);
    }

    let message: QqMessage =
        serde_json::from_value(payload.d.clone()).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    let target = if GROUP_EVENTS.contains(&event_type) {
        MessageTarget::Group {
            group_id: required(message.group_openid.clone(), event_type, "group_openid")?,
        }
    } else if PRIVATE_EVENTS.contains(&event_type) {
        MessageTarget::Private {
            user_id: required(
                message.author.user_openid.clone(),
                event_type,
                "author.user_openid",
            )?,
        }
    } else {
        MessageTarget::Channel {
            channel_id: required(message.channel_id.clone(), event_type, "channel_id")?,
        }
    };
    let sender_id = [
        message.author.member_openid.as_deref(),
        message.author.user_openid.as_deref(),
        message.author.id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.trim().is_empty())
    .map(str::to_owned)
    .ok_or_else(|| MappingError::MissingField {
        event_type: event_type.to_owned(),
        field: "author identifier",
    })?;
    let message_id = required(Some(message.id), event_type, "message.id")?;
    let event_id = payload
        .id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| message_id.clone());
    let timestamp = message
        .timestamp
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|source| MappingError::InvalidTimestamp {
            event_type: event_type.to_owned(),
            source,
        })?
        .map(|value| value.with_timezone(&Utc));
    let text = message.content.clone();
    let raw = serde_json::to_value(payload).unwrap_or_else(|_| payload.d.clone());

    Ok(Some(EventEnvelope {
        id: EventId::new(event_id),
        adapter: adapter.clone(),
        delivery_id: None,
        timestamp,
        event: Event::Message(CommonMessage {
            message_id,
            target,
            sender: Sender {
                id: sender_id,
                display_name: message.author.username,
            },
            text: text.clone(),
            segments: vec![MessageSegment::Text { text }],
            reply_to: None,
        }),
        raw,
    }))
}

fn required(
    value: Option<String>,
    event_type: &str,
    field: &'static str,
) -> Result<String, MappingError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| MappingError::MissingField {
            event_type: event_type.to_owned(),
            field,
        })
}

#[cfg(test)]
mod tests {
    use bot_core::{AdapterId, Event, MessageTarget};
    use qqbot_protocol::{GatewayPayload, OpCode};
    use serde_json::json;

    use super::map_dispatch;

    #[test]
    fn maps_group_at_message() {
        let payload = GatewayPayload {
            id: Some("event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"message-id",
                "content":"/ping",
                "group_openid":"group-id",
                "author":{"member_openid":"member-id"}
            }),
            s: Some(2),
            t: Some("GROUP_AT_MESSAGE_CREATE".to_owned()),
        };

        let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        let Event::Message(message) = envelope.event else {
            panic!("expected common message");
        };
        assert_eq!(message.text, "/ping");
        assert_eq!(
            message.target,
            MessageTarget::Group {
                group_id: "group-id".to_owned()
            }
        );
    }
}
