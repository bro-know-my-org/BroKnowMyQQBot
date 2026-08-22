//! Typed QQ C2C streaming-message and chunked-media upload contracts.

use std::{collections::BTreeSet, fmt};

use chrono::DateTime;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};
use serde_json::{Map, Value};
use url::Url;

use crate::MediaFileType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamUploadValidationError {
    EmptyField {
        field: &'static str,
    },
    InvalidStreamMode {
        value: String,
    },
    InvalidStreamState {
        value: u32,
    },
    InvalidContentType {
        value: String,
    },
    ConflictingReplyReference,
    MissingReplyReference,
    InvalidStreamSequence,
    InvalidTimestamp {
        field: &'static str,
    },
    InvalidDecimalBytes {
        field: &'static str,
    },
    ZeroByteSize {
        field: &'static str,
    },
    InvalidDigest {
        field: &'static str,
    },
    UnsupportedFileType,
    InvalidPresignedUrl,
    InvalidPresignedDestination,
    EmptyParts,
    ZeroConcurrency,
    PartSizeMismatch {
        expected: u64,
        actual: u64,
    },
    ZeroUploadTimeout,
    InvalidUploadTimeout,
    InvalidUploadId,
    DuplicatePartIndex {
        index: u32,
    },
    InvalidPartSequence,
    InvalidPartBlockSize {
        index: u32,
        expected_max: u64,
        actual: u64,
    },
    PartPlanSizeMismatch {
        expected: u64,
        actual: u64,
    },
    PartPlanSizeOverflow,
}

impl fmt::Display for StreamUploadValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(formatter, "QQ `{field}` must not be empty"),
            Self::InvalidStreamMode { value } => {
                write!(
                    formatter,
                    "QQ stream input_mode must be append or replace, got {value}"
                )
            }
            Self::InvalidStreamState { value } => {
                write!(
                    formatter,
                    "QQ stream input_state must be 1 or 10, got {value}"
                )
            }
            Self::InvalidContentType { value } => {
                write!(
                    formatter,
                    "QQ stream content_type must be text or markdown, got {value}"
                )
            }
            Self::ConflictingReplyReference => {
                formatter.write_str("QQ stream request cannot contain both msg_id and event_id")
            }
            Self::MissingReplyReference => formatter.write_str(
                "QQ non-wakeup stream request requires exactly one of msg_id or event_id",
            ),
            Self::InvalidStreamSequence => formatter.write_str(
                "QQ stream index 0 must omit stream_msg_id and later indexes must include it",
            ),
            Self::InvalidTimestamp { field } => {
                write!(formatter, "QQ `{field}` must be an RFC 3339 timestamp")
            }
            Self::InvalidDecimalBytes { field } => write!(
                formatter,
                "QQ `{field}` must be an unsigned decimal byte-count string"
            ),
            Self::ZeroByteSize { field } => {
                write!(formatter, "QQ `{field}` must be greater than zero")
            }
            Self::InvalidDigest { field } => {
                write!(
                    formatter,
                    "QQ `{field}` must be fixed-length ASCII hexadecimal"
                )
            }
            Self::UnsupportedFileType => formatter.write_str(
                "QQ chunked upload file_type must be 1 (image), 2 (video), 3 (audio), or 4 (file)",
            ),
            Self::InvalidPresignedUrl => formatter.write_str(
                "QQ upload presigned_url must be an absolute HTTPS URL without whitespace",
            ),
            Self::InvalidPresignedDestination => formatter.write_str(
                "QQ upload presigned_url must resolve exclusively to public network addresses",
            ),
            Self::EmptyParts => {
                formatter.write_str("QQ upload response must contain at least one part")
            }
            Self::ZeroConcurrency => {
                formatter.write_str("QQ upload response concurrency must be greater than zero")
            }
            Self::PartSizeMismatch { expected, actual } => write!(
                formatter,
                "QQ upload part requires {expected} bytes, got {actual}"
            ),
            Self::ZeroUploadTimeout => {
                formatter.write_str("QQ upload part timeout must be greater than zero")
            }
            Self::InvalidUploadTimeout => {
                formatter.write_str("QQ upload part timeout is outside the supported range")
            }
            Self::InvalidUploadId => {
                formatter.write_str("QQ upload_id must not contain leading or trailing whitespace")
            }
            Self::DuplicatePartIndex { index } => {
                write!(formatter, "QQ upload response repeats part index {index}")
            }
            Self::InvalidPartSequence => formatter
                .write_str("QQ upload response part indexes must form a contiguous range from 0"),
            Self::InvalidPartBlockSize {
                index,
                expected_max,
                actual,
            } => write!(
                formatter,
                "QQ upload part {index} has block_size {actual}, expected at most {expected_max}"
            ),
            Self::PartPlanSizeMismatch { expected, actual } => write!(
                formatter,
                "QQ upload part plan totals {actual} bytes, expected {expected}"
            ),
            Self::PartPlanSizeOverflow => {
                formatter.write_str("QQ upload part plan byte total overflows u64")
            }
        }
    }
}

