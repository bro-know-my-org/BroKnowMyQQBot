//! Typed QQ private-domain forum requests, responses, and dispatch payloads.

use std::fmt;

use chrono::DateTime;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForumValidationError {
    EmptyField { field: &'static str },
    InvalidFormat { value: u32 },
    InvalidJsonContent,
    InvalidUnixSeconds { field: &'static str },
    InvalidTimestamp { field: &'static str },
    InvalidContentShape,
    InvalidListFinish { value: u32 },
}

impl fmt::Display for ForumValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(formatter, "QQ forum `{field}` must not be empty"),
            Self::InvalidFormat { value } => {
                write!(
                    formatter,
                    "QQ forum format must be between 1 and 4, got {value}"
                )
            }
            Self::InvalidJsonContent => {
                formatter.write_str("QQ forum format 4 content must contain valid JSON text")
            }
            Self::InvalidUnixSeconds { field } => write!(
                formatter,
                "QQ forum `{field}` must be an unsigned decimal Unix second string"
            ),
            Self::InvalidTimestamp { field } => {
                write!(
                    formatter,
                    "QQ forum `{field}` must be an RFC 3339 timestamp"
                )
            }
            Self::InvalidContentShape => {
                formatter.write_str("QQ forum content must be a string, object, or array")
            }
            Self::InvalidListFinish { value } => {
                write!(formatter, "QQ forum is_finish must be 0 or 1, got {value}")
            }
        }
    }
}

impl std::error::Error for ForumValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForumFormat(u32);

impl ForumFormat {
    pub const TEXT: Self = Self(1);
    pub const HTML: Self = Self(2);
    pub const MARKDOWN: Self = Self(3);
    pub const JSON: Self = Self(4);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateForumThreadRequest {
    pub title: String,
    pub content: String,
    pub format: ForumFormat,
}

impl CreateForumThreadRequest {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_non_empty("title", &self.title)?;
        validate_non_empty("content", &self.content)?;
        if !(1..=4).contains(&self.format.value()) {
            return Err(ForumValidationError::InvalidFormat {
                value: self.format.value(),
            });
        }
        if self.format == ForumFormat::JSON && serde_json::from_str::<Value>(&self.content).is_err()
        {
            return Err(ForumValidationError::InvalidJsonContent);
        }
        Ok(())
    }
}

/// A validated unsigned decimal Unix second string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForumCreateTime(String);

