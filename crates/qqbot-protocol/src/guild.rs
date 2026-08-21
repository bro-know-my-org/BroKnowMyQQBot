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

#[cfg(test)]
mod tests {
    use super::{
        GuildRequestValidationError, GuildRoleMemberRequest, GuildRoleMutation,
        RemoveGuildMemberRequest,
    };

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
