//! Validated plugin identifiers.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginIdError {
    #[error("plugin ID must contain between 5 and 128 ASCII characters")]
    InvalidLength,
    #[error("plugin ID must contain only ASCII characters")]
    NonAscii,
    #[error("plugin ID must contain at least three dot-separated segments")]
    TooFewSegments,
    #[error("plugin ID segment `{0}` is invalid")]
    InvalidSegment(String),
}

impl PluginId {
    pub fn new(value: impl Into<String>) -> Result<Self, PluginIdError> {
        let value = value.into();
        if !value.is_ascii() {
            return Err(PluginIdError::NonAscii);
        }
        if !(5..=128).contains(&value.len()) {
            return Err(PluginIdError::InvalidLength);
        }
        let segments: Vec<_> = value.split('.').collect();
        if segments.len() < 3 {
            return Err(PluginIdError::TooFewSegments);
        }
        for segment in segments {
            let mut characters = segment.chars();
            let Some(first) = characters.next() else {
                return Err(PluginIdError::InvalidSegment(segment.to_owned()));
            };
            if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
                return Err(PluginIdError::InvalidSegment(segment.to_owned()));
            }
            if !characters.all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '_' | '-')
            }) {
                return Err(PluginIdError::InvalidSegment(segment.to_owned()));
            }
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("PluginId").field(&self.0).finish()
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for PluginId {
    type Err = PluginIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for PluginId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PluginId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{PluginId, PluginIdError};

    #[test]
    fn validates_reverse_domain_ids() {
        assert_eq!(
            PluginId::new("dev.bkm.ping").unwrap().as_str(),
            "dev.bkm.ping"
        );
        assert_eq!(
            PluginId::new("plugin").unwrap_err(),
            PluginIdError::TooFewSegments
        );
        assert!(PluginId::new("Dev.bkm.ping").is_err());
        assert!(PluginId::new("dev..ping").is_err());
    }
}
