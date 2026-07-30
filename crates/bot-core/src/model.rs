//! Platform-independent event and action model.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }
    };
}

string_id!(AdapterId);
string_id!(EventId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageScope {
    Group,
    Private,
    Channel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum MessageTarget {
    Group { group_id: String },
    Private { user_id: String },
    Channel { channel_id: String },
}

impl MessageTarget {
    pub const fn scope(&self) -> MessageScope {
        match self {
            Self::Group { .. } => MessageScope::Group,
            Self::Private { .. } => MessageScope::Private,
            Self::Channel { .. } => MessageScope::Channel,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageSegment {
    Text { text: String },
    Platform { kind: String, data: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommonMessage {
    pub message_id: String,
    pub target: MessageTarget,
    pub sender: Sender,
    pub text: String,
    #[serde(default)]
    pub segments: Vec<MessageSegment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

impl CommonMessage {
    pub const fn scope(&self) -> MessageScope {
        self.target.scope()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    Message(CommonMessage),
    Notice(Value),
    Request(Value),
    Lifecycle(Value),
    Platform { name: String, payload: Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: EventId,
    pub adapter: AdapterId,
    #[serde(skip)]
    pub delivery_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    pub event: Event,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyAction {
    pub target: MessageTarget,
    pub source_message_id: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageAction {
    pub target: MessageTarget,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Action {
    Reply(ReplyAction),
    SendMessage(SendMessageAction),
    Recall {
        target: MessageTarget,
        message_id: String,
    },
    Platform {
        name: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub raw: Value,
}

#[cfg(test)]
mod tests {
    use super::{AdapterId, MessageScope, MessageTarget};

    #[test]
    fn ids_remain_opaque_strings() {
        let id = AdapterId::new("not-a-number/with-symbols");
        assert_eq!(id.as_str(), "not-a-number/with-symbols");
    }

    #[test]
    fn target_exposes_common_scope() {
        let target = MessageTarget::Group {
            group_id: "opaque".to_owned(),
        };
        assert_eq!(target.scope(), MessageScope::Group);
    }
}
