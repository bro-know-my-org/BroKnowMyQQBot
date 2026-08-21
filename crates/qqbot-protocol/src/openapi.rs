//! Minimal QQ `OpenAPI` client required by the WebSocket message loop.

use std::{fmt, time::Duration};

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use url::Url;

use crate::{
    auth::{AuthError, TokenManager},
    gateway::{Gateway, GatewayBot},
    group::{
        CreateGroupJoinStrategyRequest, GroupJoinRequestPage, GroupJoinStrategyCreated,
        GroupJoinStrategyPage, GroupJoinStrategyUpdated, GroupJoinStrategyWhitelistUpdated,
        GroupMuteSetting, PageRequest, ReviewGroupJoinRequest, SetGroupMuteRequest,
        UpdateGroupJoinStrategyRequest, UpdateGroupJoinStrategyWhitelistRequest,
    },
    guild::{
        ChannelMemberPermissions, ChannelPermissionValidationError, ChannelRolePermissions,
        CreateGuildRoleResult, GuildMember, GuildMemberPageRequest, GuildRequestValidationError,
        GuildRoleMemberPage, GuildRoleMemberPageRequest, GuildRoleMemberRequest, GuildRoleMutation,
        GuildRoles, OnlineMemberCount, RemoveGuildMemberRequest, UpdateChannelPermissionsRequest,
        UpdateGuildRoleResult,
    },
    menu::{
        BotMenuResponse, BotMenuVersion, GenerateShareLinkRequest, MenuValidationError, ShareLink,
        ShareLinkValidationError, UpdateBotMenuRequest,
    },
    message::{
        ChannelMessageRequest, CreateDirectMessageRequest, DirectMessageSession,
        InlineMediaUploadRequest, MediaUploadRequest, MediaUploadResponse, MessageRequest,
        MessageResponse,
    },
    profile::{BotProfile, GroupBotState, GroupInfo},
    reaction::{ReactionEmoji, ReactionUsersPage, ReactionUsersRequest, ReactionValidationError},
};

const PRODUCTION_BASE_URL: &str = "https://api.bot.qq.com/";
const SANDBOX_BASE_URL: &str = "https://api.bot.qq.com/";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// QQ `OpenAPI` environment selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenApiEnvironment {
    Production,
    Sandbox,
}

impl OpenApiEnvironment {
    fn base_url(self) -> &'static str {
        match self {
            Self::Production => PRODUCTION_BASE_URL,
            Self::Sandbox => SANDBOX_BASE_URL,
        }
    }
}

