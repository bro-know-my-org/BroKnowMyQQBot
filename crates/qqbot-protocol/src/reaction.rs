//! Typed QQ channel message reaction models.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionValidationError {
    EmptyField { field: &'static str },
    InvalidEmojiType { emoji_type: u32 },
    InvalidTargetType { target_type: u32 },
    PageLimitOutOfRange { limit: u8 },
    LimitWithCookie,
}

impl fmt::Display for ReactionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => {
                write!(formatter, "QQ reaction `{field}` must not be empty")
            }
            Self::InvalidEmojiType { emoji_type } => write!(
                formatter,
                "QQ reaction emoji type must be 1 or 2, got {emoji_type}"
            ),
            Self::InvalidTargetType { target_type } => write!(
                formatter,
                "QQ reaction target type must be between 0 and 3, got {target_type}"
            ),
            Self::PageLimitOutOfRange { limit } => write!(
                formatter,
                "QQ reaction user page limit must be between 1 and 50, got {limit}"
            ),
            Self::LimitWithCookie => formatter
                .write_str("QQ reaction user page limit may only be set on the first request"),
        }
    }
}

impl std::error::Error for ReactionValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionEmoji {
    pub id: String,
    #[serde(rename = "type")]
    pub emoji_type: u32,
}

impl ReactionEmoji {
    pub fn validate(&self) -> Result<(), ReactionValidationError> {
        validate_non_empty("emoji.id", &self.id)?;
        if !matches!(self.emoji_type, 1 | 2) {
            return Err(ReactionValidationError::InvalidEmojiType {
                emoji_type: self.emoji_type,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionUsersRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u8>,
}

impl ReactionUsersRequest {
    pub fn validate(&self) -> Result<(), ReactionValidationError> {
        if let Some(cookie) = self.cookie.as_deref() {
            validate_non_empty("cookie", cookie)?;
            if self.limit.is_some() {
                return Err(ReactionValidationError::LimitWithCookie);
            }
        }
        if let Some(limit) = self.limit {
            if !(1..=50).contains(&limit) {
                return Err(ReactionValidationError::PageLimitOutOfRange { limit });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionUser {
    pub id: String,
    pub username: String,
    pub avatar: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionUsersPage {
    pub users: Vec<ReactionUser>,
    pub cookie: String,
    pub is_end: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionTarget {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReactionEvent {
    pub user_id: String,
    pub emoji: ReactionEmoji,
    pub channel_id: String,
    pub guild_id: String,
    pub target: ReactionTarget,
}

impl MessageReactionEvent {
    pub fn validate(&self) -> Result<(), ReactionValidationError> {
        validate_non_empty("user_id", &self.user_id)?;
        self.emoji.validate()?;
        validate_non_empty("channel_id", &self.channel_id)?;
        validate_non_empty("guild_id", &self.guild_id)?;
        validate_non_empty("target.id", &self.target.id)?;
        if self.target.target_type > 3 {
            return Err(ReactionValidationError::InvalidTargetType {
                target_type: self.target.target_type,
            });
        }
        Ok(())
    }
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), ReactionValidationError> {
    if value.trim().is_empty() {
        Err(ReactionValidationError::EmptyField { field })
    } else {
        Ok(())
    }
}
