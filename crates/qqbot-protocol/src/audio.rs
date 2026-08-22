//! Typed QQ audio-channel requests and dispatch payloads.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioValidationError {
    EmptyField { field: &'static str },
    InvalidStatus { value: u32 },
    MissingAudioUrl,
    UnexpectedPlaybackField { field: &'static str },
    InvalidAudioUrl,
}

impl fmt::Display for AudioValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(formatter, "QQ audio `{field}` must not be empty"),
            Self::InvalidStatus { value } => {
                write!(
                    formatter,
                    "QQ audio status must be between 0 and 3, got {value}"
                )
            }
            Self::MissingAudioUrl => {
                formatter.write_str("QQ audio start request requires audio_url")
            }
            Self::UnexpectedPlaybackField { field } => write!(
                formatter,
                "QQ audio `{field}` is only allowed when starting playback"
            ),
            Self::InvalidAudioUrl => formatter.write_str(
                "QQ audio `audio_url` must be an HTTP(S) URL with a host and no whitespace",
            ),
        }
    }
}

impl std::error::Error for AudioValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AudioStatus(u32);

impl AudioStatus {
    pub const START: Self = Self(0);
    pub const PAUSE: Self = Self(1);
    pub const RESUME: Self = Self(2);
    pub const STOP: Self = Self(3);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioControlRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub status: AudioStatus,
}

impl AudioControlRequest {
    pub fn start(audio_url: impl Into<String>, text: Option<String>) -> Self {
        Self {
            audio_url: Some(audio_url.into()),
            text,
            status: AudioStatus::START,
        }
    }

    pub const fn pause() -> Self {
        Self {
            audio_url: None,
            text: None,
            status: AudioStatus::PAUSE,
        }
    }

    pub const fn resume() -> Self {
        Self {
            audio_url: None,
            text: None,
            status: AudioStatus::RESUME,
        }
    }

    pub const fn stop() -> Self {
        Self {
            audio_url: None,
            text: None,
            status: AudioStatus::STOP,
        }
    }

    pub fn validate(&self) -> Result<(), AudioValidationError> {
        match self.status {
            AudioStatus::START => {
                let Some(audio_url) = self.audio_url.as_deref() else {
                    return Err(AudioValidationError::MissingAudioUrl);
                };
                validate_audio_url(audio_url)?;
            }
            AudioStatus::PAUSE | AudioStatus::RESUME | AudioStatus::STOP => {
                if self.audio_url.is_some() {
                    return Err(AudioValidationError::UnexpectedPlaybackField {
                        field: "audio_url",
                    });
                }
                if self.text.is_some() {
                    return Err(AudioValidationError::UnexpectedPlaybackField { field: "text" });
                }
            }
            other => {
                return Err(AudioValidationError::InvalidStatus {
                    value: other.value(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioActionEvent {
    pub guild_id: String,
    pub channel_id: String,
    #[serde(default)]
    pub audio_url: String,
    #[serde(default)]
    pub text: String,
}

impl AudioActionEvent {
    pub fn validate(&self) -> Result<(), AudioValidationError> {
        validate_non_empty("guild_id", &self.guild_id)?;
        validate_non_empty("channel_id", &self.channel_id)?;
        if !self.audio_url.is_empty() {
            validate_audio_url(&self.audio_url)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AudioOrLiveChannelType(u32);

impl AudioOrLiveChannelType {
    pub const AUDIO: Self = Self(2);
    pub const LIVE: Self = Self(5);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioOrLiveChannelMemberEvent {
    pub guild_id: String,
    pub channel_id: String,
    pub channel_type: AudioOrLiveChannelType,
    pub user_id: String,
}

impl AudioOrLiveChannelMemberEvent {
    pub fn validate(&self) -> Result<(), AudioValidationError> {
        validate_non_empty("guild_id", &self.guild_id)?;
        validate_non_empty("channel_id", &self.channel_id)?;
        validate_non_empty("user_id", &self.user_id)
    }
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), AudioValidationError> {
    if value.trim().is_empty() {
        Err(AudioValidationError::EmptyField { field })
    } else {
        Ok(())
    }
}

fn validate_audio_url(value: &str) -> Result<(), AudioValidationError> {
    validate_non_empty("audio_url", value)?;
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(AudioValidationError::InvalidAudioUrl);
    }
    let url = Url::parse(value).map_err(|_| AudioValidationError::InvalidAudioUrl)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(AudioValidationError::InvalidAudioUrl);
    }
    Ok(())
}