/// Errors produced by QQ `OpenAPI` calls.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Authentication(#[from] AuthError),
    #[error("QQ OpenAPI request failed")]
    Request(#[source] reqwest::Error),
    #[error("QQ OpenAPI returned HTTP {status}; code={code:?}, message={message:?}")]
    HttpStatus {
        status: StatusCode,
        code: Option<i64>,
        message: Option<String>,
        trace_id: Option<String>,
        retry_after: Option<Duration>,
    },
    #[error("QQ OpenAPI returned platform error {code}: {message}")]
    Platform {
        code: i64,
        message: String,
        trace_id: Option<String>,
    },
    #[error("QQ OpenAPI response could not be decoded")]
    Decode(#[source] serde_json::Error),
    #[error("QQ OpenAPI response exceeds one MiB")]
    ResponseTooLarge,
    #[error("failed to construct QQ OpenAPI URL: {0}")]
    InvalidUrl(String),
    #[error("invalid QQ OpenAPI request: {0}")]
    InvalidRequest(String),
    #[error("invalid QQ guild request: {0}")]
    InvalidGuildRequest(#[source] GuildRequestValidationError),
    #[error("invalid QQ channel permission request: {0}")]
    InvalidChannelPermissionRequest(#[source] ChannelPermissionValidationError),
    #[error("invalid QQ reaction request: {0}")]
    InvalidReactionRequest(#[source] ReactionValidationError),
    #[error("invalid QQ bot menu request: {0}")]
    InvalidMenuRequest(#[source] MenuValidationError),
    #[error("invalid QQ share-link request: {0}")]
    InvalidShareLinkRequest(#[source] ShareLinkValidationError),
}

/// Authenticated QQ `OpenAPI` client.
#[derive(Clone)]
pub struct OpenApiClient {
    client: Client,
    base_url: Url,
    tokens: TokenManager,
}

impl fmt::Debug for OpenApiClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenApiClient")
            .field("base_url", &self.base_url)
            .field("tokens", &self.tokens)
            .finish_non_exhaustive()
    }
}

impl OpenApiClient {
    pub fn new(environment: OpenApiEnvironment, tokens: TokenManager) -> Result<Self, ApiError> {
        let base_url = Url::parse(environment.base_url())
            .map_err(|error| ApiError::InvalidUrl(error.to_string()))?;
        Self::with_base_url(base_url, tokens)
    }

    pub fn with_base_url(base_url: Url, tokens: TokenManager) -> Result<Self, ApiError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(ApiError::Request)?;
        Ok(Self {
            client,
            base_url,
            tokens,
        })
    }

    pub async fn gateway(&self) -> Result<Gateway, ApiError> {
        let url = self.endpoint(&["gateway"])?;
        self.get_json(url).await
    }

    pub async fn access_token(&self) -> Result<crate::AccessToken, ApiError> {
        self.tokens.access_token().await.map_err(ApiError::from)
    }

    pub async fn gateway_bot(&self) -> Result<GatewayBot, ApiError> {
        let url = self.endpoint(&["gateway", "bot"])?;
        self.get_json(url).await
    }

    pub async fn bot_profile(&self) -> Result<BotProfile, ApiError> {
        let url = self.endpoint(&["users", "@me"])?;
        self.get_json(url).await
    }

    pub async fn group_info(&self, group_openid: &str) -> Result<GroupInfo, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "info"])?;
        self.get_json(url).await
    }

    pub async fn group_bot_state(&self, group_openid: &str) -> Result<GroupBotState, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "bot_state"])?;
        self.get_json(url).await
    }

    pub async fn generate_share_link(
        &self,
        request: &GenerateShareLinkRequest,
    ) -> Result<ShareLink, ApiError> {
        request
            .validate()
            .map_err(ApiError::InvalidShareLinkRequest)?;
        let url = self.endpoint(&["v2", "generate_url_link"])?;
        self.post_json(url, request).await
    }

    pub async fn bot_menu(&self) -> Result<BotMenuResponse, ApiError> {
        let url = self.endpoint(&["v2", "menu"])?;
        self.get_json(url).await
    }

    pub async fn update_bot_menu(
        &self,
        request: &UpdateBotMenuRequest,
    ) -> Result<BotMenuVersion, ApiError> {
        request.validate().map_err(ApiError::InvalidMenuRequest)?;
        let url = self.endpoint(&["v2", "menu"])?;
        self.put_json(url, request).await
    }

    pub async fn send_c2c_message(
        &self,
        user_openid: &str,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "users", user_openid, "messages"])?;
        self.post_json(url, request).await
    }

    pub async fn send_group_message(
        &self,
        group_openid: &str,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", group_openid, "messages"])?;
        self.post_json(url, request).await
    }

    pub async fn send_channel_message(
        &self,
        channel_id: &str,
        request: &ChannelMessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["channels", channel_id, "messages"])?;
        self.post_json(url, request).await
    }

    pub async fn create_direct_message_session(
        &self,
        request: &CreateDirectMessageRequest,
    ) -> Result<DirectMessageSession, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["users", "@me", "dms"])?;
        self.post_json(url, request).await
    }

    pub async fn send_direct_message(
        &self,
        guild_id: &str,
        request: &ChannelMessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["dms", guild_id, "messages"])?;
        self.post_json(url, request).await
    }

    pub async fn upload_c2c_media(
        &self,
        user_openid: &str,
        request: &MediaUploadRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "users", user_openid, "files"])?;
        self.post_json(url, request).await
    }

    pub async fn upload_group_media(
        &self,
        group_openid: &str,
        request: &MediaUploadRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", group_openid, "files"])?;
        self.post_json(url, request).await
    }

    pub async fn upload_c2c_inline_media(
        &self,
        user_openid: &str,
        request: &InlineMediaUploadRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        let url = self.endpoint(&["v2", "users", user_openid, "files"])?;
        self.post_json(url, request).await
    }

    pub async fn upload_group_inline_media(
        &self,
        group_openid: &str,
        request: &InlineMediaUploadRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        let url = self.endpoint(&["v2", "groups", group_openid, "files"])?;
        self.post_json(url, request).await
    }

    pub async fn recall_c2c_message(
        &self,
        user_openid: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        let url = self.endpoint(&["v2", "users", user_openid, "messages", message_id])?;
        self.delete(url).await
    }

    pub async fn recall_group_message(
        &self,
        group_openid: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        let url = self.endpoint(&["v2", "groups", group_openid, "messages", message_id])?;
        self.delete(url).await
    }

    pub async fn recall_channel_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        let url = self.endpoint(&["channels", channel_id, "messages", message_id])?;
        self.delete(url).await
    }

    pub async fn recall_direct_message(
        &self,
        guild_id: &str,
        message_id: &str,
        hide_tip: bool,
    ) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("message_id", message_id)?;
        let mut url = self.endpoint(&["dms", guild_id, "messages", message_id])?;
        url.query_pairs_mut()
            .append_pair("hidetip", if hide_tip { "true" } else { "false" });
        self.delete(url).await
    }

    pub async fn guilds(&self) -> Result<serde_json::Value, ApiError> {
        let url = self.endpoint(&["users", "@me", "guilds"])?;
        self.get_json(url).await
    }

    pub async fn guild(&self, guild_id: &str) -> Result<serde_json::Value, ApiError> {
        let url = self.endpoint(&["guilds", guild_id])?;
        self.get_json(url).await
    }

    pub async fn guild_channels(&self, guild_id: &str) -> Result<serde_json::Value, ApiError> {
        let url = self.endpoint(&["guilds", guild_id, "channels"])?;
        self.get_json(url).await
    }

    pub async fn channel(&self, channel_id: &str) -> Result<serde_json::Value, ApiError> {
        let url = self.endpoint(&["channels", channel_id])?;
        self.get_json(url).await
    }

    /// Sends a raw QQ channel-create document for fields not yet modeled by this crate.
    pub async fn create_channel_raw(
        &self,
        guild_id: &str,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        validate_channel_document(request, true)
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["guilds", guild_id, "channels"])?;
        self.post_json(url, request).await
    }

    /// Sends a raw QQ channel-update document for fields not yet modeled by this crate.
    pub async fn update_channel_raw(
        &self,
        channel_id: &str,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        validate_channel_document(request, false)
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["channels", channel_id])?;
        self.patch_json(url, request).await
    }

    pub async fn delete_channel(&self, channel_id: &str) -> Result<(), ApiError> {
        let url = self.endpoint(&["channels", channel_id])?;
        self.delete(url).await
    }

    pub async fn channel_online_member_count(
        &self,
        channel_id: &str,
    ) -> Result<OnlineMemberCount, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id, "online_nums"])?;
        self.get_json(url).await
    }

    pub async fn guild_members(
        &self,
        guild_id: &str,
        request: &GuildMemberPageRequest,
    ) -> Result<Vec<GuildMember>, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request.validate().map_err(ApiError::InvalidGuildRequest)?;
        let mut url = self.endpoint(&["guilds", guild_id, "members"])?;
        url.query_pairs_mut()
            .append_pair("after", &request.after)
            .append_pair("limit", &request.limit.to_string());
        self.get_json(url).await
    }

    pub async fn guild_role_members(
        &self,
        guild_id: &str,
        role_id: &str,
        request: &GuildRoleMemberPageRequest,
    ) -> Result<GuildRoleMemberPage, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("role_id", role_id)?;
        request.validate().map_err(ApiError::InvalidGuildRequest)?;
        let mut url = self.endpoint(&["guilds", guild_id, "roles", role_id, "members"])?;
        url.query_pairs_mut()
            .append_pair("start_index", &request.start_index)
            .append_pair("limit", &request.limit.to_string());
        self.get_json(url).await
    }

    pub async fn guild_member(
        &self,
        guild_id: &str,
        user_id: &str,
    ) -> Result<GuildMember, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("user_id", user_id)?;
        let url = self.endpoint(&["guilds", guild_id, "members", user_id])?;
        self.get_json(url).await
    }

    pub async fn remove_guild_member(
        &self,
        guild_id: &str,
        user_id: &str,
        request: &RemoveGuildMemberRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("user_id", user_id)?;
        request.validate().map_err(ApiError::InvalidGuildRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "members", user_id])?;
        self.delete_json_unit(url, request).await
    }

    pub async fn guild_roles(&self, guild_id: &str) -> Result<GuildRoles, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        let url = self.endpoint(&["guilds", guild_id, "roles"])?;
        self.get_json(url).await
    }

    pub async fn create_guild_role(
        &self,
        guild_id: &str,
        request: &GuildRoleMutation,
    ) -> Result<CreateGuildRoleResult, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request.validate().map_err(ApiError::InvalidGuildRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "roles"])?;
        self.post_json(url, request).await
    }

    pub async fn update_guild_role(
        &self,
        guild_id: &str,
        role_id: &str,
        request: &GuildRoleMutation,
    ) -> Result<UpdateGuildRoleResult, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("role_id", role_id)?;
        request.validate().map_err(ApiError::InvalidGuildRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "roles", role_id])?;
        self.patch_json(url, request).await
    }

    pub async fn delete_guild_role(&self, guild_id: &str, role_id: &str) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("role_id", role_id)?;
        let url = self.endpoint(&["guilds", guild_id, "roles", role_id])?;
        self.delete(url).await
    }

    pub async fn add_guild_role_member(
        &self,
        guild_id: &str,
        user_id: &str,
        role_id: &str,
        request: &GuildRoleMemberRequest,
    ) -> Result<(), ApiError> {
        validate_guild_role_member_request(guild_id, user_id, role_id, request)?;
        let url = self.endpoint(&["guilds", guild_id, "members", user_id, "roles", role_id])?;
        self.put_json_unit(url, request).await
    }

    pub async fn remove_guild_role_member(
        &self,
        guild_id: &str,
        user_id: &str,
        role_id: &str,
        request: &GuildRoleMemberRequest,
    ) -> Result<(), ApiError> {
        validate_guild_role_member_request(guild_id, user_id, role_id, request)?;
        let url = self.endpoint(&["guilds", guild_id, "members", user_id, "roles", role_id])?;
        self.delete_json_unit(url, request).await
    }

    pub async fn channel_member_permissions(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelMemberPermissions, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("user_id", user_id)?;
        let url = self.endpoint(&["channels", channel_id, "members", user_id, "permissions"])?;
        self.get_json(url).await
    }

    pub async fn update_channel_member_permissions(
        &self,
        channel_id: &str,
        user_id: &str,
        request: &UpdateChannelPermissionsRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("user_id", user_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidChannelPermissionRequest)?;
        let url = self.endpoint(&["channels", channel_id, "members", user_id, "permissions"])?;
        self.put_json_unit(url, request).await
    }

    pub async fn channel_role_permissions(
        &self,
        channel_id: &str,
        role_id: &str,
    ) -> Result<ChannelRolePermissions, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("role_id", role_id)?;
        let url = self.endpoint(&["channels", channel_id, "roles", role_id, "permissions"])?;
        self.get_json(url).await
    }

    pub async fn update_channel_role_permissions(
        &self,
        channel_id: &str,
        role_id: &str,
        request: &UpdateChannelPermissionsRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("role_id", role_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidChannelPermissionRequest)?;
        let url = self.endpoint(&["channels", channel_id, "roles", role_id, "permissions"])?;
        self.put_json_unit(url, request).await
    }

    pub async fn add_channel_message_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &ReactionEmoji,
    ) -> Result<(), ApiError> {
        let url = self.reaction_endpoint(channel_id, message_id, emoji)?;
        self.put(url).await
    }

    pub async fn remove_channel_message_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &ReactionEmoji,
    ) -> Result<(), ApiError> {
        let url = self.reaction_endpoint(channel_id, message_id, emoji)?;
        self.delete(url).await
    }

    pub async fn channel_message_reaction_users(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &ReactionEmoji,
        request: &ReactionUsersRequest,
    ) -> Result<ReactionUsersPage, ApiError> {
        let mut url = self.reaction_endpoint(channel_id, message_id, emoji)?;
        request
            .validate()
            .map_err(ApiError::InvalidReactionRequest)?;
        if let Some(cookie) = request.cookie.as_deref() {
            url.query_pairs_mut().append_pair("cookie", cookie);
        }
        if let Some(limit) = request.limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        self.get_json(url).await
    }

    fn reaction_endpoint(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &ReactionEmoji,
    ) -> Result<Url, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("message_id", message_id)?;
        emoji.validate().map_err(ApiError::InvalidReactionRequest)?;
        self.endpoint(&[
            "channels",
            channel_id,
            "messages",
            message_id,
            "reactions",
            &emoji.emoji_type.to_string(),
            &emoji.id,
        ])
    }

    pub async fn group_mute_setting(
        &self,
        group_openid: &str,
    ) -> Result<GroupMuteSetting, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "restrict_chat_setting"])?;
        self.get_json(url).await
    }

    pub async fn set_group_mute(
        &self,
        group_openid: &str,
        request: &SetGroupMuteRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("group_openid", group_openid)?;
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", group_openid, "restrict_chat_setting"])?;
        self.post_json_unit(url, request).await
    }

    pub async fn group_join_requests(
        &self,
        group_openid: &str,
        request: &PageRequest,
    ) -> Result<GroupJoinRequestPage, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", group_openid, "join_request_list"])?;
        // The QQ 2026-08-10 generated contract explicitly defines cursor and
        // limit as a JSON request body for this GET endpoint.
        self.get_json_body(url, request).await
    }

    pub async fn review_group_join_request(
        &self,
        group_openid: &str,
        member_openid: &str,
        request: &ReviewGroupJoinRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("group_openid", group_openid)?;
        validate_path_id("member_openid", member_openid)?;
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&[
            "v2",
            "groups",
            group_openid,
            "approval_join_request",
            member_openid,
        ])?;
        self.post_json_unit(url, request).await
    }

    pub async fn create_group_join_strategy(
        &self,
        request: &CreateGroupJoinStrategyRequest,
    ) -> Result<GroupJoinStrategyCreated, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", "join_approval_strategy"])?;
        self.post_json(url, request).await
    }

    pub async fn group_join_strategies(
        &self,
        request: &PageRequest,
    ) -> Result<GroupJoinStrategyPage, ApiError> {
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", "join_approval_strategy"])?;
        // QQ's generated contract also defines pagination as a GET JSON body.
        self.get_json_body(url, request).await
    }

    pub async fn update_group_join_strategy(
        &self,
        strategy_id: &str,
        request: &UpdateGroupJoinStrategyRequest,
    ) -> Result<GroupJoinStrategyUpdated, ApiError> {
        validate_path_id("strategy_id", strategy_id)?;
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&["v2", "groups", "join_approval_strategy", strategy_id])?;
        self.patch_json(url, request).await
    }

    pub async fn execute_group_join_strategy(&self, strategy_id: &str) -> Result<(), ApiError> {
        validate_path_id("strategy_id", strategy_id)?;
        let url = self.endpoint(&[
            "v2",
            "groups",
            "join_approval_strategy",
            strategy_id,
            "execute",
        ])?;
        self.post_unit(url).await
    }

    pub async fn update_group_join_strategy_whitelist(
        &self,
        strategy_id: &str,
        request: &UpdateGroupJoinStrategyWhitelistRequest,
    ) -> Result<GroupJoinStrategyWhitelistUpdated, ApiError> {
        validate_path_id("strategy_id", strategy_id)?;
        request
            .validate()
            .map_err(|message| ApiError::InvalidRequest(message.to_owned()))?;
        let url = self.endpoint(&[
            "v2",
            "groups",
            "join_approval_strategy",
            strategy_id,
            "whitelist_users",
        ])?;
        self.post_json(url, request).await
    }

    pub async fn delete_group_join_strategy(&self, strategy_id: &str) -> Result<(), ApiError> {
        validate_path_id("strategy_id", strategy_id)?;
        let url = self.endpoint(&["v2", "groups", "join_approval_strategy", strategy_id])?;
        self.delete(url).await
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, ApiError> {
        let mut url = self.base_url.clone();
        let mut path = url.path_segments_mut().map_err(|()| {
            ApiError::InvalidUrl("base URL cannot be used for path segments".to_owned())
        })?;
        path.pop_if_empty();
        path.extend(segments);
        drop(path);
        Ok(url)
    }

    async fn get_json<T>(&self, url: Url) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
    {
        let response = self
            .send_authorized(|token| self.client.get(url.clone()).qqbot_token(token))
            .await?;
        Self::decode(response).await
    }

    async fn get_json_body<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.get(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode(response).await
    }

    async fn post_json<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.post(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode(response).await
    }

    async fn post_json_unit<B>(&self, url: Url, body: &B) -> Result<(), ApiError>
    where
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.post(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode_unit(response).await
    }

    async fn post_unit(&self, url: Url) -> Result<(), ApiError> {
        let response = self
            .send_authorized(|token| self.client.post(url.clone()).qqbot_token(token))
            .await?;
        Self::decode_unit(response).await
    }

    async fn patch_json<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.patch(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode(response).await
    }

    async fn put_json_unit<B>(&self, url: Url, body: &B) -> Result<(), ApiError>
    where
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.put(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode_unit(response).await
    }

    async fn put_json<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.put(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode(response).await
    }

    async fn put(&self, url: Url) -> Result<(), ApiError> {
        let response = self
            .send_authorized(|token| self.client.put(url.clone()).qqbot_token(token))
            .await?;
        Self::decode_unit(response).await
    }

    async fn delete_json_unit<B>(&self, url: Url, body: &B) -> Result<(), ApiError>
    where
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| {
                self.client
                    .delete(url.clone())
                    .qqbot_token(token)
                    .json(body)
            })
            .await?;
        Self::decode_unit(response).await
    }

    async fn delete(&self, url: Url) -> Result<(), ApiError> {
        let response = self
            .send_authorized(|token| self.client.delete(url.clone()).qqbot_token(token))
            .await?;
        Self::decode_unit(response).await
    }

    async fn decode_unit(response: Response) -> Result<(), ApiError> {
        let bytes = Self::decode_bytes(response).await?;
        if bytes.is_empty() {
            return Ok(());
        }
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map(|_| ())
            .map_err(ApiError::Decode)
    }

    async fn send_authorized<F>(&self, build: F) -> Result<Response, ApiError>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let token = self.tokens.access_token().await?;
        let response = build(token.expose())
            .send()
            .await
            .map_err(ApiError::Request)?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }

        let replacement = self.tokens.refresh_if_current(&token).await?;
        build(replacement.expose())
            .send()
            .await
            .map_err(ApiError::Request)
    }

    async fn decode<T>(response: Response) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
    {
        let bytes = Self::decode_bytes(response).await?;
        serde_json::from_slice(&bytes).map_err(ApiError::Decode)
    }

    async fn decode_bytes(mut response: Response) -> Result<Vec<u8>, ApiError> {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let trace_header = response
            .headers()
            .get("x-tps-trace-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ApiError::ResponseTooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(ApiError::Request)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ApiError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        let error_body = serde_json::from_slice::<ErrorBody>(&bytes).ok();

        if !status.is_success() {
            return Err(ApiError::HttpStatus {
                status,
                code: error_body.as_ref().and_then(ErrorBody::code),
                message: error_body.as_ref().and_then(|body| body.message.clone()),
                trace_id: error_body
                    .as_ref()
                    .and_then(|body| body.trace_id.clone())
                    .or(trace_header),
                retry_after,
            });
        }

        if let Some(body) = error_body.as_ref() {
            if let Some(code) = body.code() {
                if code != 0 {
                    return Err(ApiError::Platform {
                        code,
                        message: body
                            .message
                            .clone()
                            .unwrap_or_else(|| "unknown QQ platform error".to_owned()),
                        trace_id: body.trace_id.clone().or(trace_header),
                    });
                }
            }
        }

        Ok(bytes)
    }
}

