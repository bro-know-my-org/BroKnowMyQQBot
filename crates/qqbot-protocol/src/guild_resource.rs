//! Typed QQ guild and channel resource models.

use std::{collections::BTreeMap, fmt};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Validation failures for Guild pagination, Channel mutations, and typed resource responses.
pub enum GuildResourceValidationError {
    EmptyField { field: &'static str },
    ControlCharacter { field: &'static str },
    ConflictingGuildCursors,
    GuildPageLimitOutOfRange { limit: u16 },
    InvalidJoinedAt,
    ChannelGuildMismatch,
    EmptyChannelMutation,
    InvalidChannelType { value: i64 },
    InvalidChannelSubType { value: i64 },
    InvalidChannelPrivateType { value: i64 },
    InvalidSpeakPermission { value: i64 },
    InvalidChannelPosition { position: i64 },
    GroupPositionTooSmall { position: i64 },
    ApplicationIdForNonApplicationChannel,
    PrivateUsersForIncompatibleChannel,
}

impl fmt::Display for GuildResourceValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField { field } => write!(formatter, "QQ `{field}` must not be empty"),
            Self::ControlCharacter { field } => {
                write!(
                    formatter,
                    "QQ `{field}` must not contain control characters"
                )
            }
            Self::ConflictingGuildCursors => {
                formatter.write_str("QQ guild list `before` and `after` are mutually exclusive")
            }
            Self::GuildPageLimitOutOfRange { limit } => write!(
                formatter,
                "QQ guild list limit must be between 1 and 100, got {limit}"
            ),
            Self::InvalidJoinedAt => {
                formatter.write_str("QQ guild `joined_at` must be an RFC3339 timestamp")
            }
            Self::ChannelGuildMismatch => {
                formatter.write_str("QQ nested channel does not belong to its guild")
            }
            Self::EmptyChannelMutation => {
                formatter.write_str("QQ channel mutation must contain at least one field")
            }
            Self::InvalidChannelType { value } => write!(
                formatter,
                "QQ channel type is not a currently supported send value: {value}"
            ),
            Self::InvalidChannelSubType { value } => write!(
                formatter,
                "QQ channel sub-type must be between 0 and 3, got {value}"
            ),
            Self::InvalidChannelPrivateType { value } => write!(
                formatter,
                "QQ channel private type must be between 0 and 2, got {value}"
            ),
            Self::InvalidSpeakPermission { value } => write!(
                formatter,
                "QQ channel speak permission must be 1 or 2, got {value}"
            ),
            Self::InvalidChannelPosition { position } => write!(
                formatter,
                "QQ channel position must be positive, got {position}"
            ),
            Self::GroupPositionTooSmall { position } => write!(
                formatter,
                "QQ channel group position must be at least 2, got {position}"
            ),
            Self::ApplicationIdForNonApplicationChannel => formatter
                .write_str("QQ channel `application_id` is only valid for application channels"),
            Self::PrivateUsersForIncompatibleChannel => formatter.write_str(
                "QQ channel `private_user_ids` requires the selected-members private type",
            ),
        }
    }
}

