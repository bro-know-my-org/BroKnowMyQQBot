//! Minimal QQ `OpenAPI` client required by the WebSocket message loop.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Weak},
    time::Duration,
};

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{net::lookup_host, sync::Mutex};
use url::{Host, Url};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{
    audio::{AudioControlRequest, AudioValidationError},
    auth::{AuthError, TokenManager},
    channel_content::{
        ChannelContentValidationError, CreateGuildAnnouncementRequest, CreateScheduleRequest,
        GuildAnnouncement, ListSchedulesQuery, PinsMessage, Schedule, UpdateScheduleRequest,
    },
    forum::{
        CreateForumThreadRequest, ForumPublishTask, ForumThreadDetail, ForumThreadList,
        ForumValidationError,
    },
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
    guild_control::{
        GuildApiPermissionDemand, GuildApiPermissionDemandRequest, GuildApiPermissionList,
        GuildControlValidationError, GuildMembersMuteRequest, GuildMembersMuteResponse,
        GuildMessageSetting, GuildMuteRequest,
    },
    guild_resource::{
        Channel, CreateChannelRequest, Guild, GuildListQuery, GuildResourceValidationError,
        UpdateChannelRequest,
    },
    interaction::{InteractionResponseRequest, InteractionValidationError},
    menu::{
        BotMenuResponse, BotMenuVersion, GenerateShareLinkRequest, MenuValidationError, ShareLink,
        ShareLinkValidationError, UpdateBotMenuRequest,
    },
    message::{
        ChannelMessageRequest, CreateDirectMessageRequest, DirectMessageSession,
        InlineMediaUploadRequest, MediaUploadRequest, MediaUploadResponse,
        MediaUploadValidationError, MessageRequest, MessageResponse,
    },
    panel::{
        CreatePanelRequest, CreatePanelResponse, PanelDetail, PanelListRequest, PanelPage,
        PanelValidationError, PanelVersion, UpdatePanelRequest, UpdatePanelTargetsRequest,
    },
    profile::{BotProfile, GroupBotState, GroupInfo},
    reaction::{ReactionEmoji, ReactionUsersPage, ReactionUsersRequest, ReactionValidationError},
    stream_upload::{
        C2cStreamMessageRequest, C2cStreamMessageResponse, MediaUploadFinalizeRequest,
        StreamUploadValidationError, UploadPart, UploadPartFinishRequest, UploadPrepareRequest,
        UploadPrepareResponse,
    },
};

const PRODUCTION_BASE_URL: &str = "https://api.bot.qq.com/";
const SANDBOX_BASE_URL: &str = "https://api.bot.qq.com/";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_UPLOAD_CLIENT_CACHE_ENTRIES: usize = 16;
const UPLOAD_CLIENT_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq, Eq)]
struct UploadClientKey {
    host: String,
    port: u16,
}

#[derive(Clone)]
struct UploadClientCacheEntry {
    key: UploadClientKey,
    client: Client,
    expires_at: tokio::time::Instant,
}