fn validate_channel_document(
    request: &serde_json::Value,
    require_name: bool,
) -> Result<(), &'static str> {
    let object = request
        .as_object()
        .ok_or("QQ channel request must be a JSON object")?;
    if object.is_empty() {
        return Err("QQ channel request must contain at least one field");
    }
    if let Some(name) = request.get("name") {
        if name.as_str().is_none_or(str::is_empty) {
            return Err("QQ channel name must be a non-empty string");
        }
    } else if require_name {
        return Err("QQ channel create request must contain a non-empty name");
    }
    Ok(())
}

fn validate_path_id(field: &str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::InvalidRequest(format!(
            "QQ OpenAPI path field `{field}` must not be empty"
        )));
    }
    Ok(())
}

fn validate_guild_role_member_request(
    guild_id: &str,
    user_id: &str,
    role_id: &str,
    request: &GuildRoleMemberRequest,
) -> Result<(), ApiError> {
    validate_path_id("guild_id", guild_id)?;
    validate_path_id("user_id", user_id)?;
    validate_path_id("role_id", role_id)?;
    request
        .validate(role_id)
        .map_err(ApiError::InvalidGuildRequest)
}

trait RequestBuilderExt {
    fn qqbot_token(self, token: &str) -> Self;
}

impl RequestBuilderExt for reqwest::RequestBuilder {
    fn qqbot_token(self, token: &str) -> Self {
        self.header(reqwest::header::AUTHORIZATION, format!("QQBot {token}"))
    }
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    err_code: Option<i64>,
    #[serde(default, alias = "msg")]
    message: Option<String>,
    #[serde(default, alias = "traceId")]
    trace_id: Option<String>,
}

