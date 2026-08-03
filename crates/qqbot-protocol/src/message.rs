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
    keyboard: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ark: Option<serde_json::Value>,
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
            keyboard: None,
            ark: None,
            media: None,
        }
    }

    pub fn reply_text(message_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            msg_id: Some(message_id.into()),
            ..Self::text(content)
        }
    }

    pub fn markdown(markdown: serde_json::Value, keyboard: Option<serde_json::Value>) -> Self {
        Self {
            msg_type: MessageType::MARKDOWN,
            content: None,
            msg_id: None,
            msg_seq: None,
            markdown: Some(markdown),
            keyboard,
            ark: None,
            media: None,
        }
    }

    pub fn ark(ark: serde_json::Value) -> Self {
        Self {
            msg_type: MessageType::ARK,
            content: None,
            msg_id: None,
            msg_seq: None,
            markdown: None,
            keyboard: None,
            ark: Some(ark),
            media: None,
        }
    }

    pub fn media(file_info: impl Into<String>) -> Self {
        Self {
            msg_type: MessageType::MEDIA,
            content: None,
            msg_id: None,
            msg_seq: None,
            markdown: None,
            keyboard: None,
            ark: None,
            media: Some(serde_json::json!({ "file_info": file_info.into() })),
        }
    }

    #[must_use]
    pub fn with_reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.msg_id = Some(message_id.into());
        self
    }

    #[must_use]
    pub const fn with_sequence(mut self, sequence: u32) -> Self {
        self.msg_seq = Some(sequence);
        self
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let valid = match self.msg_type {
            MessageType::TEXT => {
                self.content.is_some()
                    && self.markdown.is_none()
                    && self.keyboard.is_none()
                    && self.ark.is_none()
                    && self.media.is_none()
            }
            MessageType::MARKDOWN => {
                self.content.is_none()
                    && self
                        .markdown
                        .as_ref()
                        .is_some_and(serde_json::Value::is_object)
                    && self
                        .keyboard
                        .as_ref()
                        .is_none_or(serde_json::Value::is_object)
                    && self.ark.is_none()
                    && self.media.is_none()
            }
            MessageType::ARK => {
                self.content.is_none()
                    && self.markdown.is_none()
                    && self.keyboard.is_none()
                    && self.ark.as_ref().is_some_and(serde_json::Value::is_object)
                    && self.media.is_none()
            }
            MessageType::MEDIA => {
                self.content.is_none()
                    && self.markdown.is_none()
                    && self.keyboard.is_none()
                    && self.ark.is_none()
                    && self
                        .media
                        .as_ref()
                        .is_some_and(serde_json::Value::is_object)
            }
            _ => false,
        };
        valid
            .then_some(())
            .ok_or("message type and payload fields are inconsistent or unsupported")
    }
}

/// Request body for the channel message endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelMessageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embed: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ark: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    markdown: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keyboard: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    msg_id: Option<String>,
}

