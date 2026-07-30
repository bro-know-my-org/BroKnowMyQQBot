//! QQ message event and send-message types used by the first end-to-end path.

use serde::{Deserialize, Serialize};

/// A QQ message type.
///
/// Unknown numeric values can be preserved with [`MessageType::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageType(u8);

impl MessageType {
    pub const TEXT: Self = Self(0);
    pub const MARKDOWN: Self = Self(2);
    pub const ARK: Self = Self(3);
    pub const EMBED: Self = Self(4);
    pub const MEDIA: Self = Self(7);

    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

/// Author information shared by QQ message events.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageAuthor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_openid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_openid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_openid: Option<String>,
}

/// Attachment metadata included by QQ message events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// A message event received from QQ.
///
/// Fields that only apply to one scope remain optional so new platform fields
/// can be accepted without splitting the transport decoder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QqMessage {
    pub id: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub author: MessageAuthor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_openid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// Request body for QQ group and C2C message endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageRequest {
    msg_type: MessageType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    msg_seq: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    markdown: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media: Option<serde_json::Value>,
}

impl MessageRequest {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            msg_type: MessageType::TEXT,
            content: Some(content.into()),
            msg_id: None,
            msg_seq: None,
            markdown: None,
            media: None,
        }
    }

    pub fn reply_text(message_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            msg_id: Some(message_id.into()),
            ..Self::text(content)
        }
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let valid = match self.msg_type {
            MessageType::TEXT => {
                self.content.is_some() && self.markdown.is_none() && self.media.is_none()
            }
            MessageType::MARKDOWN => {
                self.content.is_none() && self.markdown.is_some() && self.media.is_none()
            }
            MessageType::MEDIA => {
                self.content.is_none() && self.markdown.is_none() && self.media.is_some()
            }
            _ => false,
        };
        valid
            .then_some(())
            .ok_or("message type and payload fields are inconsistent or unsupported")
    }
}

/// Request body for the channel message endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelMessageRequest {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
}

/// Common fields returned after sending a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::{MessageRequest, MessageType, QqMessage};

    #[test]
    fn decodes_group_message_with_unknown_fields() {
        let message: QqMessage = serde_json::from_str(
            r#"{
                "id":"event-message-id",
                "content":" /ping ",
                "group_openid":"group-open-id",
                "author":{"member_openid":"member-open-id"},
                "future_field":{"kept_by_raw_envelope":true}
            }"#,
        )
        .unwrap();

        assert_eq!(message.group_openid.as_deref(), Some("group-open-id"));
        assert_eq!(
            message.author.member_openid.as_deref(),
            Some("member-open-id")
        );
    }

    #[test]
    fn text_reply_matches_official_wire_shape() {
        let request = MessageRequest::reply_text("source-message", "pong");
        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["msg_type"], MessageType::TEXT.value());
        assert_eq!(value["content"], "pong");
        assert_eq!(value["msg_id"], "source-message");
        assert!(value.get("markdown").is_none());
    }
}
