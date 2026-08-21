//! Typed QQ robot and group profile responses.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotProfile {
    pub id: String,
    pub username: String,
    pub avatar: String,
    pub bot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_openid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub union_user_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub welcome_msg: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_openid: String,
    pub group_name: String,
    pub group_finger_memo: String,
    pub group_class_text: String,
    pub group_tags: Vec<String>,
    pub group_member_num: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupBotState {
    pub member_openid: String,
    pub joined_at: DateTime<FixedOffset>,
    pub allow_proactive_msg: bool,
    pub recv_msg_setting: String,
    pub member_role: String,
}
