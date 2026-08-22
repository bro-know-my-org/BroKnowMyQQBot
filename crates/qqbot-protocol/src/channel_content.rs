//! Typed QQ guild announcements, channel pins, and channel schedules.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::guild::GuildMember;

const MAX_RECOMMENDED_CHANNELS: usize = 3;
const MAX_SCHEDULE_DURATION_MILLIS: u64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelContentValidationError {
    EmptyField { field: &'static str },
    InvalidAnnouncementType { value: u32 },
    InvalidAnnouncementShape,
    RecommendedChannelCount { count: usize },
    InvalidRecommendedChannel { index: usize, field: &'static str },
    InvalidEpochMillis { field: &'static str },
    InvalidRemindType { value: u32 },
    ScheduleEndsBeforeStart,
    ScheduleDurationTooLong,
    EmptyScheduleUpdate,
}

impl fmt::Display for ChannelContentValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => {
                write!(formatter, "QQ channel content `{field}` must not be empty")
            }
            Self::InvalidAnnouncementType { value } => {
                write!(
                    formatter,
                    "QQ announcement type must be 0 or 1, got {value}"
                )
            }
            Self::InvalidAnnouncementShape => formatter
                .write_str("QQ announcement fields do not match the selected announcement type"),
            Self::RecommendedChannelCount { count } => write!(
                formatter,
                "QQ recommended-channel announcement must contain between 1 and {MAX_RECOMMENDED_CHANNELS} channels, got {count}"
            ),
            Self::InvalidRecommendedChannel { index, field } => write!(
                formatter,
                "QQ recommended channel at index {index} has an empty `{field}`"
            ),
            Self::InvalidEpochMillis { field } => write!(
                formatter,
                "QQ schedule `{field}` must be an unsigned decimal Unix epoch millisecond string"
            ),
            Self::InvalidRemindType { value } => {
                write!(
                    formatter,
                    "QQ schedule remind type must be between 0 and 5, got {value}"
                )
            }
            Self::ScheduleEndsBeforeStart => {
                formatter.write_str("QQ schedule must not end before it starts")
            }
            Self::ScheduleDurationTooLong => {
                formatter.write_str("QQ schedule duration must not exceed seven days")
            }
            Self::EmptyScheduleUpdate => {
                formatter.write_str("QQ schedule update must contain at least one field")
            }
        }
    }
}

impl std::error::Error for ChannelContentValidationError {}

/// QQ announcement kind. Requests currently accept only `MEMBER` and
/// `WELCOME`; responses retain unknown integers for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnnouncementType(u32);

impl AnnouncementType {
    pub const MEMBER: Self = Self(0);
    pub const WELCOME: Self = Self(1);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecommendChannel {
    pub channel_id: String,
    pub introduce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGuildAnnouncementRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    pub announces_type: AnnouncementType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommend_channels: Option<Vec<RecommendChannel>>,
}

impl CreateGuildAnnouncementRequest {
    pub fn message(message_id: impl Into<String>, channel_id: impl Into<String>) -> Self {
        Self {
            message_id: Some(message_id.into()),
            channel_id: Some(channel_id.into()),
            announces_type: AnnouncementType::MEMBER,
            recommend_channels: None,
        }
    }

    pub fn recommended(channels: Vec<RecommendChannel>) -> Self {
        Self {
            message_id: None,
            channel_id: None,
            announces_type: AnnouncementType::WELCOME,
            recommend_channels: Some(channels),
        }
    }

