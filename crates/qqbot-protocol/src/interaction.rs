//! Typed QQ interaction events and response requests.

use std::{collections::BTreeMap, fmt};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Validation failures for interaction events and responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionValidationError {
    EmptyField { field: &'static str },
    InvalidTimestamp,
    PrefixedInteractionId,
    InvalidInteractionId,
    InvalidField { field: &'static str },
    MismatchedInteractionType { top_level: u32, data: u32 },
    InvalidResponseCode { code: u8 },
}

impl fmt::Display for InteractionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => {
                write!(formatter, "QQ interaction `{field}` must not be empty")
            }
            Self::InvalidTimestamp => {
                formatter.write_str("QQ interaction `timestamp` must be RFC 3339")
            }
            Self::PrefixedInteractionId => formatter.write_str(
                "QQ interaction id must use event data.id without an INTERACTION_CREATE prefix",
            ),
            Self::InvalidInteractionId => formatter
                .write_str("QQ interaction id must not contain whitespace or control characters"),
            Self::InvalidField { field } => write!(
                formatter,
                "QQ interaction `{field}` must not contain whitespace or control characters"
            ),
            Self::MismatchedInteractionType { top_level, data } => write!(
                formatter,
                "QQ interaction top-level type {top_level} does not match data.type {data}"
            ),
            Self::InvalidResponseCode { code } => write!(
                formatter,
                "QQ interaction response code must be between 0 and 5, got {code}"
            ),
        }
    }
}

impl std::error::Error for InteractionValidationError {}

/// Forward-compatible payload nested under an interaction event's `data` field.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InteractionData {
    /// Interaction type repeated by QQ inside `data` when present.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub interaction_type: Option<u32>,
    /// Button, feedback, authorization, or other resolved interaction details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Value>,
    /// Fields introduced by future QQ interaction payload versions.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Data carried by an `INTERACTION_CREATE` dispatch.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InteractionEvent {
    /// One-time identifier used by the interaction response endpoint.
    pub id: String,
    /// Numeric interaction type; unknown future values are preserved.
    #[serde(rename = "type")]
    pub interaction_type: u32,
    /// Optional scene such as `c2c`, `group`, or `guild`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// Optional numeric chat type supplied by QQ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_type: Option<u32>,
    /// RFC 3339 event timestamp.
    pub timestamp: String,
    /// Guild identifier for guild-scoped interactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    /// Channel identifier for guild-scoped interactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// User `OpenID` for C2C interactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_openid: Option<String>,
    /// Group `OpenID` for group interactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_openid: Option<String>,
    /// Member `OpenID` for the actor in a group interaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_member_openid: Option<String>,
    /// Interaction-specific resolved details when supplied by QQ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<InteractionData>,
    /// Interaction payload version when supplied by QQ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// QQ application identifier associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    /// Fields introduced by future QQ interaction payload versions.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl InteractionEvent {
    /// Validates required event fields without rejecting unknown interaction types.
    pub fn validate(&self) -> Result<(), InteractionValidationError> {
        validate_interaction_id(&self.id)?;
        if let Some(data_type) = self.data.as_ref().and_then(|data| data.interaction_type) {
            if data_type != self.interaction_type {
                return Err(InteractionValidationError::MismatchedInteractionType {
                    top_level: self.interaction_type,
                    data: data_type,
                });
            }
        }
        for (field, value) in [
            ("guild_id", self.guild_id.as_deref()),
            ("channel_id", self.channel_id.as_deref()),
            ("user_openid", self.user_openid.as_deref()),
            ("group_openid", self.group_openid.as_deref()),
            ("group_member_openid", self.group_member_openid.as_deref()),
            ("application_id", self.application_id.as_deref()),
        ] {
            validate_optional_identifier(field, value)?;
        }
        if let Some(scene) = self.scene.as_deref() {
            if scene.trim().is_empty() {
                return Err(InteractionValidationError::EmptyField { field: "scene" });
            }
            if scene
                .chars()
                .any(|character| character.is_control() || character.is_whitespace())
            {
                return Err(InteractionValidationError::InvalidField { field: "scene" });
            }
        }
        DateTime::parse_from_rfc3339(&self.timestamp)
            .map_err(|_| InteractionValidationError::InvalidTimestamp)?;
        Ok(())
    }

    /// Returns whether QQ requires an explicit response for this interaction type.
    pub const fn requires_response(&self) -> bool {
        matches!(self.interaction_type, 11 | 12)
    }
}

/// Optional result code sent when responding to an interaction.
///
/// Omitting `code` produces the official empty JSON object request. The QQ
/// success response, independently, may be either an empty body or `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionResponseRequest {
    /// `0` succeeds; `1..=5` describe the supported failure outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u8>,
}

impl InteractionResponseRequest {
    /// Validates the one-time interaction identifier and optional response code.
    pub fn validate(&self, interaction_id: &str) -> Result<(), InteractionValidationError> {
        validate_interaction_id(interaction_id)?;
        if let Some(code) = self.code {
            if code > 5 {
                return Err(InteractionValidationError::InvalidResponseCode { code });
            }
        }
        Ok(())
    }
}

fn validate_interaction_id(interaction_id: &str) -> Result<(), InteractionValidationError> {
    if interaction_id.trim().is_empty() {
        return Err(InteractionValidationError::EmptyField {
            field: "interaction_id",
        });
    }
    if interaction_id.starts_with("INTERACTION_CREATE:") {
        return Err(InteractionValidationError::PrefixedInteractionId);
    }
    if interaction_id
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(InteractionValidationError::InvalidInteractionId);
    }
    Ok(())
}

fn validate_optional_identifier(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), InteractionValidationError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(InteractionValidationError::EmptyField { field });
    }
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(InteractionValidationError::InvalidField { field });
    }
    Ok(())
}