impl ForumCreateTime {
    pub fn new(value: impl Into<String>) -> Result<Self, ForumValidationError> {
        let value = value.into();
        validate_unix_seconds("create_time", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn value(&self) -> u64 {
        self.0
            .parse()
            .expect("ForumCreateTime preserves its numeric invariant")
    }
}

impl Serialize for ForumCreateTime {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ForumCreateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        validate_unix_seconds("create_time", &value).map_err(de::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPublishTask {
    pub task_id: String,
    pub create_time: ForumCreateTime,
}

impl ForumPublishTask {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_non_empty("task_id", &self.task_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForumContent(Value);

impl ForumContent {
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    pub fn as_str(&self) -> Option<&str> {
        self.0.as_str()
    }
}

impl TryFrom<Value> for ForumContent {
    type Error = ForumValidationError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        if value.is_string() || value.is_array() || value.is_object() {
            Ok(Self(value))
        } else {
            Err(ForumValidationError::InvalidContentShape)
        }
    }
}

impl Serialize for ForumContent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ForumContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::try_from(value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumThreadInfo {
    pub thread_id: String,
    pub title: ForumContent,
    pub content: ForumContent,
    pub date_time: String,
}

impl ForumThreadInfo {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_non_empty("thread_info.thread_id", &self.thread_id)?;
        validate_rfc3339("thread_info.date_time", &self.date_time)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumThread {
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub thread_info: ForumThreadInfo,
}

impl ForumThread {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_outer(
            self.guild_id.as_str(),
            self.channel_id.as_str(),
            self.author_id.as_str(),
        )?;
        self.thread_info.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForumListFinish(u32);

impl ForumListFinish {
    pub const MORE: Self = Self(0);
    pub const FINISHED: Self = Self(1);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumThreadList {
    pub threads: Vec<ForumThread>,
    pub is_finish: ForumListFinish,
}

impl ForumThreadList {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        if !matches!(
            self.is_finish,
            ForumListFinish::MORE | ForumListFinish::FINISHED
        ) {
            return Err(ForumValidationError::InvalidListFinish {
                value: self.is_finish.value(),
            });
        }
        self.threads.iter().try_for_each(ForumThread::validate)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumThreadDetail {
    pub thread: ForumThread,
}

impl ForumThreadDetail {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        self.thread.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPostInfo {
    pub thread_id: String,
    pub post_id: String,
    pub content: ForumContent,
    pub date_time: String,
}

impl ForumPostInfo {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_non_empty("post_info.thread_id", &self.thread_id)?;
        validate_non_empty("post_info.post_id", &self.post_id)?;
        validate_rfc3339("post_info.date_time", &self.date_time)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPostEvent {
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub post_info: ForumPostInfo,
}

impl ForumPostEvent {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_outer(
            self.guild_id.as_str(),
            self.channel_id.as_str(),
            self.author_id.as_str(),
        )?;
        self.post_info.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumReplyInfo {
    pub thread_id: String,
    pub post_id: String,
    pub reply_id: String,
    pub content: ForumContent,
    pub date_time: String,
}

impl ForumReplyInfo {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_non_empty("reply_info.thread_id", &self.thread_id)?;
        validate_non_empty("reply_info.post_id", &self.post_id)?;
        validate_non_empty("reply_info.reply_id", &self.reply_id)?;
        validate_rfc3339("reply_info.date_time", &self.date_time)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumReplyEvent {
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub reply_info: ForumReplyInfo,
}

impl ForumReplyEvent {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_outer(
            self.guild_id.as_str(),
            self.channel_id.as_str(),
            self.author_id.as_str(),
        )?;
        self.reply_info.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForumAuditType(u32);

impl ForumAuditType {
    pub const THREAD: Self = Self(1);
    pub const POST: Self = Self(2);
    pub const REPLY: Self = Self(3);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForumAuditResult(u32);

impl ForumAuditResult {
    pub const SUCCESS: Self = Self(0);
    pub const FAILED: Self = Self(1);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForumPublishAuditEvent {
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub post_id: String,
    #[serde(default)]
    pub reply_id: String,
    #[serde(rename = "type")]
    pub audit_type: ForumAuditType,
    pub result: ForumAuditResult,
    #[serde(default)]
    pub err_msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_time: Option<String>,
}

impl ForumPublishAuditEvent {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_outer(
            self.guild_id.as_str(),
            self.channel_id.as_str(),
            self.author_id.as_str(),
        )?;
        match self.audit_type {
            ForumAuditType::THREAD => validate_non_empty("thread_id", &self.thread_id)?,
            ForumAuditType::POST => {
                validate_non_empty("thread_id", &self.thread_id)?;
                validate_non_empty("post_id", &self.post_id)?;
            }
            ForumAuditType::REPLY => {
                validate_non_empty("thread_id", &self.thread_id)?;
                validate_non_empty("post_id", &self.post_id)?;
                validate_non_empty("reply_id", &self.reply_id)?;
            }
            _ => {}
        }
        for (field, value) in [
            ("thread_id", self.thread_id.as_str()),
            ("post_id", self.post_id.as_str()),
            ("reply_id", self.reply_id.as_str()),
        ] {
            if !value.is_empty() {
                validate_non_empty(field, value)?;
            }
        }
        validate_optional_non_empty("task_id", self.task_id.as_deref())?;
        if let Some(date_time) = self.date_time.as_deref() {
            validate_rfc3339("date_time", date_time)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenForumEvent {
    pub guild_id: String,
    pub channel_id: String,
    pub author_id: String,
}

impl OpenForumEvent {
    pub fn validate(&self) -> Result<(), ForumValidationError> {
        validate_outer(
            self.guild_id.as_str(),
            self.channel_id.as_str(),
            self.author_id.as_str(),
        )
    }
}

fn validate_outer(
    guild_id: &str,
    channel_id: &str,
    author_id: &str,
) -> Result<(), ForumValidationError> {
    validate_non_empty("guild_id", guild_id)?;
    validate_non_empty("channel_id", channel_id)?;
    validate_non_empty("author_id", author_id)
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), ForumValidationError> {
    if value.trim().is_empty() {
        Err(ForumValidationError::EmptyField { field })
    } else {
        Ok(())
    }
}

fn validate_optional_non_empty(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), ForumValidationError> {
    value.map_or(Ok(()), |value| validate_non_empty(field, value))
}

fn validate_rfc3339(field: &'static str, value: &str) -> Result<(), ForumValidationError> {
    DateTime::parse_from_rfc3339(value)
        .map(|_| ())
        .map_err(|_| ForumValidationError::InvalidTimestamp { field })
}

fn validate_unix_seconds(field: &'static str, value: &str) -> Result<u64, ForumValidationError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ForumValidationError::InvalidUnixSeconds { field });
    }
    value
        .parse()
        .map_err(|_| ForumValidationError::InvalidUnixSeconds { field })
}
