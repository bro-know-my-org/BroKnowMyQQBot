//! Typed QQ bot command-panel requests and responses.
//!
//! Panels contain at most 20 commands or HTTPS links. Creation supports all
//! users in every scope, while targeted creation is limited to C2C and group
//! scopes and may omit its initial `OpenID` collection. `OpenID` collections
//! contain at most 20 entries.

use std::fmt;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use url::Url;

/// QQ conversation scope in which a command panel is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelScope {
    C2c,
    Group,
    Channel,
    Dm,
}

impl PanelScope {
    /// Returns the lowercase wire value used by the QQ API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::C2c => "c2c",
            Self::Group => "group",
            Self::Channel => "channel",
            Self::Dm => "dm",
        }
    }
}

/// Whether a panel applies to every conversation or selected `OpenID` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelTargetType {
    All,
    Specific,
}

/// Behavior performed by a panel item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelItemType {
    Command,
    Link,
}

/// Operation used when modifying a panel's target `OpenID` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelTargetOperation {
    Add,
    Del,
}

/// Validation failures for command-panel requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelValidationError {
    EmptyField {
        field: &'static str,
    },
    PageLimitOutOfRange {
        limit: u8,
    },
    SpecificScopeUnsupported {
        scope: PanelScope,
    },
    UnexpectedTargetField {
        field: &'static str,
        scope: PanelScope,
        target_type: PanelTargetType,
    },
    TooManyTargets {
        field: &'static str,
        count: usize,
        maximum: usize,
    },
    TooManyItems {
        count: usize,
    },
    TextTooLong {
        field: &'static str,
        weight: usize,
        maximum: usize,
    },
    CharacterLimitExceeded {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    InvalidOpenId {
        field: &'static str,
        index: usize,
    },
    EmptyOpenId {
        field: &'static str,
        index: usize,
    },
    InvalidItem {
        index: usize,
        source: Box<PanelValidationError>,
    },
    MissingLink,
    UnexpectedLink,
    InvalidHttpsUrl,
    MissingTargetObjects,
    MultipleTargetKinds,
}

impl fmt::Display for PanelValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(formatter, "QQ panel `{field}` must not be empty"),
            Self::PageLimitOutOfRange { limit } => {
                write!(
                    formatter,
                    "QQ panel page limit must be between 1 and 50, got {limit}"
                )
            }
            Self::SpecificScopeUnsupported { scope } => write!(
                formatter,
                "QQ panel scope {scope:?} does not support target_type specific"
            ),
            Self::UnexpectedTargetField {
                field,
                scope,
                target_type,
            } => write!(
                formatter,
                "QQ panel `{field}` is incompatible with scope {scope:?} and target_type {target_type:?}"
            ),
            Self::TooManyTargets {
                field,
                count,
                maximum,
            } => write!(
                formatter,
                "QQ panel `{field}` must not contain more than {maximum} entries, got {count}"
            ),
            Self::TooManyItems { count } => write!(
                formatter,
                "QQ panel must not contain more than 20 items, got {count}"
            ),
            Self::TextTooLong {
                field,
                weight,
                maximum,
            } => write!(
                formatter,
                "QQ panel `{field}` must not exceed {maximum} weighted characters (non-ASCII counts as 2), got {weight}"
            ),
            Self::CharacterLimitExceeded {
                field,
                length,
                maximum,
            } => write!(
                formatter,
                "QQ panel `{field}` must not exceed {maximum} characters, got {length}"
            ),
            Self::InvalidOpenId { field, index } => write!(
                formatter,
                "QQ panel `{field}` entry at index {index} contains whitespace or control characters"
            ),
            Self::EmptyOpenId { field, index } => {
                write!(
                    formatter,
                    "QQ panel `{field}` entry at index {index} is empty"
                )
            }
            Self::InvalidItem { index, source } => {
                write!(
                    formatter,
                    "QQ panel item at index {index} is invalid: {source}"
                )
            }
            Self::MissingLink => formatter.write_str("QQ link panel item must contain link"),
            Self::UnexpectedLink => {
                formatter.write_str("QQ command panel item must not contain link")
            }
            Self::InvalidHttpsUrl => {
                formatter.write_str("QQ panel item link must be a valid HTTPS URL")
            }
            Self::MissingTargetObjects => formatter
                .write_str("QQ panel target update must contain one non-empty OpenID collection"),
            Self::MultipleTargetKinds => formatter.write_str(
                "QQ panel target update must not contain both user_openids and group_openids",
            ),
        }
    }
}

