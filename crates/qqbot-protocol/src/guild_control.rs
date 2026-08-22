//! Typed QQ guild speaking-control and API-permission models.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Validation failures for guild speaking-control and API-permission requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuildControlValidationError {
    MissingMuteTiming,
    EmptyField { field: &'static str },
    InvalidField { field: &'static str },
    EmptyUserIds,
    InvalidUserId { index: usize },
}

impl fmt::Display for GuildControlValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMuteTiming => formatter
                .write_str("QQ guild mute request requires `mute_end_timestamp` or `mute_seconds`"),
            Self::EmptyField { field } => {
                write!(formatter, "QQ guild control `{field}` must not be empty")
            }
            Self::InvalidField { field } => write!(
                formatter,
                "QQ guild control `{field}` contains an invalid value"
            ),
            Self::EmptyUserIds => {
                formatter.write_str("QQ guild batch mute `user_ids` must not be empty")
            }
            Self::InvalidUserId { index } => write!(
                formatter,
                "QQ guild batch mute `user_ids[{index}]` must be a non-empty identifier"
            ),
        }
    }
}

impl std::error::Error for GuildControlValidationError {}

/// QQ's per-guild message and direct-message sending settings.
///
/// The official model table calls the two switches strings, while both its
/// wire example and Tencent's `botgo` SDK use booleans. Fields remain optional
/// because the page does not publish a required schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMessageSetting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_create_dm: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_push_msg: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_push_max_num: Option<u32>,
}

/// Time selection shared by guild mute requests.
///
/// Both fields use the official string wire format. Supplying both is valid;
/// QQ documents that `mute_end_timestamp` then takes precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMuteRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute_end_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute_seconds: Option<String>,
}

impl GuildMuteRequest {
    pub fn validate(&self) -> Result<(), GuildControlValidationError> {
        if self.mute_end_timestamp.is_none() && self.mute_seconds.is_none() {
            return Err(GuildControlValidationError::MissingMuteTiming);
        }
        validate_decimal_string("mute_end_timestamp", self.mute_end_timestamp.as_deref())?;
        validate_decimal_string("mute_seconds", self.mute_seconds.as_deref())
    }
}

/// Batch-member mute request sharing the guild-wide `/mute` path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMembersMuteRequest {
    #[serde(flatten)]
    pub timing: GuildMuteRequest,
    pub user_ids: Vec<String>,
}

impl GuildMembersMuteRequest {
    pub fn validate(&self) -> Result<(), GuildControlValidationError> {
        self.timing.validate()?;
        if self.user_ids.is_empty() {
            return Err(GuildControlValidationError::EmptyUserIds);
        }
        for (index, user_id) in self.user_ids.iter().enumerate() {
            if !is_valid_identifier(user_id) {
                return Err(GuildControlValidationError::InvalidUserId { index });
            }
        }
        Ok(())
    }
}

/// IDs for which QQ actually applied a batch-member mute request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMembersMuteResponse {
    pub user_ids: Vec<String>,
}

/// HTTP method and templated path uniquely identifying a guild `OpenAPI`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildApiIdentify {
    pub path: String,
    pub method: String,
}

/// Strict request-side identifier for a quota-consuming permission demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuildApiPermissionDemandIdentify {
    pub path: String,
    pub method: String,
}

impl GuildApiPermissionDemandIdentify {
    fn validate(&self) -> Result<(), GuildControlValidationError> {
        validate_token("api_identify.path", &self.path)?;
        if !self.path.starts_with('/') {
            return Err(GuildControlValidationError::InvalidField {
                field: "api_identify.path",
            });
        }
        validate_token("api_identify.method", &self.method)
    }
}

/// One API permission exposed by QQ for a guild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildApiPermission {
    pub path: String,
    pub method: String,
    pub desc: String,
    pub auth_status: i64,
}

/// Guild API permissions currently visible to the bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildApiPermissionList {
    pub apis: Vec<GuildApiPermission>,
}

/// Request for QQ to send an API-permission authorization demand message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildApiPermissionDemandRequest {
    pub channel_id: String,
    pub api_identify: GuildApiPermissionDemandIdentify,
    pub desc: String,
}

impl GuildApiPermissionDemandRequest {
    pub fn validate(&self) -> Result<(), GuildControlValidationError> {
        validate_token("channel_id", &self.channel_id)?;
        self.api_identify.validate()?;
        validate_description("desc", &self.desc)
    }
}

/// Metadata returned after QQ sends an API-permission authorization demand.
///
/// QQ does not return the authorization URL itself, so the model intentionally
/// contains no invented `url` or `link` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildApiPermissionDemand {
    pub guild_id: String,
    pub channel_id: String,
    pub api_identify: GuildApiIdentify,
    pub title: String,
    pub desc: String,
}

fn validate_decimal_string(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), GuildControlValidationError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty() {
        return Err(GuildControlValidationError::EmptyField { field });
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GuildControlValidationError::InvalidField { field });
    }
    Ok(())
}

fn validate_token(field: &'static str, value: &str) -> Result<(), GuildControlValidationError> {
    if value.trim().is_empty() {
        return Err(GuildControlValidationError::EmptyField { field });
    }
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(GuildControlValidationError::InvalidField { field });
    }
    Ok(())
}

fn validate_description(
    field: &'static str,
    value: &str,
) -> Result<(), GuildControlValidationError> {
    if value.trim().is_empty() {
        return Err(GuildControlValidationError::EmptyField { field });
    }
    Ok(())
}

fn is_valid_identifier(value: &str) -> bool {
    !value.trim().is_empty()
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}
