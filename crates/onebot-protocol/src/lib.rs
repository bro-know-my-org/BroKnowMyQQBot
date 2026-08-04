//! `OneBot` 11 protocol types shared by transports and adapters.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OneBotId(String);

impl OneBotId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_json(&self) -> Value {
        Value::String(self.0.clone())
    }
}

impl fmt::Debug for OneBotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("OneBotId").field(&self.0).finish()
    }
}

impl fmt::Display for OneBotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for OneBotId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OneBotId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IdVisitor;

        impl Visitor<'_> for IdVisitor {
            type Value = OneBotId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a OneBot string or integer ID")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OneBotId::new(value))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(OneBotId::new(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OneBotId::new(value.to_string()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OneBotId::new(value.to_string()))
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageSegment {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub data: BTreeMap<String, Value>,
}

impl MessageSegment {
    pub fn text(value: impl Into<String>) -> Self {
        Self {
            kind: "text".to_owned(),
            data: BTreeMap::from([("text".to_owned(), Value::String(value.into()))]),
        }
    }

    pub fn reply(message_id: &OneBotId) -> Self {
        Self {
            kind: "reply".to_owned(),
            data: BTreeMap::from([("id".to_owned(), message_id.to_json())]),
        }
    }

    pub fn image_bytes(data: &[u8]) -> Self {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        Self {
            kind: "image".to_owned(),
            data: BTreeMap::from([(
                "file".to_owned(),
                Value::String(format!("base64://{}", STANDARD.encode(data))),
            )]),
        }
    }

    pub fn text_value(&self) -> Option<&str> {
        (self.kind == "text")
            .then(|| self.data.get("text").and_then(Value::as_str))
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Segments(Vec<MessageSegment>),
    Text(String),
}

impl Message {
    pub fn text_content(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Segments(segments) => segments
                .iter()
                .filter_map(MessageSegment::text_value)
                .collect(),
        }
    }