impl std::error::Error for PanelValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidItem { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Query parameters for listing command panels in one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelListRequest {
    /// Conversation scope to list.
    pub scope: PanelScope,
    /// Pagination cursor returned by the preceding page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Requested page size, from 1 through 50 when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u8>,
}

impl PanelListRequest {
    /// Validates the local page-size constraint before authentication.
    pub fn validate(&self) -> Result<(), PanelValidationError> {
        if let Some(limit) = self.limit {
            if !(1..=50).contains(&limit) {
                return Err(PanelValidationError::PageLimitOutOfRange { limit });
            }
        }
        Ok(())
    }
}

/// Command-panel content shared by create, update, and response payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Panel {
    /// Optional panel items; QQ accepts at most 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<PanelItem>>,
    /// Optional operator remark, limited to 255 Unicode characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
    /// Optional panel-content version supplied by QQ responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

impl Panel {
    /// Validates item counts, item fields, links, and the remark limit.
    pub fn validate(&self) -> Result<(), PanelValidationError> {
        if let Some(items) = &self.items {
            if items.len() > 20 {
                return Err(PanelValidationError::TooManyItems { count: items.len() });
            }
            for (index, item) in items.iter().enumerate() {
                item.validate()
                    .map_err(|source| PanelValidationError::InvalidItem {
                        index,
                        source: Box::new(source),
                    })?;
            }
        }
        if let Some(remark) = &self.remark {
            validate_max_characters("panel.remark", remark, 255)?;
        }
        Ok(())
    }
}

/// One command or external link displayed in a panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelItem {
    /// Display name, limited to weight 14 where non-ASCII characters count as two.
    pub name: String,
    /// Description, limited to weight 30 where non-ASCII characters count as two.
    pub desc: String,
    /// Whether the item invokes a command or opens a link.
    #[serde(rename = "type")]
    pub item_type: PanelItemType,
    /// Whether only administrators may use the item.
    pub only_admin: bool,
    /// HTTPS destination required for link items and forbidden for command items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl PanelItem {
    fn validate(&self) -> Result<(), PanelValidationError> {
        validate_weighted_text("panel.items[].name", &self.name, 14)?;
        validate_weighted_text("panel.items[].desc", &self.desc, 30)?;
        match self.item_type {
            PanelItemType::Command if self.link.is_some() => {
                Err(PanelValidationError::UnexpectedLink)
            }
            PanelItemType::Link => {
                let link = self
                    .link
                    .as_deref()
                    .ok_or(PanelValidationError::MissingLink)?;
                validate_https_url(link)
            }
            PanelItemType::Command => Ok(()),
        }
    }
}

/// Request body for creating a command panel.
///
/// Targeted C2C panels use `user_openids`; targeted group panels use
/// `group_openids`. Both collections may be omitted to create the panel before
/// assigning initial targets. Channel and DM scopes do not support targeted
/// creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePanelRequest {
    /// Conversation scope for the new panel.
    pub scope: PanelScope,
    /// Whether the panel targets all conversations or selected `OpenID` values.
    pub target_type: PanelTargetType,
    /// Initial C2C targets, with at most 20 `OpenID` values when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_openids: Option<Vec<String>>,
    /// Initial group targets, with at most 20 `OpenID` values when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_openids: Option<Vec<String>>,
    /// Panel content to create.
    pub panel: Panel,
}

impl CreatePanelRequest {
    /// Validates scope/target compatibility and panel content before authentication.
    pub fn validate(&self) -> Result<(), PanelValidationError> {
        validate_create_targets(self)?;
        self.panel.validate()
    }
}

/// Response returned after a panel is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePanelResponse {
    /// QQ-assigned panel identifier.
    pub panel_id: String,
}

/// Request body for replacing a panel's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePanelRequest {
    /// Replacement panel content.
    pub panel: Panel,
}

impl UpdatePanelRequest {
    /// Validates replacement panel content before authentication.
    pub fn validate(&self) -> Result<(), PanelValidationError> {
        self.panel.validate()
    }
}

/// Version response returned after updating a panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelVersion {
    /// Current top-level panel version assigned by QQ.
    pub version: u64,
}

/// Request body for adding or deleting panel targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePanelTargetsRequest {
    /// Add or delete operation.
    pub op: PanelTargetOperation,
    /// C2C target `OpenID` values; mutually exclusive with `group_openids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_openids: Option<Vec<String>>,
    /// Group target `OpenID` values; mutually exclusive with `user_openids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_openids: Option<Vec<String>>,
}

impl UpdatePanelTargetsRequest {
    /// Validates that exactly one non-empty collection with at most 20 `OpenID` values is present.
    pub fn validate(&self) -> Result<(), PanelValidationError> {
        match (&self.user_openids, &self.group_openids) {
            (Some(_), Some(_)) => return Err(PanelValidationError::MultipleTargetKinds),
            (None, None) => return Err(PanelValidationError::MissingTargetObjects),
            (Some(openids), None) => validate_openids("user_openids", openids, 20)?,
            (None, Some(openids)) => validate_openids("group_openids", openids, 20)?,
        }
        Ok(())
    }
}

