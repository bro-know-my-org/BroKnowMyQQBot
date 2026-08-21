//! QQ Official Bot API v2 protocol types and clients.

#![forbid(unsafe_code)]

pub mod auth;
pub mod gateway;
pub mod group;
pub mod guild;
pub mod message;
pub mod openapi;
pub mod profile;
pub mod reaction;

pub use auth::{AccessToken, AuthError, TokenManager};
pub use gateway::{Gateway, GatewayBot, GatewayPayload, Intents, OpCode, SessionStartLimit};
pub use group::{
    CreateGroupJoinStrategyRequest, GroupJoinApprovalOperation, GroupJoinAutoApproved,
    GroupJoinRequest, GroupJoinRequestEvent, GroupJoinRequestPage, GroupJoinReviewQuestion,
    GroupJoinStrategy, GroupJoinStrategyCreated, GroupJoinStrategyGroupAction,
    GroupJoinStrategyGroupOperation, GroupJoinStrategyPage, GroupJoinStrategySwitch,
    GroupJoinStrategyUpdated, GroupJoinStrategyWhitelistOperation,
    GroupJoinStrategyWhitelistUpdated, GroupJoinVerifyInfo, GroupMemberEvent,
    GroupMemberEventValidationError, GroupMuteGlobalRule, GroupMuteMemberOperation,
    GroupMuteOperation, GroupMuteRecurringRule, GroupMuteScheduleRule, GroupMuteSetting,
    GroupMutedMember, PageRequest, ReviewGroupJoinRequest, SetGroupMuteRequest,
    UpdateGroupJoinStrategyRequest, UpdateGroupJoinStrategyWhitelistRequest,
};
pub use guild::{
    ChannelEvent, ChannelMemberPermissions, ChannelPermissionField,
    ChannelPermissionValidationError, ChannelRolePermissions, CreateGuildRoleResult,
    GuildChannelReference, GuildDispatchValidationError, GuildEvent, GuildMember, GuildMemberEvent,
    GuildMemberPageRequest, GuildRequestValidationError, GuildRole, GuildRoleMemberPage,
    GuildRoleMemberPageRequest, GuildRoleMemberRequest, GuildRoleMutation, GuildRoles, GuildUser,
    OnlineMemberCount, RemoveGuildMemberRequest, UpdateChannelPermissionsRequest,
    UpdateGuildRoleResult,
};
pub use message::{
    ChannelMessageRequest, CreateDirectMessageRequest, DirectMessageSession,
    InlineMediaUploadRequest, MediaFileType, MediaUploadRequest, MediaUploadResponse,
    MessageRequest, MessageResponse, MessageType, QqMessage,
};
pub use openapi::{ApiError, OpenApiClient, OpenApiEnvironment};
pub use profile::{BotProfile, GroupBotState, GroupInfo};
pub use reaction::{
    MessageReactionEvent, ReactionEmoji, ReactionTarget, ReactionUser, ReactionUsersPage,
    ReactionUsersRequest, ReactionValidationError,
};