    pub fn into_segments(self) -> Vec<MessageSegment> {
        match self {
            Self::Segments(segments) => segments,
            Self::Text(text) => vec![MessageSegment::text(text)],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageSender {
    #[serde(default)]
    pub user_id: Option<OneBotId>,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub card: Option<String>,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl MessageSender {
    pub fn display_name(&self) -> Option<String> {
        self.card
            .as_deref()
            .filter(|value| !value.is_empty())
            .or_else(|| self.nickname.as_deref().filter(|value| !value.is_empty()))
            .map(str::to_owned)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEvent {
    pub time: i64,
    pub self_id: OneBotId,
    pub message_type: String,
    #[serde(default)]
    pub sub_type: String,
    pub message_id: OneBotId,
    pub user_id: OneBotId,
    #[serde(default)]
    pub group_id: Option<OneBotId>,
    pub message: Message,
    #[serde(default)]
    pub raw_message: String,
    #[serde(default)]
    pub sender: MessageSender,
    #[serde(default, flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoticeEvent {
    pub time: i64,
    pub self_id: OneBotId,
    pub notice_type: String,
    #[serde(default)]
    pub sub_type: String,
    #[serde(default, flatten)]
    pub data: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEvent {
    pub time: i64,
    pub self_id: OneBotId,
    pub request_type: String,
    #[serde(default)]
    pub sub_type: String,
    #[serde(default, flatten)]
    pub data: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaEvent {
    pub time: i64,
    pub self_id: OneBotId,
    pub meta_event_type: String,
    #[serde(default)]
    pub sub_type: String,
    #[serde(default, flatten)]
    pub data: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Message(Box<MessageEvent>),
    Notice(NoticeEvent),
    Request(RequestEvent),
    Meta(MetaEvent),
    Unknown { post_type: String, raw: Value },
}

impl Event {
    pub fn parse(raw: Value) -> Result<Self, ProtocolError> {
        let post_type = raw
            .get("post_type")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::MissingPostType)?;
        match post_type {
            "message" => decode_event(raw).map(Box::new).map(Self::Message),
            "message_sent" => Ok(Self::Unknown {
                post_type: post_type.to_owned(),
                raw,
            }),
            "notice" => decode_event(raw).map(Self::Notice),
            "request" => decode_event(raw).map(Self::Request),
            "meta_event" => decode_event(raw).map(Self::Meta),
            other => Ok(Self::Unknown {
                post_type: other.to_owned(),
                raw,
            }),
        }
    }

    pub const fn time(&self) -> Option<i64> {
        match self {
            Self::Message(event) => Some(event.time),
            Self::Notice(event) => Some(event.time),
            Self::Request(event) => Some(event.time),
            Self::Meta(event) => Some(event.time),
            Self::Unknown { .. } => None,
        }
    }
}

fn decode_event<T>(raw: Value) -> Result<T, ProtocolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(raw).map_err(ProtocolError::Event)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRequest {
    pub action: String,
    #[serde(default)]
    pub params: Value,
    pub echo: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResponse {
    pub status: String,
    pub retcode: i64,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub wording: String,
    pub echo: Value,
}

impl ActionResponse {
    pub fn echo_key(&self) -> Option<String> {
        match &self.echo {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.status == "ok" && self.retcode == 0
    }

    pub fn message_id(&self) -> Option<String> {
        self.data.get("message_id").and_then(value_as_id)
    }

    pub fn safe_error(&self) -> String {
        let message = if self.wording.is_empty() {
            &self.message
        } else {
            &self.wording
        };
        format!(
            "OneBot action failed with retcode {}: {message}",
            self.retcode
        )
    }
}

pub fn value_as_id(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

pub fn response_like(value: &Value) -> bool {
    value.get("status").is_some_and(Value::is_string)
        && value.get("retcode").and_then(Value::as_i64).is_some()
        && matches!(value.get("echo"), Some(Value::String(_) | Value::Number(_)))
}

pub fn object_without(value: &Value, keys: &[&str]) -> Map<String, Value> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !keys.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("OneBot event is missing string post_type")]
    MissingPostType,
    #[error("OneBot event could not be decoded")]
    Event(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ActionResponse, Event, Message, MessageSegment, OneBotId, response_like};

    const GROUP_MESSAGE: &str = include_str!("../../../test-data/onebot11/group-message.json");
    const PRIVATE_MESSAGE: &str = include_str!("../../../test-data/onebot11/private-message.json");
    const NOTICE: &str = include_str!("../../../test-data/onebot11/notice.json");
    const REQUEST: &str = include_str!("../../../test-data/onebot11/request.json");
    const LIFECYCLE: &str = include_str!("../../../test-data/onebot11/lifecycle.json");
    const ACTION_RESPONSE: &str = include_str!("../../../test-data/onebot11/action-response.json");

    #[test]
    fn ids_accept_numbers_and_strings() {
        let numeric: OneBotId = serde_json::from_value(json!(12345)).unwrap();
        let textual: OneBotId = serde_json::from_value(json!("00123")).unwrap();
        assert_eq!(numeric.as_str(), "12345");
        assert_eq!(textual.as_str(), "00123");
        assert_eq!(textual.to_json(), json!("00123"));
    }

    #[test]
    fn image_bytes_use_the_onebot_base64_uri_shape() {
        let segment = MessageSegment::image_bytes(b"png");
        assert_eq!(segment.kind, "image");
        assert_eq!(segment.data["file"], "base64://cG5n");
    }

    #[test]
    fn parses_array_message_and_preserves_unknown_segments() {
        let event = Event::parse(json!({
            "time": 1,
            "self_id": 42,
            "post_type": "message",
            "message_type": "group",
            "sub_type": "normal",
            "message_id": 7,
            "group_id": 9,
            "user_id": 8,
            "message": [
                {"type":"at","data":{"qq":"42"}},
                {"type":"text","data":{"text":" /ping"}}
            ],
            "raw_message": "[CQ:at,qq=42] /ping",
            "sender": {"nickname":"Tester"}
        }))
        .unwrap();
        let Event::Message(event) = event else {
            panic!("expected message event");
        };
        assert_eq!(event.message.text_content(), " /ping");
        let Message::Segments(segments) = event.message else {
            panic!("expected segment message");
        };
        assert_eq!(segments[0].kind, "at");
    }

    #[test]
    fn action_response_accepts_numeric_echo_and_message_id() {
        let response: ActionResponse = serde_json::from_value(json!({
            "status":"ok",
            "retcode":0,
            "data":{"message_id":88},
            "echo":17
        }))
        .unwrap();
        assert_eq!(response.echo_key().as_deref(), Some("17"));
        assert_eq!(response.message_id().as_deref(), Some("88"));
    }

    #[test]
    fn protocol_fixtures_parse_and_preserve_compatible_extensions() {
        for fixture in [GROUP_MESSAGE, PRIVATE_MESSAGE, NOTICE, REQUEST, LIFECYCLE] {
            let raw = serde_json::from_str(fixture).unwrap();
            Event::parse(raw).unwrap();
        }

        let Event::Message(group) =
            Event::parse(serde_json::from_str(GROUP_MESSAGE).unwrap()).unwrap()
        else {
            panic!("expected group message fixture");
        };
        assert_eq!(group.extensions.get("future_field"), Some(&json!(true)));

        let response: ActionResponse = serde_json::from_str(ACTION_RESPONSE).unwrap();
        assert!(response.succeeded());
        assert_eq!(response.echo_key().as_deref(), Some("fixture-echo"));
        assert_eq!(response.message_id().as_deref(), Some("202"));
    }

    #[test]
    fn response_discriminator_requires_valid_core_field_types() {
        assert!(response_like(&json!({
            "status":"ok", "retcode":0, "echo":"request"
        })));
        assert!(!response_like(&json!({"retcode":0, "echo":"request"})));
        assert!(!response_like(&json!({
            "status":"ok", "retcode":"0", "echo":"request"
        })));
        assert!(!response_like(&json!({
            "status":"ok", "retcode":0.5, "echo":"request"
        })));
        assert!(!response_like(&json!({
            "status":"ok", "retcode":0, "echo":null
        })));
    }
}
