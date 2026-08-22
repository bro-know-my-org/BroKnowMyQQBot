//! Typed QQ guild member and role-management protocol models.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Validation failures for public guild member and role-management requests.
pub enum GuildRequestValidationError {
    EmptyPageCursor,
    PageLimitOutOfRange { limit: u16 },
    EmptyRoleMutation,
    EmptyRoleName,
    InvalidRoleHoist { hoist: u32 },
    InvalidHistoryDeletionDays { days: i32 },
    MissingChannelForRoleFive,
    EmptyRoleMemberChannelId,
}

impl fmt::Display for GuildRequestValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPageCursor => {
                formatter.write_str("QQ guild member page cursor must not be empty")
            }
            Self::PageLimitOutOfRange { limit } => write!(
                formatter,
                "QQ guild member page limit must be between 1 and 400, got {limit}"
            ),
            Self::EmptyRoleMutation => {
                formatter.write_str("QQ guild role mutation must contain at least one field")
            }
            Self::EmptyRoleName => formatter.write_str("QQ guild role name must not be empty"),
            Self::InvalidRoleHoist { hoist } => {
                write!(formatter, "QQ guild role hoist must be 0 or 1, got {hoist}")
            }
            Self::InvalidHistoryDeletionDays { days } => write!(
                formatter,
                "QQ member history deletion days must be -1, 0, 3, 7, 15, or 30, got {days}"
            ),
            Self::MissingChannelForRoleFive => {
                formatter.write_str("QQ guild role 5 member request requires a channel id")
            }
            Self::EmptyRoleMemberChannelId => {
                formatter.write_str("QQ guild role member channel id must not be empty")
            }
        }
    }
}

impl std::error::Error for GuildRequestValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPermissionField {
    Add,
    Remove,
}

impl fmt::Display for ChannelPermissionField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Add => "add",
            Self::Remove => "remove",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelPermissionValidationError {
    InvalidBitmap { field: ChannelPermissionField },
    BitmapOverflow { field: ChannelPermissionField },
    ManageChannelPermission { field: ChannelPermissionField },
}

impl fmt::Display for ChannelPermissionValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBitmap { field } => write!(
                formatter,
                "QQ channel permission `{field}` must be an unsigned decimal bitmap"
            ),
            Self::BitmapOverflow { field } => write!(
                formatter,
                "QQ channel permission `{field}` exceeds the u64 bitmap range"
            ),
            Self::ManageChannelPermission { field } => write!(
                formatter,
                "QQ channel permission `{field}` must not modify the manage-channel bit"
            ),
        }
    }
}

impl std::error::Error for ChannelPermissionValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildUser {
    pub id: String,
    pub username: String,
    pub avatar: String,
    pub bot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_flags: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_openid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_user_account: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuildDispatchValidationError {
    pub field: &'static str,
}

impl fmt::Display for GuildDispatchValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "QQ guild dispatch `{}` must not be empty",
            self.field
        )
    }
}

impl std::error::Error for GuildDispatchValidationError {}

/// Payload shared by QQ `GUILD_CREATE`, `GUILD_UPDATE`, and `GUILD_DELETE`.
///
/// The generated QQ event pages list `joined_at` and `op_user_id`, while the
/// update/delete examples may omit them, so both are optional on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildEvent {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub owner_id: String,
    pub member_count: u64,
    pub max_members: u64,
    pub description: String,
    /// Generic Guild objects call this field `owner`; current generated event
    /// examples omit it, so dispatch decoding keeps it optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_user_id: Option<String>,
}

impl GuildEvent {
    pub fn validate(&self) -> Result<(), GuildDispatchValidationError> {
        validate_dispatch_fields([
            ("id", self.id.as_str()),
            ("owner_id", self.owner_id.as_str()),
        ])?;
        validate_optional_dispatch_field("joined_at", self.joined_at.as_deref())?;
        validate_optional_dispatch_field("op_user_id", self.op_user_id.as_deref())
    }
}

/// Payload shared by QQ `CHANNEL_CREATE`, `CHANNEL_UPDATE`, and
/// `CHANNEL_DELETE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEvent {
    pub id: String,
    pub guild_id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub channel_type: u64,
    pub sub_type: u64,
    pub owner_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_type: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speak_permission: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_user_id: Option<String>,
}

impl ChannelEvent {
    pub fn validate(&self) -> Result<(), GuildDispatchValidationError> {
        validate_dispatch_fields([
            ("id", self.id.as_str()),
            ("guild_id", self.guild_id.as_str()),
            ("owner_id", self.owner_id.as_str()),
        ])?;
        validate_optional_dispatch_field("op_user_id", self.op_user_id.as_deref())
    }
}

/// Payload shared by QQ `GUILD_MEMBER_ADD`, `GUILD_MEMBER_UPDATE`, and
/// `GUILD_MEMBER_REMOVE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMemberEvent {
    pub guild_id: String,
    pub joined_at: String,
    /// Guild nickname. QQ may return JSON `null` for members without one.
    pub nick: Option<String>,
    pub op_user_id: String,
    pub roles: Vec<String>,
    pub user: GuildUser,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deaf: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
}

