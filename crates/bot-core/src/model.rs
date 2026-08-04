//! Platform-independent event and action model.

use std::{fmt, marker::PhantomData};

use chrono::{DateTime, Utc};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, SeqAccess, Visitor},
};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaAttachment {
    mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    data: Vec<u8>,
}

impl MediaAttachment {
    pub fn image(
        mime_type: impl Into<String>,
        filename: Option<String>,
        data: Vec<u8>,
    ) -> Result<Self, &'static str> {
        let attachment = Self {
            mime_type: mime_type.into(),
            filename,
            data,
        };
        attachment
            .validated_image_mime()
            .ok_or("media attachment must be a supported image of at most 8 MiB")?;
        Ok(attachment)
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// Returns the normalized supported image MIME type when it agrees with
    /// the attachment's file signature.
    pub fn validated_image_mime(&self) -> Option<&'static str> {
        if self.data.is_empty() || self.data.len() > MAX_MEDIA_ATTACHMENT_BYTES {
            return None;
        }
        let mime_type = self.mime_type.split(';').next().unwrap_or_default().trim();
        if mime_type.eq_ignore_ascii_case("image/png")
            && self.data.starts_with(b"\x89PNG\r\n\x1a\n")
        {
            Some("image/png")
        } else if mime_type.eq_ignore_ascii_case("image/jpeg")
            && self.data.starts_with(&[0xff, 0xd8, 0xff])
        {
            Some("image/jpeg")
        } else if mime_type.eq_ignore_ascii_case("image/gif")
            && (self.data.starts_with(b"GIF87a") || self.data.starts_with(b"GIF89a"))
        {
            Some("image/gif")
        } else if mime_type.eq_ignore_ascii_case("image/webp")
            && self.data.starts_with(b"RIFF")
            && self.data.get(8..12) == Some(b"WEBP")
        {
            Some("image/webp")
        } else {
            None
        }
    }
}

impl<'de> Deserialize<'de> for MediaAttachment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireAttachment {
            mime_type: String,
            #[serde(default)]
            filename: Option<String>,
            #[serde(deserialize_with = "deserialize_media_attachment_data")]
            data: Vec<u8>,
        }

        let wire = WireAttachment::deserialize(deserializer)?;
        Self::image(wire.mime_type, wire.filename, wire.data).map_err(de::Error::custom)
    }
}

const MAX_MEDIA_ATTACHMENT_BYTES: usize = 8 * 1024 * 1024;

fn deserialize_media_attachment_data<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoundedBytesVisitor(PhantomData<Vec<u8>>);

    impl<'de> Visitor<'de> for BoundedBytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_MEDIA_ATTACHMENT_BYTES} bytes")
        }

        fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value.len() > MAX_MEDIA_ATTACHMENT_BYTES {
                return Err(E::custom("media attachment exceeds the 8 MiB limit"));
            }
            Ok(value.to_vec())
        }

        fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value.len() > MAX_MEDIA_ATTACHMENT_BYTES {
                return Err(E::custom("media attachment exceeds the 8 MiB limit"));
            }
            Ok(value)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let capacity = sequence
                .size_hint()
                .unwrap_or_default()
                .min(MAX_MEDIA_ATTACHMENT_BYTES);
            let mut data = Vec::with_capacity(capacity);
            while let Some(byte) = sequence.next_element::<u8>()? {
                if data.len() == MAX_MEDIA_ATTACHMENT_BYTES {
                    return Err(de::Error::custom(
                        "media attachment exceeds the 8 MiB limit",
                    ));
                }
                data.push(byte);
            }
            Ok(data)
        }
    }

    deserializer.deserialize_byte_buf(BoundedBytesVisitor(PhantomData))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyMediaAction {
    pub target: MessageTarget,
    pub source_message_id: String,
    pub attachment: MediaAttachment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMediaAction {
    pub target: MessageTarget,
    pub attachment: MediaAttachment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Action {
    Reply(ReplyAction),
    SendMessage(SendMessageAction),
    ReplyMedia(ReplyMediaAction),
    SendMedia(SendMediaAction),
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
    use super::{AdapterId, MediaAttachment, MessageScope, MessageTarget};

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

    #[test]
    fn media_image_mime_must_match_the_file_signature() {
        let png = MediaAttachment::image(
            "Image/PNG; charset=binary",
            None,
            b"\x89PNG\r\n\x1a\nbody".to_vec(),
        )
        .unwrap();
        assert_eq!(png.validated_image_mime(), Some("image/png"));

        assert!(MediaAttachment::image("image/png", None, b"not an image".to_vec()).is_err());
        assert!(
            serde_json::from_value::<MediaAttachment>(serde_json::json!({
                "mime_type": "image/png",
                "data": [1, 2, 3]
            }))
            .is_err()
        );
    }
}