impl std::error::Error for GuildResourceValidationError {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Optional query parameters for `GET /users/@me/guilds`.
///
/// `before` and `after` are mutually exclusive opaque Guild IDs. `limit`, when
/// present, must be in `1..=100`.
pub struct GuildListQuery {
    /// Read the page before this opaque Guild ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// Read the page after this opaque Guild ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Maximum number of Guilds returned by QQ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
}

impl GuildListQuery {
    pub fn validate(&self) -> Result<(), GuildResourceValidationError> {
        if self.before.is_some() && self.after.is_some() {
            return Err(GuildResourceValidationError::ConflictingGuildCursors);
        }
        if let Some(before) = self.before.as_deref() {
            validate_text("before", before)?;
        }
        if let Some(after) = self.after.as_deref() {
            validate_text("after", after)?;
        }
        if let Some(limit) = self.limit {
            if !(1..=100).contains(&limit) {
                return Err(GuildResourceValidationError::GuildPageLimitOutOfRange { limit });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// A Guild returned by the QQ resource `OpenAPI`.
///
/// Unknown response fields are retained in a read-only extension map and are
/// emitted again when the value is serialized by the Adapter.
pub struct Guild {
    /// Opaque Guild ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Guild icon URL.
    pub icon: String,
    /// Opaque owner user ID.
    pub owner_id: String,
    /// Whether the current Bot/user is the owner.
    pub owner: bool,
    /// RFC3339 time at which the Bot/user joined the Guild.
    pub joined_at: String,
    /// Current member count.
    pub member_count: u64,
    /// Maximum member count.
    pub max_members: u64,
    /// Guild description, which QQ may return as an empty string.
    pub description: String,
    /// Optional embedded Channel extension used by some first-party SDK responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<Channel>>,
    /// Optional cross-Guild world identifier exposed by some QQ responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_world_id: Option<String>,
    /// Optional cross-Guild organization identifier exposed by some QQ responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_org_id: Option<String>,
    /// Optional operator user ID used by some shared Guild payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_user_id: Option<String>,
    #[serde(default, flatten)]
    extra: BTreeMap<String, Value>,
}

impl Guild {
    /// Returns unknown fields retained from the QQ response.
    pub fn extra(&self) -> &BTreeMap<String, Value> {
        &self.extra
    }

    /// Validates IDs, the join timestamp, and any embedded Channel association.
    pub fn validate(&self) -> Result<(), GuildResourceValidationError> {
        validate_text("guild.id", &self.id)?;
        validate_text("guild.owner_id", &self.owner_id)?;
        validate_optional_text("guild.union_world_id", self.union_world_id.as_deref())?;
        validate_optional_text("guild.union_org_id", self.union_org_id.as_deref())?;
        DateTime::parse_from_rfc3339(&self.joined_at)
            .map_err(|_| GuildResourceValidationError::InvalidJoinedAt)?;
        if let Some(channels) = &self.channels {
            for channel in channels {
                channel.validate()?;
                if channel.guild_id != self.id {
                    return Err(GuildResourceValidationError::ChannelGuildMismatch);
                }
            }
        }
        validate_optional_text("guild.op_user_id", self.op_user_id.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// QQ Channel type.
///
/// Responses retain unknown integer values for forward compatibility. Channel
/// creation only sends values for which [`Self::is_known`] returns `true`.
pub struct ChannelType(pub i64);

impl ChannelType {
    /// Text Channel (`0`).
    pub const TEXT: Self = Self(0);
    /// Voice Channel (`2`).
    pub const VOICE: Self = Self(2);
    /// Channel group (`4`).
    pub const GROUP: Self = Self(4);
    /// Live Channel (`10005`).
    pub const LIVE: Self = Self(10_005);
    /// Application Channel (`10006`).
    pub const APPLICATION: Self = Self(10_006);
    /// Forum Channel (`10007`).
    pub const FORUM: Self = Self(10_007);

    /// Whether this value is currently documented and allowed in create requests.
    pub fn is_known(self) -> bool {
        matches!(
            self,
            Self::TEXT | Self::VOICE | Self::GROUP | Self::LIVE | Self::APPLICATION | Self::FORUM
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// QQ Channel sub-type.
///
/// Responses retain unknown values; requests currently allow `0..=3`.
pub struct ChannelSubType(pub i64);

impl ChannelSubType {
    /// Chat (`0`).
    pub const CHAT: Self = Self(0);
    /// Announcement (`1`).
    pub const ANNOUNCEMENT: Self = Self(1);
    /// Guide (`2`).
    pub const GUIDE: Self = Self(2);
    /// Team-up/game voice context (`3`).
    pub const TEAM_UP: Self = Self(3);

    /// Whether this value is currently documented and allowed in create requests.
    pub fn is_known(self) -> bool {
        (0..=3).contains(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// QQ Channel visibility type.
///
/// Responses retain unknown values; requests currently allow `0..=2`.
pub struct ChannelPrivateType(pub i64);

impl ChannelPrivateType {
    /// Public visibility (`0`).
    pub const PUBLIC: Self = Self(0);
    /// Visible to the owner and administrators (`1`).
    pub const ADMIN_ONLY: Self = Self(1);
    /// Visible to the owner, administrators, and selected members (`2`).
    pub const SELECTED_MEMBERS: Self = Self(2);

    /// Whether this value is currently documented.
    pub fn is_known(self) -> bool {
        (0..=2).contains(&self.0)
    }

    /// Whether this value represents either documented private visibility mode.
    pub fn is_private(self) -> bool {
        matches!(self, Self::ADMIN_ONLY | Self::SELECTED_MEMBERS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
/// QQ Channel speaking permission.
///
/// Responses retain unknown values and may contain `0`; requests only send
/// `1` or `2` because QQ documents `0` as an invalid response state.
pub struct SpeakPermission(pub i64);

impl SpeakPermission {
    /// Invalid/unconfigured response state (`0`).
    pub const INVALID: Self = Self(0);
    /// Everyone may speak (`1`).
    pub const EVERYONE: Self = Self(1);
    /// Only owners, administrators, and selected members may speak (`2`).
    pub const ADMIN_AND_SELECTED: Self = Self(2);

    /// Whether this value is currently documented for responses.
    pub fn is_known(self) -> bool {
        (0..=2).contains(&self.0)
    }

    fn is_sendable(self) -> bool {
        matches!(self, Self::EVERYONE | Self::ADMIN_AND_SELECTED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// A Channel returned by the QQ resource `OpenAPI`.
///
/// Numeric classifications preserve unknown response values. Unknown object
/// fields are retained in a read-only extension map.
pub struct Channel {
    /// Opaque Channel ID.
    pub id: String,
    /// Opaque parent Guild ID.
    pub guild_id: String,
    /// Channel display name.
    pub name: String,
    /// Channel type from the wire field `type`.
    #[serde(rename = "type")]
    pub channel_type: ChannelType,
    /// Channel sub-type.
    pub sub_type: ChannelSubType,
    /// Sort position; QQ starts ordinary positions at one and groups at two.
    pub position: i64,
    /// Opaque parent group ID; root-level Channels commonly use `"0"`.
    pub parent_id: String,
    /// Opaque creator/owner user ID.
    pub owner_id: String,
    /// Visibility classification.
    pub private_type: ChannelPrivateType,
    /// Speaking permission classification.
    pub speak_permission: SpeakPermission,
    /// Optional application ID for application Channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    /// Optional decimal permission bitmap returned for the current user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(default, flatten)]
    extra: BTreeMap<String, Value>,
}

impl Channel {
    /// Returns unknown fields retained from the QQ response.
    pub fn extra(&self) -> &BTreeMap<String, Value> {
        &self.extra
    }

    /// Validates resource IDs and documented position invariants.
    pub fn validate(&self) -> Result<(), GuildResourceValidationError> {
        validate_text("channel.id", &self.id)?;
        validate_text("channel.guild_id", &self.guild_id)?;
        validate_text("channel.parent_id", &self.parent_id)?;
        validate_text("channel.owner_id", &self.owner_id)?;
        if self.position < 1 {
            return Err(GuildResourceValidationError::InvalidChannelPosition {
                position: self.position,
            });
        }
        if self.channel_type == ChannelType::GROUP && self.position < 2 {
            return Err(GuildResourceValidationError::GroupPositionTooSmall {
                position: self.position,
            });
        }
        validate_optional_text("channel.application_id", self.application_id.as_deref())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Fields accepted by `POST /guilds/{guild_id}/channels`.
///
/// QQ currently marks every wire field optional and does not publish the true
/// minimum creation set. The client therefore rejects a completely empty
/// request and explicit contradictions, but does not invent additional required
/// fields. Zero-valued classifications remain explicitly serializable.
pub struct CreateChannelRequest {
    /// Optional Channel name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional documented Channel type, serialized as `type`.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<ChannelType>,
    /// Optional Channel sub-type; QQ's voice example uses sub-type `3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<ChannelSubType>,
    /// Optional sort position; group positions must be at least two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// Optional parent group ID; root-level examples use `"0"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Optional visibility classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_type: Option<ChannelPrivateType>,
    /// Optional selected-member IDs for private Channel creation.
    ///
    /// QQ has not documented the list limit, empty-list behavior, or defaults
    /// when `private_type` is omitted, so only an explicit public-mode conflict
    /// is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_user_ids: Option<Vec<String>>,
    /// Optional speaking permission; requests only allow `1` or `2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speak_permission: Option<SpeakPermission>,
    /// Optional application ID. An explicitly non-application `type` conflicts
    /// with this field; an omitted type is left to QQ's undocumented defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
}

impl CreateChannelRequest {
    /// Validates documented send values and explicit field contradictions.
    pub fn validate(&self) -> Result<(), GuildResourceValidationError> {
        if self.name.is_none()
            && self.channel_type.is_none()
            && self.sub_type.is_none()
            && self.position.is_none()
            && self.parent_id.is_none()
            && self.private_type.is_none()
            && self.private_user_ids.is_none()
            && self.speak_permission.is_none()
            && self.application_id.is_none()
        {
            return Err(GuildResourceValidationError::EmptyChannelMutation);
        }
        validate_optional_text("channel.name", self.name.as_deref())?;
        validate_optional_text("channel.parent_id", self.parent_id.as_deref())?;
        validate_optional_text("channel.application_id", self.application_id.as_deref())?;
        validate_send_fields(
            self.channel_type,
            self.sub_type,
            self.position,
            self.private_type,
            self.speak_permission,
        )?;
        if self.application_id.is_some()
            && self
                .channel_type
                .is_some_and(|channel_type| channel_type != ChannelType::APPLICATION)
        {
            return Err(GuildResourceValidationError::ApplicationIdForNonApplicationChannel);
        }
        if let Some(user_ids) = &self.private_user_ids {
            if !user_ids.is_empty()
                && self
                    .private_type
                    .is_some_and(|value| value != ChannelPrivateType::SELECTED_MEMBERS)
            {
                return Err(GuildResourceValidationError::PrivateUsersForIncompatibleChannel);
            }
            for user_id in user_ids {
                validate_text("channel.private_user_ids[]", user_id)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Partial fields accepted by `PATCH /channels/{channel_id}`.
///
/// QQ currently permits only the five fields represented here. At least one
/// field must be present.
pub struct UpdateChannelRequest {
    /// Replacement Channel name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replacement sort position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    /// Replacement parent group ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Replacement visibility classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_type: Option<ChannelPrivateType>,
    /// Replacement speaking permission (`1` or `2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speak_permission: Option<SpeakPermission>,
}

impl UpdateChannelRequest {
    /// Validates a non-empty partial update and documented send values.
    pub fn validate(&self) -> Result<(), GuildResourceValidationError> {
        if self.name.is_none()
            && self.position.is_none()
            && self.parent_id.is_none()
            && self.private_type.is_none()
            && self.speak_permission.is_none()
        {
            return Err(GuildResourceValidationError::EmptyChannelMutation);
        }
        validate_optional_text("channel.name", self.name.as_deref())?;
        validate_optional_text("channel.parent_id", self.parent_id.as_deref())?;
        validate_send_fields(
            None,
            None,
            self.position,
            self.private_type,
            self.speak_permission,
        )
    }
}

fn validate_send_fields(
    channel_type: Option<ChannelType>,
    sub_type: Option<ChannelSubType>,
    position: Option<i64>,
    private_type: Option<ChannelPrivateType>,
    speak_permission: Option<SpeakPermission>,
) -> Result<(), GuildResourceValidationError> {
    if let Some(channel_type) = channel_type.filter(|value| !value.is_known()) {
        return Err(GuildResourceValidationError::InvalidChannelType {
            value: channel_type.0,
        });
    }
    if let Some(sub_type) = sub_type.filter(|value| !value.is_known()) {
        return Err(GuildResourceValidationError::InvalidChannelSubType { value: sub_type.0 });
    }
    if let Some(position) = position {
        if position < 1 {
            return Err(GuildResourceValidationError::InvalidChannelPosition { position });
        }
        if channel_type == Some(ChannelType::GROUP) && position < 2 {
            return Err(GuildResourceValidationError::GroupPositionTooSmall { position });
        }
    }
    if let Some(private_type) = private_type.filter(|value| !value.is_known()) {
        return Err(GuildResourceValidationError::InvalidChannelPrivateType {
            value: private_type.0,
        });
    }
    if let Some(permission) = speak_permission.filter(|value| !value.is_sendable()) {
        return Err(GuildResourceValidationError::InvalidSpeakPermission {
            value: permission.0,
        });
    }
    Ok(())
}

fn validate_optional_text(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), GuildResourceValidationError> {
    if let Some(value) = value {
        validate_text(field, value)?;
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str) -> Result<(), GuildResourceValidationError> {
    if value.trim().is_empty() {
        return Err(GuildResourceValidationError::EmptyField { field });
    }
    if value.chars().any(char::is_control) {
        return Err(GuildResourceValidationError::ControlCharacter { field });
    }
    Ok(())
}
