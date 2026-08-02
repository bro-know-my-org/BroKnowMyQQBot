//! `OneBot` 11 event mapping into the platform-independent runtime model.

use bot_core::{
    AdapterId, CommonMessage, Event as CoreEvent, EventEnvelope, EventId,
    MessageSegment as CoreSegment, MessageTarget, Sender,
};
use chrono::{DateTime, Utc};
use onebot_protocol::{Event, MessageEvent, value_as_id};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const HEX: &[u8; 16] = b"0123456789abcdef";

#[derive(Debug, Error)]
pub enum MappingError {
    #[error("invalid OneBot event: {0}")]
    Protocol(#[from] onebot_protocol::ProtocolError),
    #[error("OneBot message type `{0}` is unsupported")]
    UnsupportedMessageType(String),
    #[error("OneBot group message is missing group_id")]
    MissingGroupId,
}

pub fn map_event(adapter: &AdapterId, raw: Value) -> Result<Option<EventEnvelope>, MappingError> {
    let event = Event::parse(raw.clone())?;
    let timestamp = raw
        .get("time")
        .and_then(Value::as_i64)
        .and_then(DateTime::<Utc>::from_timestamp_secs);
    let (event_id, mapped) = match event {
        Event::Message(message) => {
            let event_id = message_event_id(adapter, &message);
            (event_id, CoreEvent::Message(map_message(*message)?))
        }
        Event::Notice(notice) => (
            derived_event_id(adapter, "notice", notice.time, &raw, &notice.notice_type),
            CoreEvent::Notice(raw.clone()),
        ),
        Event::Request(request) => (
            derived_event_id(
                adapter,
                "request",
                request.time,
                &raw,
                &request.request_type,
            ),
            CoreEvent::Request(raw.clone()),
        ),
        Event::Meta(meta) => (
            derived_event_id(adapter, "meta", meta.time, &raw, &meta.meta_event_type),
            CoreEvent::Lifecycle(raw.clone()),
        ),
        Event::Unknown {
            post_type,
            raw: unknown_raw,
        } => (
            derived_event_id(
                adapter,
                "unknown",
                unknown_raw
                    .get("time")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                &unknown_raw,
                &post_type,
            ),
            CoreEvent::Platform {
                name: format!("onebot11.{post_type}"),
                payload: unknown_raw,
            },
        ),
    };
    Ok(Some(EventEnvelope {
        id: EventId::new(event_id),
        adapter: adapter.clone(),
        delivery_id: None,
        timestamp,
        event: mapped,
        raw,
    }))
}

fn map_message(message: MessageEvent) -> Result<CommonMessage, MappingError> {
    let target = match message.message_type.as_str() {
        "group" => MessageTarget::Group {
            group_id: message
                .group_id
                .ok_or(MappingError::MissingGroupId)?
                .to_string(),
        },
        "private" => MessageTarget::Private {
            user_id: message.user_id.to_string(),
        },
        other => return Err(MappingError::UnsupportedMessageType(other.to_owned())),
    };
    let text = message.message.text_content();
    let source_segments = message.message.into_segments();
    let reply_to = source_segments
        .iter()
        .find(|segment| segment.kind == "reply")
        .and_then(|segment| segment.data.get("id"))
        .and_then(value_as_id);
    let segments = source_segments
        .into_iter()
        .map(|segment| {
            if segment.kind == "text" {
                CoreSegment::Text {
                    text: segment
                        .data
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }
            } else {
                CoreSegment::Platform {
                    kind: format!("onebot11.{}", segment.kind),
                    data: Value::Object(segment.data.into_iter().collect()),
                }
            }
        })
        .collect();
    Ok(CommonMessage {
        message_id: message.message_id.to_string(),
        target,
        sender: Sender {
            id: message.user_id.to_string(),
            display_name: message.sender.display_name(),
        },
        text,
        segments,
        reply_to,
    })
}

fn derived_event_id(
    adapter: &AdapterId,
    kind: &str,
    time: i64,
    data: &Value,
    detail_type: &str,
) -> String {
    let discriminator = payload_digest(&serde_json::json!({
        "adapter": adapter.as_str(),
        "event": data,
    }));
    format!("onebot11:{kind}:{detail_type}:{time}:{discriminator}")
}

fn message_event_id(adapter: &AdapterId, message: &MessageEvent) -> String {
    let discriminator = payload_digest(&serde_json::json!({
        "adapter": adapter.as_str(),
        "self_id": message.self_id.as_str(),
        "message_id": message.message_id.as_str(),
    }));
    format!("onebot11:message:{discriminator}")
}

fn payload_digest(data: &Value) -> String {
    let canonical = canonicalize_json(data);
    let encoded = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(encoded);
    let mut discriminator = String::with_capacity(digest.len() * 2);
    for byte in digest {
        discriminator.push(char::from(HEX[usize::from(byte >> 4)]));
        discriminator.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    discriminator
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonicalize_json(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use bot_core::{AdapterId, Event, MessageSegment, MessageTarget};
    use serde_json::json;

    use super::map_event;

    #[test]
    fn maps_group_message_and_preserves_segments() {
        let envelope = map_event(
            &AdapterId::new("onebot"),
            json!({
                "time": 1_700_000_000,
                "self_id": 10000,
                "post_type": "message",
                "message_type": "group",
                "sub_type": "normal",
                "message_id": 200,
                "group_id": 300,
                "user_id": 400,
                "message": [
                    {"type":"at","data":{"qq":"10000"}},
                    {"type":"text","data":{"text":" /ping"}}
                ],
                "raw_message": "[CQ:at,qq=10000] /ping",
                "sender": {"nickname":"Tester"}
            }),
        )
        .unwrap()
        .unwrap();
        let Event::Message(message) = envelope.event else {
            panic!("expected message event");
        };
        assert_eq!(message.text, " /ping");
        assert_eq!(
            message.target,
            MessageTarget::Group {
                group_id: "300".to_owned()
            }
        );
        assert!(matches!(
            message.segments[0],
            MessageSegment::Platform { .. }
        ));
    }

    #[test]
    fn maps_private_string_message() {
        let envelope = map_event(
            &AdapterId::new("onebot"),
            json!({
                "time": 1_700_000_000,
                "self_id": "bot",
                "post_type": "message",
                "message_type": "private",
                "sub_type": "friend",
                "message_id": "message",
                "user_id": "user",
                "message": "/ping",
                "raw_message": "/ping",
                "sender": {}
            }),
        )
        .unwrap()
        .unwrap();
        let Event::Message(message) = envelope.event else {
            panic!("expected message event");
        };
        assert_eq!(
            message.target,
            MessageTarget::Private {
                user_id: "user".to_owned()
            }
        );
        assert_eq!(message.text, "/ping");
    }

    #[test]
    fn derived_event_ids_distinguish_same_second_events() {
        let first = map_event(
            &AdapterId::new("onebot"),
            json!({
                "time": 1,
                "self_id": 2,
                "post_type": "notice",
                "notice_type": "group_increase",
                "sub_type": "approve",
                "group_id": 3,
                "user_id": 4
            }),
        )
        .unwrap()
        .unwrap();
        let second = map_event(
            &AdapterId::new("onebot"),
            json!({
                "time": 1,
                "self_id": 2,
                "post_type": "notice",
                "notice_type": "group_increase",
                "sub_type": "invite",
                "group_id": 3,
                "user_id": 4
            }),
        )
        .unwrap()
        .unwrap();
        assert_ne!(first.id, second.id);

        let unknown_a = map_event(
            &AdapterId::new("onebot"),
            json!({"time":1,"post_type":"vendor","sequence":1}),
        )
        .unwrap()
        .unwrap();
        let unknown_b = map_event(
            &AdapterId::new("onebot"),
            json!({"time":1,"post_type":"vendor","sequence":2}),
        )
        .unwrap()
        .unwrap();
        assert_ne!(unknown_a.id, unknown_b.id);

        let reordered_a = map_event(
            &AdapterId::new("onebot"),
            serde_json::from_str(r#"{"time":1,"post_type":"vendor","nested":{"a":1,"b":2}}"#)
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let reordered_b = map_event(
            &AdapterId::new("onebot"),
            serde_json::from_str(r#"{"nested":{"b":2,"a":1},"post_type":"vendor","time":1}"#)
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(reordered_a.id, reordered_b.id);
    }

    #[test]
    fn message_event_ids_are_unambiguous_for_opaque_ids() {
        let event = |self_id: &str, message_id: &str| {
            map_event(
                &AdapterId::new("onebot"),
                json!({
                    "time": 1,
                    "self_id": self_id,
                    "post_type": "message",
                    "message_type": "private",
                    "message_id": message_id,
                    "user_id": "user",
                    "message": "hello",
                    "sender": {}
                }),
            )
            .unwrap()
            .unwrap()
        };
        let first = event("a:message:b", "c");
        let second = event("a", "b:message:c");
        assert_ne!(first.id, second.id);

        let raw = json!({
            "time": 1,
            "self_id": "bot",
            "post_type": "message",
            "message_type": "private",
            "message_id": "same",
            "user_id": "user",
            "message": "hello",
            "sender": {}
        });
        let adapter_a = map_event(&AdapterId::new("onebot-a"), raw.clone())
            .unwrap()
            .unwrap();
        let adapter_b = map_event(&AdapterId::new("onebot-b"), raw)
            .unwrap()
            .unwrap();
        assert_ne!(adapter_a.id, adapter_b.id);
    }

    #[test]
    fn outbound_message_sent_is_not_delivered_as_inbound_message() {
        let envelope = map_event(
            &AdapterId::new("onebot"),
            json!({
                "time": 1,
                "self_id": "bot",
                "post_type": "message_sent",
                "message_type": "private",
                "message_id": "message",
                "user_id": "user",
                "message": "/ping",
                "sender": {}
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(envelope.timestamp.unwrap().timestamp(), 1);
        assert!(matches!(
            envelope.event,
            Event::Platform { ref name, .. } if name == "onebot11.message_sent"
        ));
    }
}
