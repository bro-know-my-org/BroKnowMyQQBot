//! QQ group-management protocol types introduced by the 2026-08-10 API update.

use serde::{Deserialize, Serialize};

const MAX_PAGE_LIMIT: u32 = 100;
const MAX_MUTE_MEMBERS: usize = 10;
const MAX_STRATEGY_GROUPS: usize = 100;
const MAX_STRATEGY_REMARK_CHARS: usize = 255;
const MAX_WHITELIST_USERS_PER_REQUEST: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMuteSetting {
    pub global_rule: GroupMuteGlobalRule,
    #[serde(default)]
    pub members: Vec<GroupMutedMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMuteGlobalRule {
    pub mode: String,
    #[serde(default)]
    pub schedule_rules: Vec<GroupMuteScheduleRule>,
    #[serde(default)]
    pub recurring_rules: Vec<GroupMuteRecurringRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMuteScheduleRule {
    pub task_id: String,
    pub start_at: String,
    pub end_at: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMuteRecurringRule {
    pub task_id: String,
    #[serde(default)]
    pub weekdays: Vec<u8>,
    pub start_time: String,
    pub end_time: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMutedMember {
    pub member_openid: String,
    pub mute_expire_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_openid: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupMuteOperation {
    Add,
    Update,
    Del,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMuteMemberOperation {
    pub op: GroupMuteOperation,
    pub member_openid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mute_expire_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetGroupMuteRequest {
    pub members: Vec<GroupMuteMemberOperation>,
}

impl SetGroupMuteRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.members.is_empty() || self.members.len() > MAX_MUTE_MEMBERS {
            return Err("QQ group mute request must contain between 1 and 10 members");
        }
        if self.members.iter().any(|member| {
            member.member_openid.trim().is_empty()
                || match member.op {
                    GroupMuteOperation::Add | GroupMuteOperation::Update => member
                        .mute_expire_at
                        .as_deref()
                        .is_none_or(|value| value.trim().is_empty()),
                    GroupMuteOperation::Del => member
                        .mute_expire_at
                        .as_deref()
                        .is_some_and(|value| !value.is_empty()),
                }
        }) {
            return Err(
                "QQ group mute members require an OpenID and add/update require mute_expire_at",
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

impl PageRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self
            .limit
            .is_some_and(|limit| limit == 0 || limit > MAX_PAGE_LIMIT)
        {
            return Err("QQ page limit must be between 1 and 100");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinRequestPage {
    #[serde(default)]
    pub list: Vec<GroupJoinRequest>,
    #[serde(default)]
    pub next_cursor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinRequest {
    pub join_request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_tips: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_openid: Option<String>,
    pub member_openid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub apply_at: String,
    pub apply_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invited_by: Option<String>,
    #[serde(default)]
    pub bot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_info: Option<GroupJoinVerifyInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approved: Option<GroupJoinAutoApproved>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinRequestEvent {
    pub group_openid: String,
    #[serde(flatten)]
    pub request: GroupJoinRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinVerifyInfo {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_message: Option<String>,
    #[serde(default)]
    pub review_qa_list: Vec<GroupJoinReviewQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinReviewQuestion {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinAutoApproved {
    pub strategy_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupJoinApprovalOperation {
    Approve,
    Decline,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewGroupJoinRequest {
    pub op: GroupJoinApprovalOperation,
    pub join_request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_to_member_blacklist: Option<bool>,
}

impl ReviewGroupJoinRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.join_request_id.trim().is_empty() {
            return Err("QQ group join approval requires join_request_id");
        }
        if matches!(self.op, GroupJoinApprovalOperation::Approve)
            && (self.reject_reason.is_some() || self.add_to_member_blacklist.is_some())
        {
            return Err("QQ group join approval only accepts rejection fields when declining");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupJoinStrategySwitch {
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateGroupJoinStrategyRequest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_openids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_ids: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_enable: Option<GroupJoinStrategySwitch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
}

impl CreateGroupJoinStrategyRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        validate_strategy_groups(&self.group_openids, &self.group_ids)?;
        validate_remark(self.remark.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinStrategyCreated {
    pub strategy_id: String,
    pub is_enable: GroupJoinStrategySwitch,
    pub expire_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinStrategyPage {
    #[serde(default)]
    pub strategies: Vec<GroupJoinStrategy>,
    #[serde(default)]
    pub next_cursor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinStrategy {
    pub strategy_id: String,
    #[serde(default)]
    pub group_openids: Vec<String>,
    #[serde(default)]
    pub group_ids: Vec<u64>,
    pub whitelist_user_count: u64,
    pub is_enable: GroupJoinStrategySwitch,
    pub expire_at: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupJoinStrategyGroupOperation {
    Add,
    Del,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinStrategyGroupAction {
    pub op: GroupJoinStrategyGroupOperation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_openids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_ids: Vec<u64>,
}

impl GroupJoinStrategyGroupAction {
    fn validate(&self) -> Result<(), &'static str> {
        validate_strategy_groups(&self.group_openids, &self.group_ids)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateGroupJoinStrategyRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_enable: Option<GroupJoinStrategySwitch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_action: Option<GroupJoinStrategyGroupAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remark: Option<String>,
}

impl UpdateGroupJoinStrategyRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.is_enable.is_none()
            && self.expire_at.is_none()
            && self.group_action.is_none()
            && self.remark.is_none()
        {
            return Err("QQ group join strategy update must contain at least one field");
        }
        if let Some(action) = &self.group_action {
            action.validate()?;
        }
        validate_remark(self.remark.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinStrategyUpdated {
    pub is_enable: GroupJoinStrategySwitch,
    pub expire_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupJoinStrategyWhitelistOperation {
    Add,
    Del,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateGroupJoinStrategyWhitelistRequest {
    pub op: GroupJoinStrategyWhitelistOperation,
    pub whitelist_users: Vec<String>,
}

impl UpdateGroupJoinStrategyWhitelistRequest {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.whitelist_users.is_empty()
            || self.whitelist_users.len() > MAX_WHITELIST_USERS_PER_REQUEST
            || self.whitelist_users.iter().any(|user| {
                user.parse::<u64>()
                    .ok()
                    .is_none_or(|number| number == 0 || number.to_string() != *user)
            })
        {
            return Err(
                "QQ group join strategy whitelist must contain 1 to 10000 ASCII-decimal QQ numbers",
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupJoinStrategyWhitelistUpdated {
    pub strategy_id: String,
    pub whitelist_user_count: u64,
    pub updated_at: String,
}

fn validate_strategy_groups(
    group_openids: &[String],
    group_ids: &[u64],
) -> Result<(), &'static str> {
    if group_openids.is_empty() == group_ids.is_empty() {
        return Err("QQ group join strategy requires exactly one group identifier form");
    }
    let count = group_openids.len().max(group_ids.len());
    if count > MAX_STRATEGY_GROUPS
        || group_openids.iter().any(|id| id.trim().is_empty())
        || group_ids.contains(&0)
    {
        return Err("QQ group join strategy accepts at most 100 valid group identifiers");
    }
    Ok(())
}

fn validate_remark(remark: Option<&str>) -> Result<(), &'static str> {
    if remark.is_some_and(|value| value.chars().count() > MAX_STRATEGY_REMARK_CHARS) {
        return Err("QQ group join strategy remark exceeds 255 characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CreateGroupJoinStrategyRequest, GroupJoinApprovalOperation, GroupJoinStrategySwitch,
        GroupJoinStrategyWhitelistOperation, GroupMuteMemberOperation, GroupMuteOperation,
        PageRequest, ReviewGroupJoinRequest, SetGroupMuteRequest,
        UpdateGroupJoinStrategyWhitelistRequest,
    };

    #[test]
    fn validates_group_management_request_invariants() {
        assert!(
            PageRequest {
                cursor: None,
                limit: Some(101),
            }
            .validate()
            .is_err()
        );
        for mute_expire_at in [None, Some(String::new())] {
            SetGroupMuteRequest {
                members: vec![GroupMuteMemberOperation {
                    op: GroupMuteOperation::Del,
                    member_openid: "member-openid".to_owned(),
                    mute_expire_at,
                }],
            }
            .validate()
            .unwrap();
        }
        assert!(
            SetGroupMuteRequest {
                members: vec![GroupMuteMemberOperation {
                    op: GroupMuteOperation::Del,
                    member_openid: "member-openid".to_owned(),
                    mute_expire_at: Some("2099-08-11T10:00:00Z".to_owned()),
                }],
            }
            .validate()
            .is_err()
        );
        assert!(
            ReviewGroupJoinRequest {
                op: GroupJoinApprovalOperation::Approve,
                join_request_id: "request-id".to_owned(),
                reject_reason: Some("not allowed".to_owned()),
                add_to_member_blacklist: None,
            }
            .validate()
            .is_err()
        );
        for invalid in ["0", "012345", "18446744073709551616"] {
            assert!(
                UpdateGroupJoinStrategyWhitelistRequest {
                    op: GroupJoinStrategyWhitelistOperation::Add,
                    whitelist_users: vec![invalid.to_owned()],
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            CreateGroupJoinStrategyRequest {
                group_openids: vec!["group-openid".to_owned()],
                group_ids: vec![123],
                is_enable: Some(GroupJoinStrategySwitch::On),
                expire_at: None,
                remark: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            CreateGroupJoinStrategyRequest {
                group_openids: Vec::new(),
                group_ids: vec![0],
                is_enable: Some(GroupJoinStrategySwitch::On),
                expire_at: None,
                remark: None,
            }
            .validate()
            .is_err()
        );
    }
}