type UploadInitializerMap = HashMap<(String, u16), Weak<Mutex<()>>>;

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
    #[error("QQ upload DNS resolution failed")]
    Dns(#[source] std::io::Error),
    #[error("QQ upload DNS resolution timed out")]
    DnsTimeout,
    #[error("QQ upload DNS resolution returned no addresses")]
    DnsNoAddresses,
    #[error("QQ upload request timed out")]
    UploadTimeout,
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
    #[error("invalid QQ media upload request: {0}")]
    InvalidMediaUploadRequest(#[source] MediaUploadValidationError),
    #[error("invalid QQ guild request: {0}")]
    InvalidGuildRequest(#[source] GuildRequestValidationError),
    #[error("invalid QQ guild/channel resource request: {0}")]
    InvalidGuildResourceRequest(#[source] GuildResourceValidationError),
    #[error("invalid QQ guild/channel resource response: {0}")]
    InvalidGuildResourceResponse(#[source] GuildResourceValidationError),
    #[error("invalid QQ guild control request: {0}")]
    InvalidGuildControlRequest(#[source] GuildControlValidationError),
    #[error("invalid QQ channel permission request: {0}")]
    InvalidChannelPermissionRequest(#[source] ChannelPermissionValidationError),
    #[error("invalid QQ reaction request: {0}")]
    InvalidReactionRequest(#[source] ReactionValidationError),
    #[error("invalid QQ interaction request: {0}")]
    InvalidInteractionRequest(#[source] InteractionValidationError),
    #[error("invalid QQ bot menu request: {0}")]
    InvalidMenuRequest(#[source] MenuValidationError),
    #[error("invalid QQ share-link request: {0}")]
    InvalidShareLinkRequest(#[source] ShareLinkValidationError),
    #[error("invalid QQ command-panel request: {0}")]
    InvalidPanelRequest(#[source] PanelValidationError),
    #[error("invalid QQ channel-content request: {0}")]
    InvalidChannelContentRequest(#[source] ChannelContentValidationError),
    #[error("invalid QQ audio request: {0}")]
    InvalidAudioRequest(#[source] AudioValidationError),
    #[error("invalid QQ forum request: {0}")]
    InvalidForumRequest(#[source] ForumValidationError),
    #[error("invalid QQ forum response: {0}")]
    InvalidForumResponse(#[source] ForumValidationError),
    #[error("invalid QQ streaming/upload request: {0}")]
    InvalidStreamUploadRequest(#[source] StreamUploadValidationError),
    #[error("invalid QQ streaming/upload response: {0}")]
    InvalidStreamUploadResponse(#[source] StreamUploadValidationError),
}

/// Authenticated QQ `OpenAPI` client.
#[derive(Clone)]
pub struct OpenApiClient {
    client: Client,
    single_send_client: Client,
    base_url: Url,
    tokens: TokenManager,
    upload_clients: Arc<Mutex<VecDeque<UploadClientCacheEntry>>>,
    upload_initializers: Arc<Mutex<UploadInitializerMap>>,
    #[cfg(test)]
    upload_test_addresses: Option<Vec<SocketAddr>>,
    #[cfg(test)]
    upload_test_allow_non_public: bool,
    #[cfg(test)]
    upload_test_resolution_count: Arc<AtomicUsize>,
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
        let single_send_client = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(ApiError::Request)?;
        Ok(Self {
            client,
            single_send_client,
            base_url,
            tokens,
            upload_clients: Arc::new(Mutex::new(VecDeque::new())),
            upload_initializers: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            upload_test_addresses: None,
            #[cfg(test)]
            upload_test_allow_non_public: false,
            #[cfg(test)]
            upload_test_resolution_count: Arc::new(AtomicUsize::new(0)),
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

    pub async fn panels(&self, request: &PanelListRequest) -> Result<PanelPage, ApiError> {
        request.validate().map_err(ApiError::InvalidPanelRequest)?;
        let mut url = self.endpoint(&["v2", "panels"])?;
        url.query_pairs_mut()
            .append_pair("scope", request.scope.as_str());
        if let Some(cursor) = request.cursor.as_deref() {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(limit) = request.limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        self.get_json(url).await
    }

    pub async fn create_panel(
        &self,
        request: &CreatePanelRequest,
    ) -> Result<CreatePanelResponse, ApiError> {
        request.validate().map_err(ApiError::InvalidPanelRequest)?;
        let url = self.endpoint(&["v2", "panels"])?;
        self.post_json(url, request).await
    }

    pub async fn panel(&self, panel_id: &str) -> Result<PanelDetail, ApiError> {
        validate_path_id("panel_id", panel_id)?;
        let url = self.endpoint(&["v2", "panels", panel_id])?;
        self.get_json(url).await
    }

    pub async fn update_panel(
        &self,
        panel_id: &str,
        request: &UpdatePanelRequest,
    ) -> Result<PanelVersion, ApiError> {
        validate_path_id("panel_id", panel_id)?;
        request.validate().map_err(ApiError::InvalidPanelRequest)?;
        let url = self.endpoint(&["v2", "panels", panel_id])?;
        self.put_json(url, request).await
    }

    pub async fn delete_panel(&self, panel_id: &str) -> Result<(), ApiError> {
        validate_path_id("panel_id", panel_id)?;
        let url = self.endpoint(&["v2", "panels", panel_id])?;
        self.delete(url).await
    }

    pub async fn update_panel_targets(
        &self,
        panel_id: &str,
        request: &UpdatePanelTargetsRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("panel_id", panel_id)?;
        request.validate().map_err(ApiError::InvalidPanelRequest)?;
        let url = self.endpoint(&["v2", "panels", panel_id, "target"])?;
        self.put_json_unit(url, request).await
    }

    pub async fn create_guild_announcement(
        &self,
        guild_id: &str,
        request: &CreateGuildAnnouncementRequest,
    ) -> Result<GuildAnnouncement, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidChannelContentRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "announces"])?;
        self.post_json(url, request).await
    }

    pub async fn delete_guild_announcement(
        &self,
        guild_id: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_non_control_path_id("message_id", message_id)?;
        let url = self.endpoint(&["guilds", guild_id, "announces", message_id])?;
        self.delete(url).await
    }

    pub async fn clear_guild_announcement(&self, guild_id: &str) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        let url = self.endpoint(&["guilds", guild_id, "announces", "all"])?;
        self.delete(url).await
    }

    pub async fn add_channel_pin(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<PinsMessage, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_non_control_path_id("message_id", message_id)?;
        let url = self.endpoint(&["channels", channel_id, "pins", message_id])?;
        self.put_json_response(url).await
    }

    pub async fn delete_channel_pin(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_non_control_path_id("message_id", message_id)?;
        let url = self.endpoint(&["channels", channel_id, "pins", message_id])?;
        self.delete(url).await
    }

    pub async fn clear_channel_pins(&self, channel_id: &str) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id, "pins", "all"])?;
        self.delete(url).await
    }

    pub async fn channel_pins(&self, channel_id: &str) -> Result<PinsMessage, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id, "pins"])?;
        self.get_json(url).await
    }

    pub async fn channel_schedules(
        &self,
        channel_id: &str,
        query: &ListSchedulesQuery,
    ) -> Result<Vec<Schedule>, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let mut url = self.endpoint(&["channels", channel_id, "schedules"])?;
        if let Some(since) = query.since {
            url.query_pairs_mut()
                .append_pair("since", &since.to_string());
        }
        self.get_json(url).await
    }

    pub async fn channel_schedule(
        &self,
        channel_id: &str,
        schedule_id: &str,
    ) -> Result<Schedule, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("schedule_id", schedule_id)?;
        let url = self.endpoint(&["channels", channel_id, "schedules", schedule_id])?;
        self.get_json(url).await
    }

    pub async fn create_channel_schedule(
        &self,
        channel_id: &str,
        request: &CreateScheduleRequest,
    ) -> Result<Schedule, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidChannelContentRequest)?;
        let url = self.endpoint(&["channels", channel_id, "schedules"])?;
        self.post_json(url, request).await
    }

    pub async fn update_channel_schedule(
        &self,
        channel_id: &str,
        schedule_id: &str,
        request: &UpdateScheduleRequest,
    ) -> Result<Schedule, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("schedule_id", schedule_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidChannelContentRequest)?;
        let url = self.endpoint(&["channels", channel_id, "schedules", schedule_id])?;
        self.patch_json(url, request).await
    }

    pub async fn delete_channel_schedule(
        &self,
        channel_id: &str,
        schedule_id: &str,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("schedule_id", schedule_id)?;
        let url = self.endpoint(&["channels", channel_id, "schedules", schedule_id])?;
        self.delete(url).await
    }

    pub async fn control_channel_audio(
        &self,
        channel_id: &str,
        request: &AudioControlRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        request.validate().map_err(ApiError::InvalidAudioRequest)?;
        let url = self.endpoint(&["channels", channel_id, "audio"])?;
        self.post_json_unit(url, request).await
    }

    pub async fn join_channel_mic(&self, channel_id: &str) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id, "mic"])?;
        self.put(url).await
    }

    pub async fn leave_channel_mic(&self, channel_id: &str) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id, "mic"])?;
        self.delete(url).await
    }

    pub async fn forum_threads(&self, channel_id: &str) -> Result<ForumThreadList, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id, "threads"])?;
        let response: ForumThreadList = self.get_json(url).await?;
        response
            .validate()
            .map_err(ApiError::InvalidForumResponse)?;
        Ok(response)
    }

    pub async fn forum_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
    ) -> Result<ForumThreadDetail, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("thread_id", thread_id)?;
        let url = self.endpoint(&["channels", channel_id, "threads", thread_id])?;
        let response: ForumThreadDetail = self.get_json(url).await?;
        response
            .validate()
            .map_err(ApiError::InvalidForumResponse)?;
        Ok(response)
    }

    pub async fn create_forum_thread(
        &self,
        channel_id: &str,
        request: &CreateForumThreadRequest,
    ) -> Result<ForumPublishTask, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        request.validate().map_err(ApiError::InvalidForumRequest)?;
        let url = self.endpoint(&["channels", channel_id, "threads"])?;
        let response: ForumPublishTask = self.put_json_once(url, request).await?;
        response
            .validate()
            .map_err(ApiError::InvalidForumResponse)?;
        Ok(response)
    }

    pub async fn delete_forum_thread(
        &self,
        channel_id: &str,
        thread_id: &str,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("thread_id", thread_id)?;
        let url = self.endpoint(&["channels", channel_id, "threads", thread_id])?;
        self.delete(url).await
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

    pub async fn send_c2c_stream_message(
        &self,
        user_openid: &str,
        request: &C2cStreamMessageRequest,
    ) -> Result<C2cStreamMessageResponse, ApiError> {
        validate_path_id("user_openid", user_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "users", user_openid, "stream_messages"])?;
        let response: C2cStreamMessageResponse = self.post_json_once(url, request).await?;
        response
            .validate()
            .map_err(ApiError::InvalidStreamUploadResponse)?;
        Ok(response)
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
        validate_path_id("user_openid", user_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidMediaUploadRequest)?;
        let url = self.endpoint(&["v2", "users", user_openid, "files"])?;
        let response: MediaUploadResponse = if request.srv_send_msg {
            self.post_json_once(url, request).await?
        } else {
            self.post_json(url, request).await?
        };
        if request.srv_send_msg {
            validate_finalize_response(&response).map_err(ApiError::InvalidStreamUploadResponse)?;
        }
        Ok(response)
    }

    pub async fn upload_group_media(
        &self,
        group_openid: &str,
        request: &MediaUploadRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidMediaUploadRequest)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "files"])?;
        let response: MediaUploadResponse = if request.srv_send_msg {
            self.post_json_once(url, request).await?
        } else {
            self.post_json(url, request).await?
        };
        if request.srv_send_msg {
            validate_finalize_response(&response).map_err(ApiError::InvalidStreamUploadResponse)?;
        }
        Ok(response)
    }

    pub async fn finalize_c2c_upload(
        &self,
        user_openid: &str,
        request: &MediaUploadFinalizeRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        validate_path_id("user_openid", user_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "users", user_openid, "files"])?;
        let response: MediaUploadResponse = self.post_json_once(url, request).await?;
        if request.srv_send_msg {
            validate_finalize_response(&response).map_err(ApiError::InvalidStreamUploadResponse)?;
        }
        Ok(response)
    }

    pub async fn finalize_group_upload(
        &self,
        group_openid: &str,
        request: &MediaUploadFinalizeRequest,
    ) -> Result<MediaUploadResponse, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "files"])?;
        let response: MediaUploadResponse = self.post_json_once(url, request).await?;
        if request.srv_send_msg {
            validate_finalize_response(&response).map_err(ApiError::InvalidStreamUploadResponse)?;
        }
        Ok(response)
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

    pub async fn prepare_c2c_upload(
        &self,
        user_openid: &str,
        request: &UploadPrepareRequest,
    ) -> Result<UploadPrepareResponse, ApiError> {
        validate_path_id("user_openid", user_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "users", user_openid, "upload_prepare"])?;
        let response: UploadPrepareResponse = self.post_json_once(url, request).await?;
        response
            .validate_for_request(request)
            .map_err(ApiError::InvalidStreamUploadResponse)?;
        Ok(response)
    }

    pub async fn prepare_group_upload(
        &self,
        group_openid: &str,
        request: &UploadPrepareRequest,
    ) -> Result<UploadPrepareResponse, ApiError> {
        validate_path_id("group_openid", group_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "upload_prepare"])?;
        let response: UploadPrepareResponse = self.post_json_once(url, request).await?;
        response
            .validate_for_request(request)
            .map_err(ApiError::InvalidStreamUploadResponse)?;
        Ok(response)
    }

    pub async fn upload_prepared_part(
        &self,
        part: &UploadPart,
        bytes: Vec<u8>,
        upload_timeout: Duration,
    ) -> Result<(), ApiError> {
        part.validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let actual_size = u64::try_from(bytes.len())
            .map_err(|_| ApiError::InvalidRequest("upload part is too large".to_owned()))?;
        if actual_size != part.block_size.value() {
            return Err(ApiError::InvalidStreamUploadRequest(
                StreamUploadValidationError::PartSizeMismatch {
                    expected: part.block_size.value(),
                    actual: actual_size,
                },
            ));
        }
        if upload_timeout.is_zero() {
            return Err(ApiError::InvalidStreamUploadRequest(
                StreamUploadValidationError::ZeroUploadTimeout,
            ));
        }
        let url = Url::parse(&part.presigned_url)
            .map_err(|error| ApiError::InvalidUrl(error.to_string()))?;
        let deadline = tokio::time::Instant::now()
            .checked_add(upload_timeout)
            .ok_or(ApiError::InvalidStreamUploadRequest(
                StreamUploadValidationError::InvalidUploadTimeout,
            ))?;
        let destination_port =
            url.port_or_known_default()
                .ok_or(ApiError::InvalidStreamUploadRequest(
                    StreamUploadValidationError::InvalidPresignedDestination,
                ))?;
        let host = normalized_upload_host(&url)?;
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ApiError::UploadTimeout)?;
        let cached_client = tokio::time::timeout(
            remaining,
            self.cached_upload_client(&host, destination_port),
        )
        .await
        .map_err(|_| ApiError::UploadTimeout)?;
        let client = if let Some(client) = cached_client {
            client
        } else {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ApiError::UploadTimeout)?;
            let initializer =
                tokio::time::timeout(remaining, self.upload_initializer(&host, destination_port))
                    .await
                    .map_err(|_| ApiError::UploadTimeout)?;
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ApiError::UploadTimeout)?;
            let _guard = tokio::time::timeout(remaining, initializer.lock())
                .await
                .map_err(|_| ApiError::UploadTimeout)?;
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ApiError::UploadTimeout)?;
            if let Some(client) = tokio::time::timeout(
                remaining,
                self.cached_upload_client(&host, destination_port),
            )
            .await
            .map_err(|_| ApiError::UploadTimeout)?
            {
                client
            } else {
                let (_, addresses) = self
                    .resolve_upload_destination(&url, destination_port, deadline)
                    .await?;
                let remaining = deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(ApiError::UploadTimeout)?;
                tokio::time::timeout(
                    remaining,
                    self.upload_client_for(&host, destination_port, &addresses, deadline),
                )
                .await
                .map_err(|_| ApiError::UploadTimeout)??
            }
        };
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ApiError::UploadTimeout)?;
        tokio::time::timeout(remaining, Self::put_presigned_bytes(&client, url, bytes))
            .await
            .map_err(|_| ApiError::UploadTimeout)?
    }

    async fn upload_client_for(
        &self,
        host: &str,
        port: u16,
        addresses: &[SocketAddr],
        deadline: tokio::time::Instant,
    ) -> Result<Client, ApiError> {
        let key = UploadClientKey {
            host: host.to_owned(),
            port,
        };
        let now = tokio::time::Instant::now();
        let mut cache = self.upload_clients.lock().await;
        cache.retain(|entry| entry.expires_at > now);
        if let Some(client) = cache
            .iter()
            .find(|entry| entry.key.host == host && entry.key.port == port)
            .map(|entry| entry.client.clone())
        {
            ensure_upload_deadline(deadline)?;
            return Ok(client);
        }
        drop(cache);
        let client_builder = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .resolve_to_addrs(host, addresses);
        #[cfg(test)]
        let client_builder = if self.upload_test_addresses.is_some() {
            client_builder.danger_accept_invalid_certs(true)
        } else {
            client_builder
        };
        let client = client_builder.build().map_err(ApiError::Request)?;
        ensure_upload_deadline(deadline)?;
        let mut cache = self.upload_clients.lock().await;
        let now = tokio::time::Instant::now();
        cache.retain(|entry| entry.expires_at > now);
        if let Some(existing) = cache
            .iter()
            .find(|entry| entry.key.host == host && entry.key.port == port)
            .map(|entry| entry.client.clone())
        {
            ensure_upload_deadline(deadline)?;
            return Ok(existing);
        }
        if cache.len() >= MAX_UPLOAD_CLIENT_CACHE_ENTRIES {
            cache.pop_front();
        }
        ensure_upload_deadline(deadline)?;
        cache.push_back(UploadClientCacheEntry {
            key,
            client: client.clone(),
            expires_at: tokio::time::Instant::now() + UPLOAD_CLIENT_CACHE_TTL,
        });
        Ok(client)
    }

    async fn cached_upload_client(&self, host: &str, port: u16) -> Option<Client> {
        let now = tokio::time::Instant::now();
        let mut cache = self.upload_clients.lock().await;
        cache.retain(|entry| entry.expires_at > now);
        cache
            .iter()
            .find(|entry| entry.key.host == host && entry.key.port == port)
            .map(|entry| entry.client.clone())
    }

    async fn upload_initializer(&self, host: &str, port: u16) -> Arc<Mutex<()>> {
        let mut initializers = self.upload_initializers.lock().await;
        initializers.retain(|_, initializer| initializer.strong_count() > 0);
        let key = (host.to_owned(), port);
        if let Some(initializer) = initializers.get(&key).and_then(Weak::upgrade) {
            return initializer;
        }
        let initializer = Arc::new(Mutex::new(()));
        initializers.insert(key, Arc::downgrade(&initializer));
        initializer
    }

    async fn resolve_upload_addresses(
        &self,
        host: &str,
        port: u16,
        deadline: tokio::time::Instant,
    ) -> Result<Vec<SocketAddr>, ApiError> {
        #[cfg(test)]
        if let Some(addresses) = &self.upload_test_addresses {
            self.upload_test_resolution_count
                .fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            return if self.upload_test_allow_non_public {
                Ok(addresses.clone())
            } else {
                validate_public_upload_addresses(addresses.clone())
                    .map_err(ApiError::InvalidStreamUploadRequest)
            };
        }
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ApiError::DnsTimeout)?;
        tokio::time::timeout(remaining, resolve_public_upload_addresses(host, port))
            .await
            .map_err(|_| ApiError::DnsTimeout)?
    }

    async fn resolve_upload_destination(
        &self,
        url: &Url,
        port: u16,
        deadline: tokio::time::Instant,
    ) -> Result<(String, Vec<SocketAddr>), ApiError> {
        match url.host().ok_or(ApiError::InvalidStreamUploadRequest(
            StreamUploadValidationError::InvalidPresignedDestination,
        ))? {
            Host::Domain(host) => {
                let addresses = self.resolve_upload_addresses(host, port, deadline).await?;
                Ok((host.to_owned(), addresses))
            }
            Host::Ipv4(address) => {
                let addresses = validate_public_upload_addresses(vec![SocketAddr::new(
                    IpAddr::V4(address),
                    port,
                )])
                .map_err(ApiError::InvalidStreamUploadRequest)?;
                Ok((address.to_string(), addresses))
            }
            Host::Ipv6(address) => {
                let addresses = validate_public_upload_addresses(vec![SocketAddr::new(
                    IpAddr::V6(address),
                    port,
                )])
                .map_err(ApiError::InvalidStreamUploadRequest)?;
                Ok((address.to_string(), addresses))
            }
        }
    }

    #[cfg(test)]
    fn with_upload_test_addresses(mut self, mut addresses: Vec<SocketAddr>) -> Self {
        addresses.sort_unstable();
        addresses.dedup();
        self.upload_test_addresses = Some(addresses);
        self.upload_test_allow_non_public = true;
        self
    }

    async fn put_presigned_bytes(
        client: &Client,
        url: Url,
        bytes: Vec<u8>,
    ) -> Result<(), ApiError> {
        let response = client
            .put(url)
            .body(bytes)
            .send()
            .await
            .map_err(|error| ApiError::Request(error.without_url()))?;
        Self::decode_presigned_unit(response).await
    }

    pub async fn finish_c2c_upload_part(
        &self,
        user_openid: &str,
        request: &UploadPartFinishRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("user_openid", user_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "users", user_openid, "upload_part_finish"])?;
        self.post_json_unit_once(url, request).await
    }

    pub async fn finish_group_upload_part(
        &self,
        group_openid: &str,
        request: &UploadPartFinishRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("group_openid", group_openid)?;
        request
            .validate()
            .map_err(ApiError::InvalidStreamUploadRequest)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "upload_part_finish"])?;
        self.post_json_unit_once(url, request).await
    }

    pub async fn recall_c2c_message(
        &self,
        user_openid: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        validate_path_id("user_openid", user_openid)?;
        validate_path_id("message_id", message_id)?;
        let url = self.endpoint(&["v2", "users", user_openid, "messages", message_id])?;
        self.delete_once(url).await
    }

    pub async fn recall_group_message(
        &self,
        group_openid: &str,
        message_id: &str,
    ) -> Result<(), ApiError> {
        validate_path_id("group_openid", group_openid)?;
        validate_path_id("message_id", message_id)?;
        let url = self.endpoint(&["v2", "groups", group_openid, "messages", message_id])?;
        self.delete_once(url).await
    }

    pub async fn recall_channel_message(
        &self,
        channel_id: &str,
        message_id: &str,
        hide_tip: bool,
    ) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        validate_path_id("message_id", message_id)?;
        let mut url = self.endpoint(&["channels", channel_id, "messages", message_id])?;
        url.query_pairs_mut()
            .append_pair("hidetip", if hide_tip { "true" } else { "false" });
        self.delete_once(url).await
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
        self.delete_once(url).await
    }

    pub async fn guilds(&self, query: &GuildListQuery) -> Result<Vec<Guild>, ApiError> {
        query
            .validate()
            .map_err(ApiError::InvalidGuildResourceRequest)?;
        let mut url = self.endpoint(&["users", "@me", "guilds"])?;
        {
            let mut pairs = url.query_pairs_mut();
            if let Some(before) = query.before.as_deref() {
                pairs.append_pair("before", before);
            }
            if let Some(after) = query.after.as_deref() {
                pairs.append_pair("after", after);
            }
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }
        let guilds: Vec<Guild> = self.get_json(url).await?;
        for guild in &guilds {
            guild
                .validate()
                .map_err(ApiError::InvalidGuildResourceResponse)?;
        }
        Ok(guilds)
    }

    pub async fn guild(&self, guild_id: &str) -> Result<Guild, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        let url = self.endpoint(&["guilds", guild_id])?;
        let guild: Guild = self.get_json(url).await?;
        guild
            .validate()
            .map_err(ApiError::InvalidGuildResourceResponse)?;
        Ok(guild)
    }

    pub async fn guild_channels(&self, guild_id: &str) -> Result<Vec<Channel>, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        let url = self.endpoint(&["guilds", guild_id, "channels"])?;
        let channels: Vec<Channel> = self.get_json(url).await?;
        for channel in &channels {
            channel
                .validate()
                .map_err(ApiError::InvalidGuildResourceResponse)?;
        }
        Ok(channels)
    }

    pub async fn channel(&self, channel_id: &str) -> Result<Channel, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id])?;
        let channel: Channel = self.get_json(url).await?;
        channel
            .validate()
            .map_err(ApiError::InvalidGuildResourceResponse)?;
        Ok(channel)
    }

    pub async fn create_channel(
        &self,
        guild_id: &str,
        request: &CreateChannelRequest,
    ) -> Result<Channel, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidGuildResourceRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "channels"])?;
        let channel: Channel = self.post_json_once(url, request).await?;
        channel
            .validate()
            .map_err(ApiError::InvalidGuildResourceResponse)?;
        Ok(channel)
    }

    pub async fn update_channel(
        &self,
        channel_id: &str,
        request: &UpdateChannelRequest,
    ) -> Result<Channel, ApiError> {
        validate_path_id("channel_id", channel_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidGuildResourceRequest)?;
        let url = self.endpoint(&["channels", channel_id])?;
        let channel: Channel = self.patch_json_once(url, request).await?;
        channel
            .validate()
            .map_err(ApiError::InvalidGuildResourceResponse)?;
        Ok(channel)
    }

    pub async fn delete_channel(&self, channel_id: &str) -> Result<(), ApiError> {
        validate_path_id("channel_id", channel_id)?;
        let url = self.endpoint(&["channels", channel_id])?;
        self.delete_once(url).await
    }

    pub async fn guild_message_setting(
        &self,
        guild_id: &str,
    ) -> Result<GuildMessageSetting, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        let url = self.endpoint(&["guilds", guild_id, "message", "setting"])?;
        self.get_json(url).await
    }

    pub async fn set_guild_mute(
        &self,
        guild_id: &str,
        request: &GuildMuteRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidGuildControlRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "mute"])?;
        self.patch_json_unit(url, request).await
    }

    pub async fn set_guild_member_mute(
        &self,
        guild_id: &str,
        user_id: &str,
        request: &GuildMuteRequest,
    ) -> Result<(), ApiError> {
        validate_path_id("guild_id", guild_id)?;
        validate_path_id("user_id", user_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidGuildControlRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "members", user_id, "mute"])?;
        self.patch_json_unit(url, request).await
    }

    pub async fn set_guild_members_mute(
        &self,
        guild_id: &str,
        request: &GuildMembersMuteRequest,
    ) -> Result<GuildMembersMuteResponse, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidGuildControlRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "mute"])?;
        self.patch_json(url, request).await
    }

    pub async fn guild_api_permissions(
        &self,
        guild_id: &str,
    ) -> Result<GuildApiPermissionList, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        let url = self.endpoint(&["guilds", guild_id, "api_permission"])?;
        self.get_json(url).await
    }

    pub async fn demand_guild_api_permission(
        &self,
        guild_id: &str,
        request: &GuildApiPermissionDemandRequest,
    ) -> Result<GuildApiPermissionDemand, ApiError> {
        validate_path_id("guild_id", guild_id)?;
        request
            .validate()
            .map_err(ApiError::InvalidGuildControlRequest)?;
        let url = self.endpoint(&["guilds", guild_id, "api_permission", "demand"])?;
        self.post_json_once(url, request).await
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

    pub async fn respond_interaction(
        &self,
        interaction_id: &str,
        request: &InteractionResponseRequest,
    ) -> Result<(), ApiError> {
        request
            .validate(interaction_id)
            .map_err(ApiError::InvalidInteractionRequest)?;
        let url = self.endpoint(&["interactions", interaction_id])?;
        self.put_json_unit_once(url, request).await
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

    async fn post_json_once<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let token = self.tokens.access_token().await?;
        let response = self
            .single_send_client
            .post(url)
            .qqbot_token(token.expose())
            .json(body)
            .send()
            .await
            .map_err(ApiError::Request)?;
        let unauthorized = response.status() == StatusCode::UNAUTHORIZED;
        let result = Self::decode(response).await;
        if unauthorized && self.tokens.refresh_if_current(&token).await.is_err() {
            self.tokens.invalidate_if_current(&token).await;
        }
        result
    }

    async fn post_json_unit_once<B>(&self, url: Url, body: &B) -> Result<(), ApiError>
    where
        B: Serialize + ?Sized,
    {
        let token = self.tokens.access_token().await?;
        let response = self
            .single_send_client
            .post(url)
            .qqbot_token(token.expose())
            .json(body)
            .send()
            .await
            .map_err(ApiError::Request)?;
        let unauthorized = response.status() == StatusCode::UNAUTHORIZED;
        let result = Self::decode_unit(response).await;
        if unauthorized && self.tokens.refresh_if_current(&token).await.is_err() {
            self.tokens.invalidate_if_current(&token).await;
        }
        result
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

    async fn patch_json_once<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let token = self.tokens.access_token().await?;
        let response = self
            .single_send_client
            .patch(url)
            .qqbot_token(token.expose())
            .json(body)
            .send()
            .await
            .map_err(ApiError::Request)?;
        let unauthorized = response.status() == StatusCode::UNAUTHORIZED;
        let result = Self::decode(response).await;
        if unauthorized && self.tokens.refresh_if_current(&token).await.is_err() {
            self.tokens.invalidate_if_current(&token).await;
        }
        result
    }

    async fn patch_json_unit<B>(&self, url: Url, body: &B) -> Result<(), ApiError>
    where
        B: Serialize + ?Sized,
    {
        let response = self
            .send_authorized(|token| self.client.patch(url.clone()).qqbot_token(token).json(body))
            .await?;
        Self::decode_unit(response).await
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

    async fn put_json_once<T, B>(&self, url: Url, body: &B) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let token = self.tokens.access_token().await?;
        let response = self
            .single_send_client
            .put(url)
            .qqbot_token(token.expose())
            .json(body)
            .send()
            .await
            .map_err(ApiError::Request)?;
        let unauthorized = response.status() == StatusCode::UNAUTHORIZED;
        let result = Self::decode(response).await;
        if unauthorized && self.tokens.refresh_if_current(&token).await.is_err() {
            self.tokens.invalidate_if_current(&token).await;
        }
        result
    }

    async fn put_json_unit_once<B>(&self, url: Url, body: &B) -> Result<(), ApiError>
    where
        B: Serialize + ?Sized,
    {
        let token = self.tokens.access_token().await?;
        let response = self
            .single_send_client
            .put(url)
            .qqbot_token(token.expose())
            .json(body)
            .send()
            .await
            .map_err(ApiError::Request)?;
        let unauthorized = response.status() == StatusCode::UNAUTHORIZED;
        let result = Self::decode_unit(response).await;
        if unauthorized && self.tokens.refresh_if_current(&token).await.is_err() {
            self.tokens.invalidate_if_current(&token).await;
        }
        result
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

    async fn put_json_response<T>(&self, url: Url) -> Result<T, ApiError>
    where
        T: DeserializeOwned,
    {
        let response = self
            .send_authorized(|token| self.client.put(url.clone()).qqbot_token(token))
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

    async fn delete_once(&self, url: Url) -> Result<(), ApiError> {
        let token = self.tokens.access_token().await?;
        let response = self
            .single_send_client
            .delete(url)
            .qqbot_token(token.expose())
            .send()
            .await
            .map_err(ApiError::Request)?;
        let unauthorized = response.status() == StatusCode::UNAUTHORIZED;
        let result = Self::decode_unit(response).await;
        if unauthorized && self.tokens.refresh_if_current(&token).await.is_err() {
            self.tokens.invalidate_if_current(&token).await;
        }
        result
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

    async fn decode_presigned_unit(mut response: Response) -> Result<(), ApiError> {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(ApiError::ResponseTooLarge);
        }
        let mut received = 0_usize;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| ApiError::Request(error.without_url()))?
        {
            received = received.saturating_add(chunk.len());
            if received > MAX_RESPONSE_BYTES {
                return Err(ApiError::ResponseTooLarge);
            }
        }
        if status.is_success() {
            Ok(())
        } else {
            Err(ApiError::HttpStatus {
                status,
                code: None,
                message: None,
                trace_id: None,
                retry_after,
            })
        }
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

async fn resolve_public_upload_addresses(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, ApiError> {
    let mut addresses = lookup_host((host, port))
        .await
        .map_err(ApiError::Dns)?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(ApiError::DnsNoAddresses);
    }
    addresses.sort_unstable();
    addresses.dedup();
    validate_public_upload_addresses(addresses).map_err(ApiError::InvalidStreamUploadRequest)
}

fn normalized_upload_host(url: &Url) -> Result<String, ApiError> {
    match url.host().ok_or(ApiError::InvalidStreamUploadRequest(
        StreamUploadValidationError::InvalidPresignedDestination,
    ))? {
        Host::Domain(host) => Ok(host.to_owned()),
        Host::Ipv4(address) => Ok(address.to_string()),
        Host::Ipv6(address) => Ok(address.to_string()),
    }
}

fn ensure_upload_deadline(deadline: tokio::time::Instant) -> Result<(), ApiError> {
    if deadline
        .checked_duration_since(tokio::time::Instant::now())
        .is_none_or(|remaining| remaining.is_zero())
    {
        return Err(ApiError::UploadTimeout);
    }
    Ok(())
}

fn validate_finalize_response(
    response: &MediaUploadResponse,
) -> Result<(), StreamUploadValidationError> {
    if response
        .message_id()
        .is_some_and(|message_id| message_id.trim().is_empty())
    {
        return Err(StreamUploadValidationError::EmptyField { field: "id" });
    }
    Ok(())
}

fn validate_public_upload_addresses(
    addresses: Vec<SocketAddr>,
) -> Result<Vec<SocketAddr>, StreamUploadValidationError> {
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        return Err(StreamUploadValidationError::InvalidPresignedDestination);
    }
    Ok(addresses)
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4() {
        return is_public_ipv4(address);
    }
    let segments = address.segments();
    let is_ietf_special = segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8);
    let is_6to4 = segments[0] == 0x2002;
    let is_6bone = segments[0] == 0x3ffe;
    let is_documentation = segments[0] == 0x3fff && segments[1] & 0xf000 == 0;
    segments[0] & 0xe000 == 0x2000 && !is_ietf_special && !is_6to4 && !is_6bone && !is_documentation
}

fn validate_path_id(field: &str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::InvalidRequest(format!(
            "QQ OpenAPI path field `{field}` must not be empty"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(ApiError::InvalidRequest(format!(
            "QQ OpenAPI path field `{field}` must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_non_control_path_id(field: &str, value: &str) -> Result<(), ApiError> {
    validate_path_id(field, value)?;
    if value == "all" {
        return Err(ApiError::InvalidRequest(format!(
            "QQ OpenAPI path field `{field}` must not use the reserved value `all`"
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

    use super::{OpenApiClient, OpenApiEnvironment};

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

    async fn gateway_bot(headers: HeaderMap) -> (StatusCode, Json<Value>) {
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
        (
            StatusCode::OK,
            Json(json!({
                "url":"ws://gateway.example",
                "shards":2,
                "session_start_limit":{
                    "total":1000,
                    "remaining":999,
                    "reset_after":14_400_000,
                    "max_concurrency":1
                }
            })),
        )
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
        Query(query): Query<std::collections::HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!(channel, "channel/id");
        assert_eq!(message, "message/id");
        assert_eq!(query.get("hidetip").map(String::as_str), Some("true"));
        Json(json!({"ok":true}))
    }

    fn guild_value(id: &str) -> Value {
        json!({
            "id":id,
            "name":"guild",
            "icon":"https://example.com/guild.png",
            "owner_id":"owner/id",
            "owner":true,
            "joined_at":"2026-08-22T10:00:00Z",
            "member_count":10,
            "max_members":100,
            "description":"description"
        })
    }

    fn channel_value(id: &str, name: &str) -> Value {
        json!({
            "id":id,
            "guild_id":"guild/id",
            "name":name,
            "type":0,
            "sub_type":0,
            "position":1,
            "parent_id":"0",
            "owner_id":"owner/id",
            "private_type":0,
            "speak_permission":1
        })
    }

    async fn guilds(Query(query): Query<std::collections::HashMap<String, String>>) -> Json<Value> {
        assert_eq!(query.get("after").map(String::as_str), Some("cursor/id"));
        assert_eq!(query.get("limit").map(String::as_str), Some("25"));
        Json(json!([guild_value("guild/id")]))
    }

    async fn guild(Path(guild): Path<String>) -> Json<Value> {
        assert_eq!(guild, "guild/id");
        Json(guild_value(&guild))
    }

    async fn guild_channels(Path(guild): Path<String>) -> Json<Value> {
        assert_eq!(guild, "guild/id");
        Json(json!([channel_value("channel/id", "general")]))
    }

    async fn create_channel(Path(guild): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(guild, "guild/id");
        assert_eq!(body["name"], "alerts");
        Json(channel_value("channel/id", body["name"].as_str().unwrap()))
    }

    async fn channel(Path(channel): Path<String>) -> Json<Value> {
        assert_eq!(channel, "channel/id");
        Json(channel_value(&channel, "general"))
    }

    async fn update_channel(Path(channel): Path<String>, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(channel, "channel/id");
        assert_eq!(body["name"], "renamed");
        Json(channel_value(&channel, body["name"].as_str().unwrap()))
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
            .route("/gateway/bot", get(gateway_bot))
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
        let gateway_bot = client.gateway_bot().await.unwrap();
        assert_eq!(gateway_bot.url, "ws://gateway.example");
        assert_eq!(gateway_bot.shards, 2);
        assert_eq!(gateway_bot.session_start_limit.remaining, 999);

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
            .recall_channel_message("channel/id", "message/id", true)
            .await
            .unwrap();
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

#[cfg(test)]
mod presigned_upload_tests {
    use std::{
        net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

    use axum::{
        Router,
        body::Bytes,
        extract::{OriginalUri, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::any,
    };
    use reqwest::Client;
    use secrecy::SecretString;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
        task::JoinHandle,
    };
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer},
    };
    use url::Url;

    use crate::DecimalBytes;

    use super::{
        ApiError, MediaUploadResponse, OpenApiClient, StreamUploadValidationError, TokenManager,
        UploadPart, validate_finalize_response, validate_public_upload_addresses,
    };

    #[derive(Default)]
    struct UploadState {
        requests: Mutex<Vec<ObservedUpload>>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ObservedUpload {
        path: String,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    struct ServerTask(Option<JoinHandle<()>>);

    #[derive(Clone, Copy)]
    enum TlsResponseMode {
        Success,
        SlowBody,
        TruncatedBody,
    }

    impl ServerTask {
        async fn abort_and_wait(mut self) {
            let task = self.0.take().expect("server task should be present");
            task.abort();
            match task.await {
                Err(error) if error.is_cancelled() => {}
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Ok(()) => panic!("test server exited unexpectedly"),
                Err(error) => panic!("test server failed: {error}"),
            }
        }
    }

    impl Drop for ServerTask {
        fn drop(&mut self) {
            if let Some(task) = self.0.take() {
                task.abort();
            }
        }
    }

    async fn upload_endpoint(
        State(state): State<Arc<UploadState>>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let path = uri.path().to_owned();
        state.requests.lock().await.push(ObservedUpload {
            path: path.clone(),
            authorization: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            body: body.to_vec(),
        });
        match path.as_str() {
            "/ok" => (StatusCode::OK, "<upload-result/>").into_response(),
            "/json-ok" => (
                StatusCode::OK,
                axum::Json(serde_json::json!({"code":200,"stored":true})),
            )
                .into_response(),
            "/redirect" => (
                StatusCode::TEMPORARY_REDIRECT,
                [("location", "/redirect-target")],
            )
                .into_response(),
            "/server-error" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            "/slow" => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                StatusCode::OK.into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn harness() -> (Url, SocketAddr, Arc<UploadState>, ServerTask) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let origin = Url::parse(&format!("http://{address}/")).unwrap();
        let state = Arc::new(UploadState::default());
        let app = Router::new()
            .fallback(any(upload_endpoint))
            .with_state(Arc::clone(&state));
        let task = ServerTask(Some(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })));
        (origin, address, state, task)
    }

    async fn tls_harness(mode: TlsResponseMode) -> (SocketAddr, Arc<UploadState>, ServerTask) {
        let certified = rcgen::generate_simple_self_signed(vec!["upload.test".to_owned()]).unwrap();
        let private_key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()).into();
        let tls_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], private_key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(UploadState::default());
        let server_state = Arc::clone(&state);
        let task = ServerTask(Some(tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let mut stream = acceptor.accept(stream).await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    let header_end = loop {
                        let read = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "TLS client closed before sending headers");
                        request.extend_from_slice(&buffer[..read]);
                        if let Some(position) =
                            request.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            break position + 4;
                        }
                    };
                    let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap();
                    while request.len() < header_end + content_length {
                        let read = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "TLS client closed before sending the full body");
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let authorization = headers.lines().find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("authorization: ")
                            .map(str::to_owned)
                    });
                    state.requests.lock().await.push(ObservedUpload {
                        path: "/upload".to_owned(),
                        authorization,
                        body: request[header_end..header_end + content_length].to_vec(),
                    });
                    match mode {
                        TlsResponseMode::Success => {
                            stream
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                                .await
                                .unwrap();
                        }
                        TlsResponseMode::SlowBody => {
                            stream
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no")
                                .await
                                .unwrap();
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            let _ = stream.write_all(b"k").await;
                        }
                        TlsResponseMode::TruncatedBody => {
                            stream
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no")
                                .await
                                .unwrap();
                        }
                    }
                });
            }
        })));
        (address, state, task)
    }

    fn public_upload_client(addresses: Vec<SocketAddr>) -> OpenApiClient {
        let tokens = TokenManager::with_client_and_endpoint(
            Client::new(),
            Url::parse("http://127.0.0.1:9/app/getAppAccessToken").unwrap(),
            "app-id",
            SecretString::from("secret".to_owned().into_boxed_str()),
        );
        OpenApiClient::with_base_url(Url::parse("https://api.bot.qq.com/").unwrap(), tokens)
            .unwrap()
            .with_upload_test_addresses(addresses)
    }

    fn upload_part(address: SocketAddr) -> UploadPart {
        UploadPart {
            index: 0,
            presigned_url: format!(
                "https://upload.test:{}/upload?signature=capability-secret",
                address.port()
            ),
            block_size: DecimalBytes::new("block_size", "5").unwrap(),
            extra: serde_json::Map::new(),
        }
    }

    fn single_send_client(timeout: Duration) -> Client {
        Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn presigned_put_sends_exact_bytes_without_qq_authorization() {
        let (origin, _address, state, task) = harness().await;
        OpenApiClient::put_presigned_bytes(
            &single_send_client(Duration::from_secs(1)),
            origin.join("ok").unwrap(),
            b"exact-part-bytes".to_vec(),
        )
        .await
        .unwrap();
        OpenApiClient::put_presigned_bytes(
            &single_send_client(Duration::from_secs(1)),
            origin.join("json-ok").unwrap(),
            b"json-response".to_vec(),
        )
        .await
        .unwrap();

        let requests = state.requests.lock().await;
        assert_eq!(
            requests.as_slice(),
            &[
                ObservedUpload {
                    path: "/ok".to_owned(),
                    authorization: None,
                    body: b"exact-part-bytes".to_vec(),
                },
                ObservedUpload {
                    path: "/json-ok".to_owned(),
                    authorization: None,
                    body: b"json-response".to_vec(),
                },
            ]
        );
        drop(requests);
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn presigned_put_does_not_follow_redirects_or_retry_server_errors() {
        let (origin, _address, state, task) = harness().await;
        let client = single_send_client(Duration::from_secs(1));
        for path in ["redirect", "server-error"] {
            assert!(
                OpenApiClient::put_presigned_bytes(
                    &client,
                    origin.join(path).unwrap(),
                    b"12345".to_vec(),
                )
                .await
                .is_err()
            );
        }

        let requests = state.requests.lock().await;
        for path in ["/redirect", "/server-error"] {
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.path == path)
                    .count(),
                1
            );
        }
        assert!(
            requests
                .iter()
                .all(|request| request.path != "/redirect-target")
        );
        drop(requests);
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn presigned_put_enforces_caller_timeout() {
        let (origin, _address, state, task) = harness().await;
        let error = OpenApiClient::put_presigned_bytes(
            &single_send_client(Duration::from_millis(20)),
            origin.join("slow").unwrap(),
            b"12345".to_vec(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::Request(source) if source.is_timeout()));
        assert_eq!(
            state
                .requests
                .lock()
                .await
                .iter()
                .filter(|request| request.path == "/slow")
                .count(),
            1
        );
        task.abort_and_wait().await;
    }

    #[test]
    fn upload_address_validation_rejects_private_and_mixed_answers() {
        let private = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 443));
        let public = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 443));
        for addresses in [vec![private], vec![public, private]] {
            assert_eq!(
                validate_public_upload_addresses(addresses).unwrap_err(),
                StreamUploadValidationError::InvalidPresignedDestination
            );
        }
        assert_eq!(
            validate_public_upload_addresses(vec![public]).unwrap(),
            vec![public]
        );

        for address in [
            Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0, 1),
            Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 1),
            Ipv6Addr::new(0, 0, 0, 1, 0, 0, 0, 1),
        ] {
            assert_eq!(
                validate_public_upload_addresses(vec![SocketAddr::V6(SocketAddrV6::new(
                    address, 443, 0, 0,
                ))])
                .unwrap_err(),
                StreamUploadValidationError::InvalidPresignedDestination
            );
        }

        let documentation = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0x3fff, 0x0fff, 0, 0, 0, 0, 0, 1),
            443,
            0,
            0,
        ));
        assert_eq!(
            validate_public_upload_addresses(vec![documentation]).unwrap_err(),
            StreamUploadValidationError::InvalidPresignedDestination
        );
        let sixbone = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0x3ffe, 0xffff, 0, 0, 0, 0, 0, 1),
            443,
            0,
            0,
        ));
        assert_eq!(
            validate_public_upload_addresses(vec![sixbone]).unwrap_err(),
            StreamUploadValidationError::InvalidPresignedDestination
        );
        for address in [
            Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 1),
            Ipv6Addr::new(0x3fff, 0x1000, 0, 0, 0, 0, 0, 1),
        ] {
            let socket = SocketAddr::V6(SocketAddrV6::new(address, 443, 0, 0));
            assert_eq!(
                validate_public_upload_addresses(vec![socket]).unwrap(),
                vec![socket]
            );
        }

        let many_public = (1..=17)
            .map(|port| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), port)))
            .collect::<Vec<_>>();
        assert_eq!(
            validate_public_upload_addresses(many_public.clone()).unwrap(),
            many_public
        );
    }

    #[tokio::test]
    async fn resolves_public_ipv6_literal_without_dns() {
        let api = public_upload_client(Vec::new());
        let url = Url::parse("https://[2606:4700::1111]/upload").unwrap();
        let (host, addresses) = api
            .resolve_upload_destination(
                &url,
                443,
                tokio::time::Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert_eq!(host, "2606:4700::1111");
        assert_eq!(addresses, vec!["[2606:4700::1111]:443".parse().unwrap()]);
    }

    #[test]
    fn finalize_response_allows_missing_but_rejects_empty_message_id() {
        let response = MediaUploadResponse {
            file_uuid: "file-uuid".to_owned(),
            file_info: "file-info".to_owned(),
            ttl: None,
            extra: serde_json::Map::new(),
        };
        validate_finalize_response(&response).unwrap();

        let mut empty_id = response;
        empty_id
            .extra
            .insert("id".to_owned(), serde_json::Value::String("   ".to_owned()));
        assert_eq!(
            validate_finalize_response(&empty_id).unwrap_err(),
            StreamUploadValidationError::EmptyField { field: "id" }
        );

        let mut extra = serde_json::Map::new();
        extra.insert(
            "download_secret".to_owned(),
            serde_json::Value::String("extra-download-capability".to_owned()),
        );
        extra.insert(
            "id".to_owned(),
            serde_json::Value::String("message-id".to_owned()),
        );
        extra.insert(
            "raw_url".to_owned(),
            serde_json::Value::String(
                "https://download.example/file?signature=download-capability".to_owned(),
            ),
        );
        let response = MediaUploadResponse {
            file_uuid: "file-capability".to_owned(),
            file_info: "file-info-capability".to_owned(),
            ttl: Some(60),
            extra,
        };
        let debug = format!("{response:?}");
        for secret in [
            "file-capability",
            "file-info-capability",
            "download-capability",
            "extra-download-capability",
        ] {
            assert!(!debug.contains(secret));
        }
    }

    #[tokio::test]
    async fn presigned_client_uses_the_pinned_address_set() {
        let (_origin, address, state, task) = harness().await;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(1))
            .resolve_to_addrs("upload.example", &[address])
            .build()
            .unwrap();
        OpenApiClient::put_presigned_bytes(
            &client,
            Url::parse("http://upload.example/ok").unwrap(),
            b"pinned".to_vec(),
        )
        .await
        .unwrap();
        assert_eq!(state.requests.lock().await[0].body, b"pinned");
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn public_upload_path_performs_pinned_https_put_without_qq_authorization() {
        let (address, state, task) = tls_harness(TlsResponseMode::Success).await;
        let mut api = public_upload_client(vec!["127.0.0.1:1".parse().unwrap(), address]);
        api.upload_prepared_part(
            &upload_part(address),
            b"12345".to_vec(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        api.upload_test_addresses = None;
        api.upload_prepared_part(
            &upload_part(address),
            b"abcde".to_vec(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(api.upload_clients.lock().await.len(), 1);
        assert_eq!(api.upload_test_resolution_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.requests.lock().await.as_slice(),
            &[
                ObservedUpload {
                    path: "/upload".to_owned(),
                    authorization: None,
                    body: b"12345".to_vec(),
                },
                ObservedUpload {
                    path: "/upload".to_owned(),
                    authorization: None,
                    body: b"abcde".to_vec(),
                },
            ]
        );
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn concurrent_uploads_share_singleflight_dns_and_client_initialization() {
        let (address, state, task) = tls_harness(TlsResponseMode::Success).await;
        let api = public_upload_client(vec![address]);
        let first_api = api.clone();
        let second_api = api.clone();
        let first_part = upload_part(address);
        let second_part = upload_part(address);
        let (first, second) = tokio::join!(
            first_api.upload_prepared_part(&first_part, b"12345".to_vec(), Duration::from_secs(1),),
            second_api.upload_prepared_part(
                &second_part,
                b"abcde".to_vec(),
                Duration::from_secs(1),
            )
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(api.upload_test_resolution_count.load(Ordering::SeqCst), 1);
        assert_eq!(api.upload_clients.lock().await.len(), 1);
        assert_eq!(state.requests.lock().await.len(), 2);
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn public_upload_deadline_covers_response_body_consumption() {
        let (address, state, task) = tls_harness(TlsResponseMode::SlowBody).await;
        let error = public_upload_client(vec![address])
            .upload_prepared_part(
                &upload_part(address),
                b"12345".to_vec(),
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::UploadTimeout));
        assert_eq!(state.requests.lock().await.len(), 1);
        assert!(!error.to_string().contains("capability-secret"));
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn public_upload_transport_errors_redact_presigned_credentials() {
        let (address, state, task) = tls_harness(TlsResponseMode::TruncatedBody).await;
        let error = public_upload_client(vec![address])
            .upload_prepared_part(
                &upload_part(address),
                b"12345".to_vec(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Request(_)));
        assert_eq!(state.requests.lock().await.len(), 1);
        assert!(!format!("{error:?}").contains("capability-secret"));
        task.abort_and_wait().await;
    }

    #[tokio::test]
    async fn upload_cache_isolates_ports_and_revalidates_expired_dns_answers() {
        let (first_address, _first_state, first_task) = tls_harness(TlsResponseMode::Success).await;
        let (second_address, _second_state, second_task) =
            tls_harness(TlsResponseMode::Success).await;
        let mut api = public_upload_client(vec![first_address]);
        api.upload_prepared_part(
            &upload_part(first_address),
            b"12345".to_vec(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        api.upload_test_addresses = Some(vec![second_address]);
        api.upload_prepared_part(
            &upload_part(second_address),
            b"abcde".to_vec(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(api.upload_clients.lock().await.len(), 2);

        let mut cache = api.upload_clients.lock().await;
        cache
            .iter_mut()
            .find(|entry| entry.key.port == first_address.port())
            .unwrap()
            .expires_at = tokio::time::Instant::now();
        drop(cache);
        api.upload_test_addresses = Some(vec![first_address]);
        api.upload_test_allow_non_public = false;
        let error = api
            .upload_prepared_part(
                &upload_part(first_address),
                b"12345".to_vec(),
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ApiError::InvalidStreamUploadRequest(
                StreamUploadValidationError::InvalidPresignedDestination
            )
        ));

        first_task.abort_and_wait().await;
        second_task.abort_and_wait().await;
    }
}