    pub fn validate(&self) -> Result<(), ChannelContentValidationError> {
        match self.announces_type {
            AnnouncementType::MEMBER => {
                validate_optional_required("message_id", self.message_id.as_deref())?;
                validate_optional_required("channel_id", self.channel_id.as_deref())?;
                if self
                    .recommend_channels
                    .as_ref()
                    .is_some_and(|channels| !channels.is_empty())
                {
                    return Err(ChannelContentValidationError::InvalidAnnouncementShape);
                }
            }
            AnnouncementType::WELCOME => {
                if self
                    .message_id
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                    || self
                        .channel_id
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
                {
                    return Err(ChannelContentValidationError::InvalidAnnouncementShape);
                }
                let channels = self.recommend_channels.as_deref().unwrap_or_default();
                if !(1..=MAX_RECOMMENDED_CHANNELS).contains(&channels.len()) {
                    return Err(ChannelContentValidationError::RecommendedChannelCount {
                        count: channels.len(),
                    });
                }
                for (index, channel) in channels.iter().enumerate() {
                    for (field, value) in [
                        ("channel_id", channel.channel_id.as_str()),
                        ("introduce", channel.introduce.as_str()),
                    ] {
                        if !non_empty(value) {
                            return Err(ChannelContentValidationError::InvalidRecommendedChannel {
                                index,
                                field,
                            });
                        }
                    }
                }
            }
            other => {
                return Err(ChannelContentValidationError::InvalidAnnouncementType {
                    value: other.value(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildAnnouncement {
    pub guild_id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub message_id: String,
    pub announces_type: AnnouncementType,
    #[serde(default)]
    pub recommend_channels: Vec<RecommendChannel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinsMessage {
    pub guild_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub message_ids: Vec<String>,
}

/// A validated decimal Unix epoch millisecond string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EpochMillis(String);

impl EpochMillis {
    pub fn new(value: impl Into<String>) -> Result<Self, ChannelContentValidationError> {
        let value = value.into();
        validate_epoch_millis("timestamp", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn value(&self) -> u64 {
        self.0
            .parse()
            .expect("EpochMillis constructor and decoder preserve the numeric invariant")
    }
}

impl Serialize for EpochMillis {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for EpochMillis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        validate_epoch_millis("timestamp", &value).map_err(de::Error::custom)?;
        Ok(Self(value))
    }
}

/// QQ schedule reminder type, serialized as a decimal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScheduleRemindType(u32);

impl ScheduleRemindType {
    pub const NONE: Self = Self(0);
    pub const AT_START: Self = Self(1);
    pub const FIVE_MINUTES: Self = Self(2);
    pub const FIFTEEN_MINUTES: Self = Self(3);
    pub const THIRTY_MINUTES: Self = Self(4);
    pub const SIXTY_MINUTES: Self = Self(5);

    pub const fn new(value: u32) -> Result<Self, ChannelContentValidationError> {
        if value <= 5 {
            Ok(Self(value))
        } else {
            Err(ChannelContentValidationError::InvalidRemindType { value })
        }
    }

    pub const fn value(self) -> u32 {
        self.0
    }

    fn validate(self) -> Result<(), ChannelContentValidationError> {
        if self.0 <= 5 {
            Ok(())
        } else {
            Err(ChannelContentValidationError::InvalidRemindType { value: self.0 })
        }
    }
}

impl Serialize for ScheduleRemindType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for ScheduleRemindType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(de::Error::custom("expected a decimal schedule remind type"));
        }
        let value = value
            .parse::<u32>()
            .map_err(|_| de::Error::custom("expected a decimal schedule remind type"))?;
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSchedulesQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSchedule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub start_timestamp: EpochMillis,
    pub end_timestamp: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_channel_id: Option<String>,
    pub remind_type: ScheduleRemindType,
}

impl CreateSchedule {
    pub fn validate(&self) -> Result<(), ChannelContentValidationError> {
        validate_non_empty("schedule.name", &self.name)?;
        validate_optional_present("schedule.jump_channel_id", self.jump_channel_id.as_deref())?;
        self.remind_type.validate()?;
        validate_schedule_range(&self.start_timestamp, &self.end_timestamp)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateScheduleRequest {
    pub schedule: CreateSchedule,
}

impl CreateScheduleRequest {
    pub fn validate(&self) -> Result<(), ChannelContentValidationError> {
        self.schedule.validate()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSchedule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_timestamp: Option<EpochMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_timestamp: Option<EpochMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_channel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remind_type: Option<ScheduleRemindType>,
}

impl UpdateSchedule {
    pub fn validate(&self) -> Result<(), ChannelContentValidationError> {
        if self.name.is_none()
            && self.description.is_none()
            && self.start_timestamp.is_none()
            && self.end_timestamp.is_none()
            && self.jump_channel_id.is_none()
            && self.remind_type.is_none()
        {
            return Err(ChannelContentValidationError::EmptyScheduleUpdate);
        }
        validate_optional_present("schedule.name", self.name.as_deref())?;
        validate_optional_present("schedule.jump_channel_id", self.jump_channel_id.as_deref())?;
        if let Some(remind_type) = self.remind_type {
            remind_type.validate()?;
        }
        if let (Some(start), Some(end)) = (&self.start_timestamp, &self.end_timestamp) {
            validate_schedule_range(start, end)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateScheduleRequest {
    pub schedule: UpdateSchedule,
}

impl UpdateScheduleRequest {
    pub fn validate(&self) -> Result<(), ChannelContentValidationError> {
        self.schedule.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub start_timestamp: EpochMillis,
    pub end_timestamp: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<GuildMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_channel_id: Option<String>,
    pub remind_type: ScheduleRemindType,
}

fn validate_optional_required(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), ChannelContentValidationError> {
    value.map_or(
        Err(ChannelContentValidationError::EmptyField { field }),
        |value| validate_non_empty(field, value),
    )
}

fn validate_optional_present(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), ChannelContentValidationError> {
    value.map_or(Ok(()), |value| validate_non_empty(field, value))
}

fn validate_non_empty(
    field: &'static str,
    value: &str,
) -> Result<(), ChannelContentValidationError> {
    if non_empty(value) {
        Ok(())
    } else {
        Err(ChannelContentValidationError::EmptyField { field })
    }
}

fn non_empty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn validate_epoch_millis(
    field: &'static str,
    value: &str,
) -> Result<u64, ChannelContentValidationError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ChannelContentValidationError::InvalidEpochMillis { field });
    }
    value
        .parse()
        .map_err(|_| ChannelContentValidationError::InvalidEpochMillis { field })
}

fn validate_schedule_range(
    start: &EpochMillis,
    end: &EpochMillis,
) -> Result<(), ChannelContentValidationError> {
    let start = validate_epoch_millis("schedule.start_timestamp", start.as_str())?;
    let end = validate_epoch_millis("schedule.end_timestamp", end.as_str())?;
    let Some(duration) = end.checked_sub(start) else {
        return Err(ChannelContentValidationError::ScheduleEndsBeforeStart);
    };
    if duration > MAX_SCHEDULE_DURATION_MILLIS {
        return Err(ChannelContentValidationError::ScheduleDurationTooLong);
    }
    Ok(())
}