impl std::error::Error for StreamUploadValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamInputMode(String);

impl StreamInputMode {
    pub fn append() -> Self {
        Self("append".to_owned())
    }

    pub fn replace() -> Self {
        Self("replace".to_owned())
    }

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamInputState(u32);

impl StreamInputState {
    pub const GENERATING: Self = Self(1);
    pub const FINISHED: Self = Self(10);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamContentType(String);

impl StreamContentType {
    pub fn text() -> Self {
        Self("text".to_owned())
    }

    pub fn markdown() -> Self {
        Self("markdown".to_owned())
    }

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct C2cStreamMessageRequest {
    pub input_mode: StreamInputMode,
    pub input_state: StreamInputState,
    pub index: u32,
    pub content_type: StreamContentType,
    pub content_raw: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_seq: Option<u32>,
    #[serde(default)]
    pub is_wakeup: bool,
}

impl C2cStreamMessageRequest {
    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        if !matches!(self.input_mode.as_str(), "append" | "replace") {
            return Err(StreamUploadValidationError::InvalidStreamMode {
                value: self.input_mode.as_str().to_owned(),
            });
        }
        if !matches!(
            self.input_state,
            StreamInputState::GENERATING | StreamInputState::FINISHED
        ) {
            return Err(StreamUploadValidationError::InvalidStreamState {
                value: self.input_state.value(),
            });
        }
        if !matches!(self.content_type.as_str(), "text" | "markdown") {
            return Err(StreamUploadValidationError::InvalidContentType {
                value: self.content_type.as_str().to_owned(),
            });
        }
        validate_optional_non_empty("event_id", self.event_id.as_deref())?;
        validate_optional_non_empty("msg_id", self.msg_id.as_deref())?;
        validate_optional_non_empty("stream_msg_id", self.stream_msg_id.as_deref())?;
        if self.event_id.is_some() && self.msg_id.is_some() {
            return Err(StreamUploadValidationError::ConflictingReplyReference);
        }
        if !self.is_wakeup && self.event_id.is_none() && self.msg_id.is_none() {
            return Err(StreamUploadValidationError::MissingReplyReference);
        }
        if (self.index == 0) != self.stream_msg_id.is_none() {
            return Err(StreamUploadValidationError::InvalidStreamSequence);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct C2cStreamMessageResponse {
    pub id: String,
    pub timestamp: String,
    #[serde(default)]
    pub ext_info: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remain_msg_len: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Serialize for C2cStreamMessageResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let extra_len = self
            .extra
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "id" | "timestamp" | "ext_info" | "remain_msg_len"
                )
            })
            .count();
        let mut map = serializer.serialize_map(Some(
            3 + usize::from(self.remain_msg_len.is_some()) + extra_len,
        ))?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("timestamp", &self.timestamp)?;
        map.serialize_entry("ext_info", &self.ext_info)?;
        if let Some(remain_msg_len) = self.remain_msg_len {
            map.serialize_entry("remain_msg_len", &remain_msg_len)?;
        }
        for (key, value) in &self.extra {
            if !matches!(
                key.as_str(),
                "id" | "timestamp" | "ext_info" | "remain_msg_len"
            ) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl C2cStreamMessageResponse {
    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        validate_non_empty("id", &self.id)?;
        DateTime::parse_from_rfc3339(&self.timestamp)
            .map_err(|_| StreamUploadValidationError::InvalidTimestamp { field: "timestamp" })?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DecimalBytes(String);

impl DecimalBytes {
    pub fn new(
        field: &'static str,
        value: impl Into<String>,
    ) -> Result<Self, StreamUploadValidationError> {
        let value = value.into();
        validate_decimal_bytes(field, &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn value(&self) -> u64 {
        self.0
            .parse()
            .expect("DecimalBytes preserves its numeric invariant")
    }
}

impl Serialize for DecimalBytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DecimalBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        validate_decimal_bytes("decimal byte count", &value).map_err(de::Error::custom)?;
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPrepareRequest {
    pub file_type: MediaFileType,
    pub file_size: DecimalBytes,
    pub file_name: String,
    pub md5: String,
    pub sha1: String,
    pub md5_10m: String,
}

impl UploadPrepareRequest {
    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        if !self.file_type.is_supported() {
            return Err(StreamUploadValidationError::UnsupportedFileType);
        }
        validate_non_empty("file_name", &self.file_name)?;
        validate_digest("md5", &self.md5, 32)?;
        validate_digest("sha1", &self.sha1, 40)?;
        validate_digest("md5_10m", &self.md5_10m, 32)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct UploadPart {
    pub index: u32,
    pub presigned_url: String,
    pub block_size: DecimalBytes,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Serialize for UploadPart {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let extra_len = self
            .extra
            .keys()
            .filter(|key| !matches!(key.as_str(), "index" | "presigned_url" | "block_size"))
            .count();
        let mut map = serializer.serialize_map(Some(3 + extra_len))?;
        map.serialize_entry("index", &self.index)?;
        map.serialize_entry("presigned_url", &self.presigned_url)?;
        map.serialize_entry("block_size", &self.block_size)?;
        for (key, value) in &self.extra {
            if !matches!(key.as_str(), "index" | "presigned_url" | "block_size") {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl fmt::Debug for UploadPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadPart")
            .field("index", &self.index)
            .field("presigned_url", &"[REDACTED]")
            .field("block_size", &self.block_size)
            .field("extra", &"[REDACTED]")
            .finish()
    }
}

impl UploadPart {
    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        validate_presigned_url(&self.presigned_url)
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct UploadConfig {
    pub concurrency: u32,
    pub retry_timeout: u32,
    pub retry_delay: u32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Serialize for UploadConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let extra_len = self
            .extra
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "concurrency" | "retry_timeout" | "retry_delay"
                )
            })
            .count();
        let mut map = serializer.serialize_map(Some(3 + extra_len))?;
        map.serialize_entry("concurrency", &self.concurrency)?;
        map.serialize_entry("retry_timeout", &self.retry_timeout)?;
        map.serialize_entry("retry_delay", &self.retry_delay)?;
        for (key, value) in &self.extra {
            if !matches!(
                key.as_str(),
                "concurrency" | "retry_timeout" | "retry_delay"
            ) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl fmt::Debug for UploadConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadConfig")
            .field("concurrency", &self.concurrency)
            .field("retry_timeout", &self.retry_timeout)
            .field("retry_delay", &self.retry_delay)
            .field("extra", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct UploadPrepareResponse {
    pub upload_id: String,
    pub block_size: DecimalBytes,
    pub parts: Vec<UploadPart>,
    pub upload_config: UploadConfig,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Serialize for UploadPrepareResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let extra_len = self
            .extra
            .keys()
            .filter(|key| {
                !matches!(
                    key.as_str(),
                    "upload_id" | "block_size" | "parts" | "upload_config"
                )
            })
            .count();
        let mut map = serializer.serialize_map(Some(4 + extra_len))?;
        map.serialize_entry("upload_id", &self.upload_id)?;
        map.serialize_entry("block_size", &self.block_size)?;
        map.serialize_entry("parts", &self.parts)?;
        map.serialize_entry("upload_config", &self.upload_config)?;
        for (key, value) in &self.extra {
            if !matches!(
                key.as_str(),
                "upload_id" | "block_size" | "parts" | "upload_config"
            ) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl fmt::Debug for UploadPrepareResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadPrepareResponse")
            .field("upload_id", &"[REDACTED]")
            .field("block_size", &self.block_size)
            .field("parts", &self.parts)
            .field("upload_config", &self.upload_config)
            .field("extra", &"[REDACTED]")
            .finish()
    }
}

impl UploadPrepareResponse {
    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        validate_upload_id(&self.upload_id)?;
        if self.parts.is_empty() {
            return Err(StreamUploadValidationError::EmptyParts);
        }
        if self.upload_config.concurrency == 0 {
            return Err(StreamUploadValidationError::ZeroConcurrency);
        }
        let mut indexes = BTreeSet::new();
        for part in &self.parts {
            part.validate()?;
            if !indexes.insert(part.index) {
                return Err(StreamUploadValidationError::DuplicatePartIndex { index: part.index });
            }
        }
        let expected_len = u32::try_from(self.parts.len())
            .map_err(|_| StreamUploadValidationError::InvalidPartSequence)?;
        if indexes.iter().copied().ne(0..expected_len) {
            return Err(StreamUploadValidationError::InvalidPartSequence);
        }
        let expected_block_size = self.block_size.value();
        for part in &self.parts {
            let actual = part.block_size.value();
            let is_final = part.index + 1 == expected_len;
            if actual > expected_block_size || (!is_final && actual != expected_block_size) {
                return Err(StreamUploadValidationError::InvalidPartBlockSize {
                    index: part.index,
                    expected_max: expected_block_size,
                    actual,
                });
            }
        }
        Ok(())
    }

    pub fn validate_for_request(
        &self,
        request: &UploadPrepareRequest,
    ) -> Result<(), StreamUploadValidationError> {
        self.validate()?;
        let actual = self.parts.iter().try_fold(0_u64, |total, part| {
            total
                .checked_add(part.block_size.value())
                .ok_or(StreamUploadValidationError::PartPlanSizeOverflow)
        })?;
        let expected = request.file_size.value();
        if actual != expected {
            return Err(StreamUploadValidationError::PartPlanSizeMismatch { expected, actual });
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPartFinishRequest {
    pub upload_id: String,
    pub part_index: u32,
    pub block_size: DecimalBytes,
    pub md5: String,
}

impl fmt::Debug for UploadPartFinishRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UploadPartFinishRequest")
            .field("upload_id", &"[REDACTED]")
            .field("part_index", &self.part_index)
            .field("block_size", &self.block_size)
            .field("md5", &self.md5)
            .finish()
    }
}

impl UploadPartFinishRequest {
    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        validate_upload_id(&self.upload_id)?;
        validate_digest("md5", &self.md5, 32)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaUploadFinalizeRequest {
    pub file_type: MediaFileType,
    pub upload_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default)]
    pub srv_send_msg: bool,
}

impl fmt::Debug for MediaUploadFinalizeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaUploadFinalizeRequest")
            .field("file_type", &self.file_type)
            .field("upload_id", &"[REDACTED]")
            .field("file_name", &self.file_name)
            .field("srv_send_msg", &self.srv_send_msg)
            .finish()
    }
}

impl MediaUploadFinalizeRequest {
    pub fn new(
        file_type: MediaFileType,
        upload_id: impl Into<String>,
        file_name: Option<String>,
    ) -> Self {
        Self {
            file_type,
            upload_id: upload_id.into(),
            file_name,
            srv_send_msg: false,
        }
    }

    pub fn validate(&self) -> Result<(), StreamUploadValidationError> {
        if !self.file_type.is_supported() {
            return Err(StreamUploadValidationError::UnsupportedFileType);
        }
        validate_upload_id(&self.upload_id)?;
        validate_optional_non_empty("file_name", self.file_name.as_deref())
    }
}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), StreamUploadValidationError> {
    if value.trim().is_empty() {
        Err(StreamUploadValidationError::EmptyField { field })
    } else {
        Ok(())
    }
}

fn validate_upload_id(value: &str) -> Result<(), StreamUploadValidationError> {
    validate_non_empty("upload_id", value)?;
    if value != value.trim() {
        return Err(StreamUploadValidationError::InvalidUploadId);
    }
    Ok(())
}

fn validate_optional_non_empty(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), StreamUploadValidationError> {
    match value {
        Some(value) => validate_non_empty(field, value),
        None => Ok(()),
    }
}

fn validate_decimal_bytes(
    field: &'static str,
    value: &str,
) -> Result<(), StreamUploadValidationError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.parse::<u64>().is_err()
    {
        return Err(StreamUploadValidationError::InvalidDecimalBytes { field });
    }
    if value.bytes().all(|byte| byte == b'0') {
        return Err(StreamUploadValidationError::ZeroByteSize { field });
    }
    Ok(())
}

fn validate_digest(
    field: &'static str,
    value: &str,
    length: usize,
) -> Result<(), StreamUploadValidationError> {
    if value.len() != length || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StreamUploadValidationError::InvalidDigest { field });
    }
    Ok(())
}

fn validate_presigned_url(value: &str) -> Result<(), StreamUploadValidationError> {
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(StreamUploadValidationError::InvalidPresignedUrl);
    }
    let url = Url::parse(value).map_err(|_| StreamUploadValidationError::InvalidPresignedUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || parsed_url_has_userinfo(value, &url)
        || url.fragment().is_some()
    {
        return Err(StreamUploadValidationError::InvalidPresignedUrl);
    }
    Ok(())
}

fn parsed_url_has_userinfo(value: &str, parsed: &Url) -> bool {
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return true;
    }
    let normalized = value
        .chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
        .map(|character| if character == '\\' { '/' } else { character })
        .collect::<String>();
    normalized
        .split_once(':')
        .map(|(_, remainder)| remainder.trim_start_matches('/'))
        .and_then(|remainder| remainder.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}