/// Panel metadata returned by list and detail endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelRecord {
    /// QQ-assigned panel identifier.
    pub panel_id: String,
    /// Conversation scope of the panel.
    pub scope: PanelScope,
    /// Targeting mode of the panel.
    pub target_type: PanelTargetType,
    /// Panel content, including any content-specific version.
    pub panel: Panel,
    /// Creation timestamp when supplied by QQ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<FixedOffset>>,
    /// Last-update timestamp when supplied by QQ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<FixedOffset>>,
    /// Required top-level record version supplied by QQ.
    pub version: u64,
}

/// Paginated command-panel list response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelPage {
    /// Panel records in this page.
    pub records: Vec<PanelRecord>,
    /// Cursor for the next page.
    pub next_cursor: String,
    /// Whether this is the final page.
    pub is_end: bool,
}

/// Command-panel detail response including resolved target `OpenID` values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelDetail {
    /// Flattened panel metadata.
    #[serde(flatten)]
    pub record: PanelRecord,
    /// C2C targets; defaults to an empty collection when omitted by QQ.
    #[serde(default)]
    pub user_openids: Vec<String>,
    /// Group targets; defaults to an empty collection when omitted by QQ.
    #[serde(default)]
    pub group_openids: Vec<String>,
}

fn validate_create_targets(request: &CreatePanelRequest) -> Result<(), PanelValidationError> {
    if request.target_type == PanelTargetType::Specific
        && matches!(request.scope, PanelScope::Channel | PanelScope::Dm)
    {
        return Err(PanelValidationError::SpecificScopeUnsupported {
            scope: request.scope,
        });
    }
    match (request.scope, request.target_type) {
        (PanelScope::C2c, PanelTargetType::Specific) => {
            reject_target_field(request, "group_openids", request.group_openids.as_ref())?;
            if let Some(openids) = &request.user_openids {
                validate_openids("user_openids", openids, 20)?;
            }
        }
        (PanelScope::Group, PanelTargetType::Specific) => {
            reject_target_field(request, "user_openids", request.user_openids.as_ref())?;
            if let Some(openids) = &request.group_openids {
                validate_openids("group_openids", openids, 20)?;
            }
        }
        _ => {
            reject_target_field(request, "user_openids", request.user_openids.as_ref())?;
            reject_target_field(request, "group_openids", request.group_openids.as_ref())?;
        }
    }
    Ok(())
}

fn reject_target_field(
    request: &CreatePanelRequest,
    field: &'static str,
    value: Option<&Vec<String>>,
) -> Result<(), PanelValidationError> {
    if value.is_some() {
        return Err(PanelValidationError::UnexpectedTargetField {
            field,
            scope: request.scope,
            target_type: request.target_type,
        });
    }
    Ok(())
}

fn validate_openids(
    field: &'static str,
    openids: &[String],
    maximum: usize,
) -> Result<(), PanelValidationError> {
    if openids.is_empty() {
        return Err(PanelValidationError::MissingTargetObjects);
    }
    if openids.len() > maximum {
        return Err(PanelValidationError::TooManyTargets {
            field,
            count: openids.len(),
            maximum,
        });
    }
    for (index, openid) in openids.iter().enumerate() {
        if openid.is_empty() {
            return Err(PanelValidationError::EmptyOpenId { field, index });
        }
        if openid
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(PanelValidationError::InvalidOpenId { field, index });
        }
    }
    Ok(())
}

fn validate_weighted_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), PanelValidationError> {
    if value.trim().is_empty() {
        return Err(PanelValidationError::EmptyField { field });
    }
    let weight = value
        .chars()
        .map(|character| usize::from(!character.is_ascii()) + 1)
        .sum::<usize>();
    if weight > maximum {
        return Err(PanelValidationError::TextTooLong {
            field,
            weight,
            maximum,
        });
    }
    Ok(())
}

fn validate_max_characters(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), PanelValidationError> {
    let length = value.chars().count();
    if length > maximum {
        return Err(PanelValidationError::CharacterLimitExceeded {
            field,
            length,
            maximum,
        });
    }
    Ok(())
}

fn validate_https_url(value: &str) -> Result<(), PanelValidationError> {
    if value.trim().is_empty()
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(PanelValidationError::InvalidHttpsUrl);
    }
    let url = Url::parse(value).map_err(|_| PanelValidationError::InvalidHttpsUrl)?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(PanelValidationError::InvalidHttpsUrl);
    }
    Ok(())
}