impl ErrorBody {
    fn code(&self) -> Option<i64> {
        self.err_code.or(self.code)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize};

    use axum::{
        Json, Router,
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{delete, get, post},
    };
    use reqwest::Client;
    use secrecy::SecretString;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use url::Url;

    use crate::{
        ChannelMessageRequest, CreateDirectMessageRequest, CreateGroupJoinStrategyRequest,
        GroupJoinApprovalOperation, GroupJoinStrategyGroupAction, GroupJoinStrategyGroupOperation,
        GroupJoinStrategySwitch, GroupJoinStrategyWhitelistOperation, GroupMuteMemberOperation,
        GroupMuteOperation, MediaFileType, MediaUploadRequest, MessageRequest, PageRequest,
        ReviewGroupJoinRequest, SetGroupMuteRequest, UpdateGroupJoinStrategyRequest,
        UpdateGroupJoinStrategyWhitelistRequest, auth::TokenManager,
    };

    use super::{ApiError, OpenApiClient, OpenApiEnvironment};

    #[derive(Clone)]
    struct StateData {
        token_calls: Arc<AtomicUsize>,
    }

    async fn token(State(state): State<StateData>) -> Json<Value> {
        let call = state
            .token_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Json(json!({
            "access_token": format!("token-{call}"),
            "expires_in": 7200
        }))
    }

