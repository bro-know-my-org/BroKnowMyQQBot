//! QQ dispatch to platform-independent event mapping.

use std::fmt::Write as _;

use bot_core::{
    AdapterId, CommonMessage, Event, EventEnvelope, EventId, MessageSegment, MessageTarget, Sender,
};
use chrono::{DateTime, Utc};
use qqbot_protocol::{GatewayPayload, QqMessage};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const GROUP_EVENTS: &[&str] = &["GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"];
const PRIVATE_EVENTS: &[&str] = &["C2C_MESSAGE_CREATE"];
const CHANNEL_EVENTS: &[&str] = &["AT_MESSAGE_CREATE", "MESSAGE_CREATE"];
const NOTICE_EVENTS: &[&str] = &[
    "GUILD_CREATE",
    "GUILD_UPDATE",
    "GUILD_DELETE",
    "CHANNEL_CREATE",
    "CHANNEL_UPDATE",
    "CHANNEL_DELETE",
    "GUILD_MEMBER_ADD",
    "GUILD_MEMBER_UPDATE",
    "GUILD_MEMBER_REMOVE",
    "MESSAGE_REACTION_ADD",
    "MESSAGE_REACTION_REMOVE",
    "GROUP_ADD_ROBOT",
    "GROUP_DEL_ROBOT",
    "GROUP_MSG_REJECT",
    "GROUP_MSG_RECEIVE",
    "FRIEND_ADD",
    "FRIEND_DEL",
    "C2C_MSG_REJECT",
    "C2C_MSG_RECEIVE",
    "MESSAGE_AUDIT_PASS",
    "MESSAGE_AUDIT_REJECT",
];

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
    #[error("QQ notice dispatch `{event_type}` contains invalid data")]
    InvalidNoticeData { event_type: String },
}

pub(crate) fn map_dispatch(
    adapter: &AdapterId,
    payload: &GatewayPayload,
) -> Result<Option<EventEnvelope>, MappingError> {
    let Some(event_type) = payload.t.as_deref() else {
        return Ok(None);
    };
    if NOTICE_EVENTS.contains(&event_type) {
        return map_notice(adapter, payload, event_type).map(Some);
    }
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

fn map_notice(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    if !payload.d.is_object() {
        return Err(MappingError::InvalidNoticeData {
            event_type: event_type.to_owned(),
        });
    }
    let raw = serde_json::to_value(payload).unwrap_or_else(|_| payload.d.clone());
    let event_id = payload
        .id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || {
                let sequence = payload.s.ok_or_else(|| MappingError::MissingField {
                    event_type: event_type.to_owned(),
                    field: "dispatch id or sequence",
                })?;
                let digest = Sha256::digest(
                    serde_json::to_vec(&(event_type, &payload.d)).unwrap_or_default(),
                );
                let mut encoded = String::with_capacity(digest.len() * 2);
                for byte in digest {
                    let _ = write!(encoded, "{byte:02x}");
                }
                Ok(format!("qq:{event_type}:{sequence}:{encoded}"))
            },
            Ok,
        )?;
    let timestamp = match payload.d.get("timestamp") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => Some(
            DateTime::parse_from_rfc3339(value)
                .map_err(|source| MappingError::InvalidTimestamp {
                    event_type: event_type.to_owned(),
                    source,
                })?
                .with_timezone(&Utc),
        ),
        Some(_) => {
            return Err(MappingError::InvalidNoticeData {
                event_type: event_type.to_owned(),
            });
        }
    };
    Ok(EventEnvelope {
        id: EventId::new(event_id),
        adapter: adapter.clone(),
        delivery_id: None,
        timestamp,
        event: Event::Notice(serde_json::json!({
            "type": event_type,
            "data": payload.d,
        })),
        raw,
    })
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

    #[test]
    fn maps_group_management_notice_with_stable_dispatch_id() {
        let payload = GatewayPayload {
            id: Some("notice-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "group_openid":"group-id",
                "op_member_openid":"member-id",
                "timestamp":"2026-08-03T10:00:00Z"
            }),
            s: Some(3),
            t: Some("GROUP_ADD_ROBOT".to_owned()),
        };

        let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        assert_eq!(envelope.id.as_str(), "notice-id");
        let Event::Notice(notice) = envelope.event else {
            panic!("expected notice event");
        };
        assert_eq!(notice["type"], "GROUP_ADD_ROBOT");
        assert_eq!(notice["data"]["group_openid"], "group-id");
    }

    #[test]
    fn derives_stable_notice_id_when_dispatch_id_is_absent() {
        let payload = GatewayPayload {
            id: None,
            op: OpCode::DISPATCH,
            d: json!({
                "group_openid":"group-id",
                "timestamp":"2026-08-03T10:00:00Z"
            }),
            s: Some(4),
            t: Some("GROUP_MSG_RECEIVE".to_owned()),
        };

        let first = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        let second = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        assert_eq!(first.id, second.id);
        assert!(first.id.as_str().starts_with("qq:GROUP_MSG_RECEIVE:4:"));

        let mut next_delivery = payload;
        next_delivery.s = Some(5);
        let third = map_dispatch(&AdapterId::new("qq"), &next_delivery)
            .unwrap()
            .unwrap();
        assert_ne!(first.id, third.id);
    }

    #[test]
    fn rejects_malformed_notice_timestamp() {
        let mut payload = GatewayPayload {
            id: Some("notice-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({"timestamp":"not-a-timestamp"}),
            s: Some(6),
            t: Some("GUILD_UPDATE".to_owned()),
        };

        assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        payload.d = json!({"timestamp":42});
        assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        payload.d = json!([]);
        assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
    }
}
