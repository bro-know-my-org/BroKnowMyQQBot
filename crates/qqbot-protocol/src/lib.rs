//! QQ Official Bot API v2 protocol types and clients.

#![forbid(unsafe_code)]

pub mod auth;
pub mod gateway;
pub mod group;
pub mod message;
pub mod openapi;

pub use auth::{AccessToken, AuthError, TokenManager};
pub use gateway::{Gateway, GatewayBot, GatewayPayload, Intents, OpCode, SessionStartLimit};
pub use group::{
    CreateGroupJoinStrategyRequest, GroupJoinApprovalOperation, GroupJoinAutoApproved,
    GroupJoinRequest, GroupJoinRequestEvent, GroupJoinRequestPage, GroupJoinReviewQuestion,
    GroupJoinStrategy, GroupJoinStrategyCreated, GroupJoinStrategyGroupAction,
    GroupJoinStrategyGroupOperation, GroupJoinStrategyPage, GroupJoinStrategySwitch,
    GroupJoinStrategyUpdated, GroupJoinStrategyWhitelistOperation,
    GroupJoinStrategyWhitelistUpdated, GroupJoinVerifyInfo, GroupMuteGlobalRule,
    GroupMuteMemberOperation, GroupMuteOperation, GroupMuteRecurringRule, GroupMuteScheduleRule,
    GroupMuteSetting, GroupMutedMember, PageRequest, ReviewGroupJoinRequest, SetGroupMuteRequest,
    UpdateGroupJoinStrategyRequest, UpdateGroupJoinStrategyWhitelistRequest,
};
pub use message::{
    ChannelMessageRequest, InlineMediaUploadRequest, MediaFileType, MediaUploadRequest,
    MediaUploadResponse, MessageRequest, MessageResponse, MessageType, QqMessage,
};
pub use openapi::{ApiError, OpenApiClient, OpenApiEnvironment};