    async fn gateway(headers: HeaderMap) -> (StatusCode, Json<Value>) {
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some("QQBot token-0")
        {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"code":11244,"message":"unauthorized"})),
            );
        }
        (StatusCode::OK, Json(json!({"url":"ws://gateway.example"})))
    }

    async fn group_message(
        Path(group): Path<String>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(group, "group/id");
        assert_eq!(headers["authorization"], "QQBot token-0");
        assert_eq!(body["content"], "pong");
        assert_eq!(body["msg_id"], "source-message");
        (StatusCode::OK, Json(json!({"id":"sent-message"})))
    }

    async fn group_media(Path(group): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(group, "group/id");
        assert_eq!(body["file_type"], 1);
        assert_eq!(body["url"], "https://example.com/image.png");
        Json(json!({
            "file_uuid":"file-uuid",
            "file_info":"file-info",
            "ttl":300
        }))
    }

    async fn c2c_media(Path(user): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(user, "user/id");
        assert_eq!(body["file_type"], 1);
        Json(json!({
            "file_uuid":"c2c-file-uuid",
            "file_info":"c2c-file-info"
        }))
    }

    async fn recall_group_message(Path((group, message)): Path<(String, String)>) -> StatusCode {
        assert_eq!(group, "group/id");
        assert_eq!(message, "message/id");
        StatusCode::NO_CONTENT
    }

    async fn recall_c2c_message(Path((user, message)): Path<(String, String)>) -> StatusCode {
        assert_eq!(user, "user/id");
        assert_eq!(message, "message/id");
        StatusCode::NO_CONTENT
    }

    async fn recall_channel_message(
        Path((channel, message)): Path<(String, String)>,
    ) -> Json<Value> {
        assert_eq!(channel, "channel/id");
        assert_eq!(message, "message/id");
        Json(json!({"ok":true}))
    }

    async fn guilds() -> Json<Value> {
        Json(json!([{"id":"guild/id"}]))
    }

    async fn guild(Path(guild): Path<String>) -> Json<Value> {
        assert_eq!(guild, "guild/id");
        Json(json!({"id":guild}))
    }

    async fn guild_channels(Path(guild): Path<String>) -> Json<Value> {
        assert_eq!(guild, "guild/id");
        Json(json!([{"id":"channel/id"}]))
    }

    async fn create_channel(Path(guild): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(guild, "guild/id");
        assert_eq!(body["name"], "alerts");
        Json(json!({"id":"channel/id","name":body["name"]}))
    }

    async fn channel(Path(channel): Path<String>) -> Json<Value> {
        assert_eq!(channel, "channel/id");
        Json(json!({"id":channel}))
    }

    async fn update_channel(Path(channel): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(channel, "channel/id");
        assert_eq!(body["name"], "renamed");
        Json(json!({"id":channel,"name":body["name"]}))
    }

    async fn delete_channel(Path(channel): Path<String>) -> StatusCode {
        assert_eq!(channel, "channel/id");
        StatusCode::NO_CONTENT
    }

    async fn create_direct_message(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["recipient_id"], "recipient/id");
        assert_eq!(body["source_guild_id"], "source/guild");
        Json(json!({
            "guild_id":"direct/guild",
            "channel_id":"direct/channel",
            "create_time":"2099-08-10T10:00:00Z"
        }))
    }

    async fn send_direct_message(
        Path(guild): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(guild, "direct/guild");
        assert_eq!(body["content"], "private pong");
        assert_eq!(body["msg_id"], "source-message");
        Json(json!({"id":"direct-message"}))
    }

    async fn recall_direct_message(
        Path((guild, message)): Path<(String, String)>,
        Query(query): Query<std::collections::HashMap<String, String>>,
    ) -> StatusCode {
        assert_eq!(guild, "direct/guild");
        assert_eq!(message, "direct/message");
        assert_eq!(query.get("hidetip").map(String::as_str), Some("true"));
        StatusCode::NO_CONTENT
    }

    async fn group_mute_setting(Path(group): Path<String>) -> Json<Value> {
        assert_eq!(group, "group/id");
        Json(json!({
            "global_rule": {
                "mode": "none",
                "schedule_rules": [],
                "recurring_rules": []
            },
            "members": []
        }))
    }

    async fn set_group_mute(Path(group): Path<String>, Json(body): Json<Value>) -> StatusCode {
        assert_eq!(group, "group/id");
        assert_eq!(body["members"][0]["op"], "add");
        assert_eq!(body["members"][0]["member_openid"], "member/id");
        StatusCode::NO_CONTENT
    }

    async fn group_join_requests(
        Path(group): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(group, "group/id");
        assert_eq!(body["limit"], 20);
        Json(json!({
            "list": [{
                "join_request_id":"join/id",
                "member_openid":"member/id",
                "apply_at":"2026-08-10T10:00:00Z",
                "apply_source":"self_apply",
                "bot":false
            }],
            "next_cursor":"next"
        }))
    }

    async fn approve_group_join_request(
        Path((group, member)): Path<(String, String)>,
        Json(body): Json<Value>,
    ) -> StatusCode {
        assert_eq!(group, "group/id");
        assert_eq!(member, "member/id");
        assert_eq!(body["op"], "approve");
        assert_eq!(body["join_request_id"], "join/id");
        StatusCode::NO_CONTENT
    }

    async fn group_join_strategies(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["limit"], 20);
        Json(json!({
            "strategies": [{
                "strategy_id":"strategy/id",
                "group_openids":["group/id"],
                "group_ids":[],
                "whitelist_user_count":2,
                "is_enable":"on",
                "expire_at":"2027-08-10T10:00:00Z",
                "created_at":"2026-08-10T10:00:00Z",
                "updated_at":"2026-08-10T10:00:00Z"
            }],
            "next_cursor":""
        }))
    }

    async fn create_group_join_strategy(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["group_openids"][0], "group/id");
        Json(json!({
            "strategy_id":"strategy/id",
            "is_enable":"on",
            "expire_at":"2027-08-10T10:00:00Z"
        }))
    }

    async fn update_group_join_strategy(
        Path(strategy): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(strategy, "strategy/id");
        assert_eq!(body["group_action"]["op"], "add");
        Json(json!({
            "is_enable":"on",
            "expire_at":"2027-08-10T10:00:00Z"
        }))
    }

    async fn delete_group_join_strategy(Path(strategy): Path<String>) -> StatusCode {
        assert_eq!(strategy, "strategy/id");
        StatusCode::NO_CONTENT
    }

    async fn execute_group_join_strategy(Path(strategy): Path<String>) -> StatusCode {
        assert_eq!(strategy, "strategy/id");
        StatusCode::ACCEPTED
    }

    async fn update_group_join_strategy_whitelist(
        Path(strategy): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(strategy, "strategy/id");
        assert_eq!(body["op"], "add");
        assert_eq!(body["whitelist_users"][0], "123456");
        Json(json!({
            "strategy_id":"strategy/id",
            "whitelist_user_count":1,
            "updated_at":"2026-08-10T10:00:00Z"
        }))
    }

    async fn client() -> (OpenApiClient, Arc<AtomicUsize>) {
        let token_calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/app/getAppAccessToken", post(token))
            .route("/gateway", get(gateway))
            .route("/v2/groups/{group}/messages", post(group_message))
            .route("/v2/groups/{group}/files", post(group_media))
            .route("/v2/users/{user}/files", post(c2c_media))
            .route(
                "/v2/groups/{group}/messages/{message}",
                delete(recall_group_message),
            )
            .route(
                "/v2/users/{user}/messages/{message}",
                delete(recall_c2c_message),
            )
            .route(
                "/channels/{channel}/messages/{message}",
                delete(recall_channel_message),
            )
            .route("/users/@me/dms", post(create_direct_message))
            .route("/dms/{guild}/messages", post(send_direct_message))
            .route(
                "/dms/{guild}/messages/{message}",
                delete(recall_direct_message),
            )
            .route("/users/@me/guilds", get(guilds))
            .route("/guilds/{guild}", get(guild))
            .route(
                "/guilds/{guild}/channels",
                get(guild_channels).post(create_channel),
            )
            .route(
                "/channels/{channel}",
                get(channel).patch(update_channel).delete(delete_channel),
            )
            .route(
                "/v2/groups/{group}/restrict_chat_setting",
                get(group_mute_setting).post(set_group_mute),
            )
            .route(
                "/v2/groups/{group}/join_request_list",
                get(group_join_requests),
            )
            .route(
                "/v2/groups/{group}/approval_join_request/{member}",
                post(approve_group_join_request),
            )
            .route(
                "/v2/groups/join_approval_strategy",
                get(group_join_strategies).post(create_group_join_strategy),
            )
            .route(
                "/v2/groups/join_approval_strategy/{strategy}",
                delete(delete_group_join_strategy).patch(update_group_join_strategy),
            )
            .route(
                "/v2/groups/join_approval_strategy/{strategy}/execute",
                post(execute_group_join_strategy),
            )
            .route(
                "/v2/groups/join_approval_strategy/{strategy}/whitelist_users",
                post(update_group_join_strategy_whitelist),
            )
            .with_state(StateData {
                token_calls: Arc::clone(&token_calls),
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let base = Url::parse(&format!("http://{address}/")).unwrap();
        let token_endpoint = base.join("app/getAppAccessToken").unwrap();
        let tokens = TokenManager::with_client_and_endpoint(
            Client::new(),
            token_endpoint,
            "app-id",
            SecretString::from("app-secret".to_owned().into_boxed_str()),
        );
        (
            OpenApiClient::with_base_url(base, tokens).unwrap(),
            token_calls,
        )
    }

    #[test]
    fn official_environments_use_the_unified_august_api_host() {
        assert_eq!(
            OpenApiEnvironment::Production.base_url(),
            "https://api.bot.qq.com/"
        );
        assert_eq!(
            OpenApiEnvironment::Sandbox.base_url(),
            "https://api.bot.qq.com/"
        );
    }

    #[tokio::test]
    async fn gets_gateway_and_sends_group_reply_with_escaped_id() {
        let (client, token_calls) = client().await;
        let gateway = client.gateway().await.unwrap();
        assert_eq!(gateway.url, "ws://gateway.example");

        let response = client
            .send_group_message(
                "group/id",
                &MessageRequest::reply_text("source-message", "pong"),
            )
            .await
            .unwrap();
        assert_eq!(response.id.as_deref(), Some("sent-message"));
        let upload = client
            .upload_group_media(
                "group/id",
                &MediaUploadRequest::from_url(
                    MediaFileType::IMAGE,
                    "https://example.com/image.png",
                ),
            )
            .await
            .unwrap();
        assert_eq!(upload.file_info, "file-info");
        let c2c_upload = client
            .upload_c2c_media(
                "user/id",
                &MediaUploadRequest::from_url(
                    MediaFileType::IMAGE,
                    "https://example.com/image.png",
                ),
            )
            .await
            .unwrap();
        assert_eq!(c2c_upload.file_info, "c2c-file-info");
        client
            .recall_group_message("group/id", "message/id")
            .await
            .unwrap();
        client
            .recall_c2c_message("user/id", "message/id")
            .await
            .unwrap();
        client
            .recall_channel_message("channel/id", "message/id")
            .await
            .unwrap();
        assert_eq!(client.guilds().await.unwrap()[0]["id"], "guild/id");
        assert_eq!(client.guild("guild/id").await.unwrap()["id"], "guild/id");
        assert_eq!(
            client.guild_channels("guild/id").await.unwrap()[0]["id"],
            "channel/id"
        );
        assert_eq!(
            client
                .create_channel_raw("guild/id", &json!({"name":"alerts"}))
                .await
                .unwrap()["name"],
            "alerts"
        );
        assert_eq!(
            client.channel("channel/id").await.unwrap()["id"],
            "channel/id"
        );
        assert_eq!(
            client
                .update_channel_raw("channel/id", &json!({"name":"renamed"}))
                .await
                .unwrap()["name"],
            "renamed"
        );
        client.delete_channel("channel/id").await.unwrap();
        assert!(matches!(
            client.create_channel_raw("guild/id", &json!({})).await,
            Err(ApiError::InvalidRequest(_))
        ));
        assert!(matches!(
            client.update_channel_raw("channel/id", &json!([])).await,
            Err(ApiError::InvalidRequest(_))
        ));
        assert!(matches!(
            client
                .update_channel_raw("channel/id", &json!({"name":42}))
                .await,
            Err(ApiError::InvalidRequest(_))
        ));
        assert_eq!(token_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn group_management_methods_match_the_august_api_contract() {
        let (client, _) = client().await;
        assert_eq!(
            client
                .group_mute_setting("group/id")
                .await
                .unwrap()
                .global_rule
                .mode,
            "none"
        );
        client
            .set_group_mute(
                "group/id",
                &SetGroupMuteRequest {
                    members: vec![GroupMuteMemberOperation {
                        op: GroupMuteOperation::Add,
                        member_openid: "member/id".to_owned(),
                        mute_expire_at: Some("2099-08-11T10:00:00Z".to_owned()),
                    }],
                },
            )
            .await
            .unwrap();
        let page = PageRequest {
            cursor: None,
            limit: Some(20),
        };
        assert_eq!(
            client
                .group_join_requests("group/id", &page)
                .await
                .unwrap()
                .list[0]
                .join_request_id,
            "join/id"
        );
        client
            .review_group_join_request(
                "group/id",
                "member/id",
                &ReviewGroupJoinRequest {
                    op: GroupJoinApprovalOperation::Approve,
                    join_request_id: "join/id".to_owned(),
                    reject_reason: None,
                    add_to_member_blacklist: None,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn direct_message_methods_use_the_official_dms_paths() {
        let (client, _) = client().await;
        let session = client
            .create_direct_message_session(&CreateDirectMessageRequest {
                recipient_id: "recipient/id".to_owned(),
                source_guild_id: "source/guild".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(session.guild_id, "direct/guild");
        let sent = client
            .send_direct_message(
                "direct/guild",
                &ChannelMessageRequest::text("private pong").with_reply_to("source-message"),
            )
            .await
            .unwrap();
        assert_eq!(sent.id.as_deref(), Some("direct-message"));
        client
            .recall_direct_message("direct/guild", "direct/message", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn group_join_strategy_methods_match_the_august_api_contract() {
        let (client, _) = client().await;
        let page = PageRequest {
            cursor: None,
            limit: Some(20),
        };
        let created = client
            .create_group_join_strategy(&CreateGroupJoinStrategyRequest {
                group_openids: vec!["group/id".to_owned()],
                group_ids: Vec::new(),
                is_enable: Some(GroupJoinStrategySwitch::On),
                expire_at: None,
                remark: Some("trusted users".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(created.strategy_id, "strategy/id");
        assert_eq!(
            client
                .group_join_strategies(&page)
                .await
                .unwrap()
                .strategies[0]
                .strategy_id,
            "strategy/id"
        );
        client
            .update_group_join_strategy(
                "strategy/id",
                &UpdateGroupJoinStrategyRequest {
                    is_enable: None,
                    expire_at: None,
                    group_action: Some(GroupJoinStrategyGroupAction {
                        op: GroupJoinStrategyGroupOperation::Add,
                        group_openids: vec!["group/id".to_owned()],
                        group_ids: Vec::new(),
                    }),
                    remark: None,
                },
            )
            .await
            .unwrap();
        client
            .update_group_join_strategy_whitelist(
                "strategy/id",
                &UpdateGroupJoinStrategyWhitelistRequest {
                    op: GroupJoinStrategyWhitelistOperation::Add,
                    whitelist_users: vec!["123456".to_owned()],
                },
            )
            .await
            .unwrap();
        client
            .execute_group_join_strategy("strategy/id")
            .await
            .unwrap();
        client
            .delete_group_join_strategy("strategy/id")
            .await
            .unwrap();
    }
}