impl ChannelMessageRequest {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            embed: None,
            ark: None,
            markdown: None,
            keyboard: None,
            msg_id: None,
        }
    }

    pub fn markdown(markdown: serde_json::Value, keyboard: Option<serde_json::Value>) -> Self {
        Self {
            content: None,
            embed: None,
            ark: None,
            markdown: Some(markdown),
            keyboard,
            msg_id: None,
        }
    }

    pub fn embed(embed: serde_json::Value) -> Self {
        Self {
            content: None,
            embed: Some(embed),
            ark: None,
            markdown: None,
            keyboard: None,
            msg_id: None,
        }
    }

    pub fn ark(ark: serde_json::Value) -> Self {
        Self {
            content: None,
            embed: None,
            ark: Some(ark),
            markdown: None,
            keyboard: None,
            msg_id: None,
        }
    }

    #[must_use]
    pub fn with_reply_to(mut self, message_id: impl Into<String>) -> Self {
        self.msg_id = Some(message_id.into());
        self
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        let payloads = [
            self.content.is_some(),
            self.embed.is_some(),
            self.ark.is_some(),
            self.markdown.is_some(),
        ];
        if payloads.into_iter().filter(|present| *present).count() != 1
            || (self.keyboard.is_some() && self.markdown.is_none())
            || self.embed.as_ref().is_some_and(|value| !value.is_object())
            || self.ark.as_ref().is_some_and(|value| !value.is_object())
            || self
                .markdown
                .as_ref()
                .is_some_and(|value| !value.is_object())
            || self
                .keyboard
                .as_ref()
                .is_some_and(|value| !value.is_object())
        {
            return Err("channel message must contain exactly one supported payload");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MediaFileType(u8);

impl MediaFileType {
    pub const IMAGE: Self = Self(1);
    pub const VIDEO: Self = Self(2);
    pub const AUDIO: Self = Self(3);
    pub const FILE: Self = Self(4);

    pub const fn is_supported(self) -> bool {
        matches!(self.0, 1..=4)
    }
}

impl TryFrom<u8> for MediaFileType {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let file_type = Self(value);
        file_type
            .is_supported()
            .then_some(file_type)
            .ok_or("QQ media file type must be one of 1 (image), 2 (video), 3 (audio), or 4 (file)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaUploadRequest {
    pub file_type: MediaFileType,
    pub url: String,
    #[serde(default)]
    pub srv_send_msg: bool,
}

impl MediaUploadRequest {
    pub fn from_url(file_type: MediaFileType, url: impl Into<String>) -> Self {
        Self {
            file_type,
            url: url.into(),
            srv_send_msg: false,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if !self.file_type.is_supported() {
            return Err("QQ media upload contains an unsupported file type");
        }
        let url = url::Url::parse(&self.url)
            .map_err(|_| "QQ media upload URL must be an absolute URL")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("QQ media upload URL must use HTTP or HTTPS and include a host");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaUploadResponse {
    pub file_uuid: String,
    pub file_info: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
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
    use super::{
        ChannelMessageRequest, MediaFileType, MediaUploadRequest, MessageRequest, MessageType,
        QqMessage,
    };
    use serde_json::json;

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

    #[test]
    fn rich_and_media_requests_match_wire_shapes() {
        let markdown = MessageRequest::markdown(
            json!({"custom_template_id":"template"}),
            Some(json!({"id":"keyboard"})),
        )
        .with_reply_to("source")
        .with_sequence(2);
        let value = serde_json::to_value(markdown).unwrap();
        assert_eq!(value["msg_type"], MessageType::MARKDOWN.value());
        assert_eq!(value["keyboard"]["id"], "keyboard");
        assert_eq!(value["msg_seq"], 2);

        let media = serde_json::to_value(MessageRequest::media("file-info")).unwrap();
        assert_eq!(media["msg_type"], MessageType::MEDIA.value());
        assert_eq!(media["media"]["file_info"], "file-info");

        let channel = serde_json::to_value(
            ChannelMessageRequest::ark(json!({"template_id": 23})).with_reply_to("source"),
        )
        .unwrap();
        assert_eq!(channel["ark"]["template_id"], 23);
        assert_eq!(channel["msg_id"], "source");

        let embed = serde_json::to_value(ChannelMessageRequest::embed(json!({
            "title":"status"
        })))
        .unwrap();
        assert_eq!(embed["embed"]["title"], "status");

        let upload = serde_json::to_value(MediaUploadRequest::from_url(
            MediaFileType::IMAGE,
            "https://example.com/image.png",
        ))
        .unwrap();
        assert_eq!(upload["file_type"], 1);
        assert_eq!(upload["srv_send_msg"], false);
        assert!(MediaFileType::try_from(0).is_err());
        assert!(MediaFileType::try_from(5).is_err());
        assert!(
            MediaUploadRequest::from_url(MediaFileType::IMAGE, "file:///tmp/image.png")
                .validate()
                .is_err()
        );
    }
}
