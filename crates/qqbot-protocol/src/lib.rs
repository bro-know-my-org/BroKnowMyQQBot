//! QQ Official Bot API v2 protocol types and clients.

#![forbid(unsafe_code)]

pub mod auth;
pub mod channel_content;
pub mod gateway;
pub mod group;
pub mod guild;
pub mod guild_control;
pub mod interaction;
pub mod menu;
pub mod message;
pub mod notice;
pub mod openapi;
pub mod panel;
pub mod profile;
pub mod reaction;

pub use auth::{AccessToken, AuthError, TokenManager};
pub use channel_content::{
    AnnouncementType, ChannelContentValidationError, CreateGuildAnnouncementRequest,
    CreateSchedule, CreateScheduleRequest, EpochMillis, GuildAnnouncement, ListSchedulesQuery,
    PinsMessage, RecommendChannel, Schedule, ScheduleRemindType, UpdateSchedule,
    UpdateScheduleRequest,
};
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
pub use guild_control::{
    GuildApiIdentify, GuildApiPermission, GuildApiPermissionDemand,
    GuildApiPermissionDemandIdentify, GuildApiPermissionDemandRequest, GuildApiPermissionList,
    GuildControlValidationError, GuildMembersMuteRequest, GuildMembersMuteResponse,
    GuildMessageSetting, GuildMuteRequest,
};
pub use interaction::{
    InteractionData, InteractionEvent, InteractionResponseRequest, InteractionValidationError,
};
pub use menu::{
    BotMenu, BotMenuItem, BotMenuItemType, BotMenuResponse, BotMenuSwitch, BotMenuVersion,
    BotSubMenuItem, BotSubMenuItemType, GenerateShareLinkRequest, MenuValidationError, ShareLink,
    ShareLinkValidationError, UpdateBotMenuRequest,
};
pub use message::{
    ChannelMessageRequest, CreateDirectMessageRequest, DirectMessageSession,
    InlineMediaUploadRequest, MediaFileType, MediaUploadRequest, MediaUploadResponse,
    MessageAuthor, MessageRequest, MessageResponse, MessageType, QqMessage,
};
pub use notice::{
    MessageAuditEvent, MessageAuditOutcome, MessageDeleteEvent, NoticeValidationError,
    SubscribeMessageStatusEvent, SubscribeMessageTemplateResult, SubscriptionOperation,
};
pub use openapi::{ApiError, OpenApiClient, OpenApiEnvironment};
pub use panel::{
    CreatePanelRequest, CreatePanelResponse, Panel, PanelDetail, PanelItem, PanelItemType,
    PanelListRequest, PanelPage, PanelRecord, PanelScope, PanelTargetOperation, PanelTargetType,
    PanelValidationError, PanelVersion, UpdatePanelRequest, UpdatePanelTargetsRequest,
};
pub use profile::{BotProfile, GroupBotState, GroupInfo};
pub use reaction::{
    MessageReactionEvent, ReactionEmoji, ReactionTarget, ReactionUser, ReactionUsersPage,
    ReactionUsersRequest, ReactionValidationError,
};
