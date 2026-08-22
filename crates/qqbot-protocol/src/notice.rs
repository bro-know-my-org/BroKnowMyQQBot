//! Typed QQ message-deletion, subscription-status, and audit notice models.

use std::fmt;

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::message::{MessageAuthor, QqMessage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeValidationError {
    EmptyField { field: &'static str },
    InvalidTimestamp { field: &'static str },
    MissingSubscriptionTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAuditOutcome {
    Pass,
    Reject,
}

impl fmt::Display for NoticeValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => {
                write!(formatter, "QQ notice `{field}` must not be empty")
            }
            Self::InvalidTimestamp { field } => {
                write!(
                    formatter,
                    "QQ notice `{field}` must be an RFC 3339 timestamp"
                )
            }
            Self::MissingSubscriptionTarget => {
                formatter.write_str("QQ subscription-status notice requires group_openid or openid")
            }
        }
    }
}

impl std::error::Error for NoticeValidationError {}

/// Payload shared by `MESSAGE_DELETE`, `PUBLIC_MESSAGE_DELETE`, and
/// `DIRECT_MESSAGE_DELETE`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageDeleteEvent {
    pub message: QqMessage,
    pub op_user: MessageAuthor,
}

impl MessageDeleteEvent {
    pub fn validate(&self) -> Result<(), NoticeValidationError> {
        validate_non_empty("message.id", &self.message.id)?;
        validate_optional_non_empty("message.guild_id", self.message.guild_id.as_deref())?;
        validate_optional_non_empty("message.channel_id", self.message.channel_id.as_deref())?;
        validate_optional_non_empty("op_user.id", self.op_user.id.as_deref())
    }
}

/// Payload shared by `MESSAGE_AUDIT_PASS` and `MESSAGE_AUDIT_REJECT`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageAuditEvent {
    pub audit_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub guild_id: String,
    pub channel_id: String,
    pub audit_time: String,
    pub create_time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq_in_channel: Option<String>,
}

impl MessageAuditEvent {
    pub fn validate(&self, outcome: MessageAuditOutcome) -> Result<(), NoticeValidationError> {
        for (field, value) in [
            ("audit_id", self.audit_id.as_str()),
            ("guild_id", self.guild_id.as_str()),
            ("channel_id", self.channel_id.as_str()),
            ("audit_time", self.audit_time.as_str()),
            ("create_time", self.create_time.as_str()),
        ] {
            validate_non_empty(field, value)?;
        }
        for (field, value) in [
            ("audit_time", self.audit_time.as_str()),
            ("create_time", self.create_time.as_str()),
        ] {
            DateTime::parse_from_rfc3339(value)
                .map_err(|_| NoticeValidationError::InvalidTimestamp { field })?;
        }
        if matches!(outcome, MessageAuditOutcome::Pass) {
            validate_optional_non_empty("message_id", self.message_id.as_deref())?;
        }
        Ok(())
    }
}

/// Payload for `SUBSCRIBE_MESSAGE_STATUS`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscribeMessageStatusEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_openid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openid: Option<String>,
    pub result: Vec<SubscribeMessageTemplateResult>,
}

impl SubscribeMessageStatusEvent {
    pub fn validate(&self) -> Result<(), NoticeValidationError> {
        if [self.group_openid.as_deref(), self.openid.as_deref()]
            .into_iter()
            .flatten()
            .all(|value| value.trim().is_empty())
        {
            return Err(NoticeValidationError::MissingSubscriptionTarget);
        }
        validate_optional_present_non_empty("group_openid", self.group_openid.as_deref())?;
        validate_optional_present_non_empty("openid", self.openid.as_deref())?;
        for result in &self.result {
            validate_non_empty("result.custom_template_id", &result.custom_template_id)?;
            validate_non_empty("result.subscribe_id", &result.subscribe_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscribeMessageTemplateResult {
    pub template_id: u64,
    pub custom_template_id: String,
    pub op: SubscriptionOperation,
    pub subscribe_id: String,
    pub subscribe_ts: u64,
    pub update_ts: u64,
}

/// A subscription authorization operation. Unknown values are retained for
/// forward compatibility.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct SubscriptionOperation(u32);

impl SubscriptionOperation {
    pub const ALLOW: Self = Self(1);
    pub const REJECT: Self = Self(2);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), NoticeValidationError> {
    if value.trim().is_empty() {
        Err(NoticeValidationError::EmptyField { field })
    } else {
        Ok(())
    }
}

fn validate_optional_non_empty(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), NoticeValidationError> {
    value.map_or(Err(NoticeValidationError::EmptyField { field }), |value| {
        validate_non_empty(field, value)
    })
}

fn validate_optional_present_non_empty(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), NoticeValidationError> {
    value.map_or(Ok(()), |value| validate_non_empty(field, value))
}