impl GuildMemberEvent {
    pub fn validate(&self) -> Result<(), GuildDispatchValidationError> {
        validate_dispatch_fields([
            ("guild_id", self.guild_id.as_str()),
            ("joined_at", self.joined_at.as_str()),
            ("op_user_id", self.op_user_id.as_str()),
            ("user.id", self.user.id.as_str()),
        ])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildMember {
    pub user: GuildUser,
    pub nick: String,
    pub roles: Vec<String>,
    pub joined_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deaf: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Pagination for the guild member list. Use `"0"` for the first page;
/// `limit` must be in `1..=400`.
pub struct GuildMemberPageRequest {
    #[serde(default = "default_page_cursor")]
    pub after: String,
    #[serde(default = "default_page_limit")]
    pub limit: u16,
}

impl Default for GuildMemberPageRequest {
    fn default() -> Self {
        Self {
            after: default_page_cursor(),
            limit: default_page_limit(),
        }
    }
}

impl GuildMemberPageRequest {
    pub fn validate(&self) -> Result<(), GuildRequestValidationError> {
        validate_page_cursor_and_limit(&self.after, self.limit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Pagination for members of a guild role. Use `"0"` for the first page;
/// `limit` must be in `1..=400`.
pub struct GuildRoleMemberPageRequest {
    #[serde(default = "default_page_cursor")]
    pub start_index: String,
    #[serde(default = "default_page_limit")]
    pub limit: u16,
}

impl Default for GuildRoleMemberPageRequest {
    fn default() -> Self {
        Self {
            start_index: default_page_cursor(),
            limit: default_page_limit(),
        }
    }
}

impl GuildRoleMemberPageRequest {
    pub fn validate(&self) -> Result<(), GuildRequestValidationError> {
        validate_page_cursor_and_limit(&self.start_index, self.limit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildRoleMemberPage {
    pub data: Vec<GuildMember>,
    pub next: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineMemberCount {
    pub online_nums: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelMemberPermissions {
    pub channel_id: String,
    pub user_id: String,
    pub permissions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRolePermissions {
    pub channel_id: String,
    pub role_id: String,
    pub permissions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Permission bitmap changes for a channel member or role.
///
/// QQ expects both fields as unsigned decimal strings. When the same bit is
/// present in both fields, removal takes precedence.
pub struct UpdateChannelPermissionsRequest {
    pub add: String,
    pub remove: String,
}

impl UpdateChannelPermissionsRequest {
    pub fn validate(&self) -> Result<(), ChannelPermissionValidationError> {
        const MANAGE_CHANNEL_PERMISSION: u64 = 1 << 1;

        let add = validate_permission_bitmap(ChannelPermissionField::Add, &self.add)?;
        let remove = validate_permission_bitmap(ChannelPermissionField::Remove, &self.remove)?;
        if add & MANAGE_CHANNEL_PERMISSION != 0 {
            return Err(ChannelPermissionValidationError::ManageChannelPermission {
                field: ChannelPermissionField::Add,
            });
        }
        if remove & MANAGE_CHANNEL_PERMISSION != 0 {
            return Err(ChannelPermissionValidationError::ManageChannelPermission {
                field: ChannelPermissionField::Remove,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildRole {
    pub id: String,
    pub name: String,
    pub color: u32,
    pub hoist: u32,
    pub number: u32,
    pub member_limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildRoles {
    pub guild_id: String,
    pub roles: Vec<GuildRole>,
    pub role_num_limit: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
/// Fields accepted when creating or updating a guild role.
///
/// At least one field is required, `name` cannot be blank, and `hoist` is
/// either `0` or `1`. QQ permits any one of these fields for role creation.
pub struct GuildRoleMutation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hoist: Option<u32>,
}

impl GuildRoleMutation {
    /// QQ documents all three fields as optional for both create and update,
    /// while requiring at least one of them to be present.
    pub fn validate(&self) -> Result<(), GuildRequestValidationError> {
        if self.name.is_none() && self.color.is_none() && self.hoist.is_none() {
            return Err(GuildRequestValidationError::EmptyRoleMutation);
        }
        if self
            .name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(GuildRequestValidationError::EmptyRoleName);
        }
        if let Some(hoist) = self.hoist.filter(|value| *value > 1) {
            return Err(GuildRequestValidationError::InvalidRoleHoist { hoist });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGuildRoleResult {
    pub role_id: String,
    pub role: GuildRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateGuildRoleResult {
    pub guild_id: String,
    pub role_id: String,
    pub role: GuildRole,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
/// Options used when removing a guild member.
///
/// `delete_history_msg_days` accepts `0` (do not retract), `3`, `7`, `15`,
/// `30`, or `-1` (retract all available history).
pub struct RemoveGuildMemberRequest {
    #[serde(default)]
    pub add_blacklist: bool,
    #[serde(default)]
    pub delete_history_msg_days: i32,
}

impl RemoveGuildMemberRequest {
    pub fn validate(&self) -> Result<(), GuildRequestValidationError> {
        if matches!(self.delete_history_msg_days, -1 | 0 | 3 | 7 | 15 | 30) {
            Ok(())
        } else {
            Err(GuildRequestValidationError::InvalidHistoryDeletionDays {
                days: self.delete_history_msg_days,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildChannelReference {
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
/// Optional channel context for a guild role membership change.
///
/// QQ requires `channel` when changing the special channel-administrator role
/// whose role ID is `"5"`.
pub struct GuildRoleMemberRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<GuildChannelReference>,
}

impl GuildRoleMemberRequest {
    pub fn for_channel(channel_id: impl Into<String>) -> Self {
        Self {
            channel: Some(GuildChannelReference {
                id: channel_id.into(),
            }),
        }
    }

    pub fn validate(&self, role_id: &str) -> Result<(), GuildRequestValidationError> {
        if role_id == "5" && self.channel.is_none() {
            return Err(GuildRequestValidationError::MissingChannelForRoleFive);
        }
        if self
            .channel
            .as_ref()
            .is_some_and(|channel| channel.id.trim().is_empty())
        {
            return Err(GuildRequestValidationError::EmptyRoleMemberChannelId);
        }
        Ok(())
    }
}

const fn default_page_limit() -> u16 {
    1
}

fn default_page_cursor() -> String {
    "0".to_owned()
}

fn validate_page_cursor_and_limit(
    cursor: &str,
    limit: u16,
) -> Result<(), GuildRequestValidationError> {
    if cursor.trim().is_empty() {
        return Err(GuildRequestValidationError::EmptyPageCursor);
    }
    if !(1..=400).contains(&limit) {
        return Err(GuildRequestValidationError::PageLimitOutOfRange { limit });
    }
    Ok(())
}

fn validate_permission_bitmap(
    field: ChannelPermissionField,
    value: &str,
) -> Result<u64, ChannelPermissionValidationError> {
    if value.is_empty() || value.bytes().any(|byte| !byte.is_ascii_digit()) {
        return Err(ChannelPermissionValidationError::InvalidBitmap { field });
    }
    value
        .parse::<u64>()
        .map_err(|_| ChannelPermissionValidationError::BitmapOverflow { field })
}

fn validate_dispatch_fields<const N: usize>(
    fields: [(&'static str, &str); N],
) -> Result<(), GuildDispatchValidationError> {
    for (field, value) in fields {
        if value.trim().is_empty() {
            return Err(GuildDispatchValidationError { field });
        }
    }
    Ok(())
}

fn validate_optional_dispatch_field(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), GuildDispatchValidationError> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(GuildDispatchValidationError { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelEvent, GuildEvent, GuildRequestValidationError, GuildRoleMemberRequest,
        GuildRoleMutation, RemoveGuildMemberRequest,
    };
    use serde_json::json;

    #[test]
    fn decodes_optional_guild_and_channel_model_fields() {
        let guild: GuildEvent = serde_json::from_value(json!({
            "id":"guild-id",
            "name":"guild",
            "icon":"https://example.com/icon.png",
            "owner_id":"owner-id",
            "owner":true,
            "member_count":12,
            "max_members":1000,
            "description":"description"
        }))
        .unwrap();
        assert_eq!(guild.owner, Some(true));
        assert_eq!(guild.joined_at, None);

        let channel: ChannelEvent = serde_json::from_value(json!({
            "id":"channel-id",
            "guild_id":"guild-id",
            "name":"channel",
            "type":10006,
            "sub_type":0,
            "position":2,
            "parent_id":"parent-id",
            "owner_id":"owner-id",
            "private_type":2,
            "speak_permission":1,
            "application_id":"1000001",
            "permissions":"7"
        }))
        .unwrap();
        assert_eq!(channel.parent_id.as_deref(), Some("parent-id"));
        assert_eq!(channel.private_type, Some(2));
        assert_eq!(channel.speak_permission, Some(1));
        assert_eq!(channel.application_id.as_deref(), Some("1000001"));
        assert_eq!(channel.permissions.as_deref(), Some("7"));
    }

    #[test]
    fn validates_role_mutations_and_member_removal_ranges() {
        assert_eq!(
            GuildRoleMutation::default().validate(),
            Err(GuildRequestValidationError::EmptyRoleMutation)
        );
        assert!(
            GuildRoleMutation {
                name: Some("moderator".to_owned()),
                ..GuildRoleMutation::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            GuildRoleMutation {
                color: Some(0xff00_ff00),
                ..GuildRoleMutation::default()
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            RemoveGuildMemberRequest {
                delete_history_msg_days: 5,
                ..RemoveGuildMemberRequest::default()
            }
            .validate(),
            Err(GuildRequestValidationError::InvalidHistoryDeletionDays { days: 5 })
        );
        assert_eq!(
            GuildRoleMemberRequest::for_channel(" ").validate("5"),
            Err(GuildRequestValidationError::EmptyRoleMemberChannelId)
        );
        assert_eq!(
            GuildRoleMemberRequest::default().validate("5"),
            Err(GuildRequestValidationError::MissingChannelForRoleFive)
        );
        assert!(GuildRoleMemberRequest::default().validate("2").is_ok());
    }
}
