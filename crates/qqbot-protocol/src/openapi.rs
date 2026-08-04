//! Minimal QQ `OpenAPI` client required by the WebSocket message loop.

use std::{fmt, time::Duration};

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use url::Url;

use crate::{
    auth::{AuthError, TokenManager},
    gateway::{Gateway, GatewayBot},
    message::{
        ChannelMessageRequest, InlineMediaUploadRequest, MediaUploadRequest, MediaUploadResponse,
        MessageRequest, MessageResponse,
    },
};

const PRODUCTION_BASE_URL: &str = "https://api.bot.qq.com/";
const SANDBOX_BASE_URL: &str = "https://sandbox.api.sgroup.qq.com/";
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

    async fn delete(&self, url: Url) -> Result<(), ApiError> {
        let response = self
            .send_authorized(|token| self.client.delete(url.clone()).qqbot_token(token))
            .await?;
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
        extract::{Path, State},
        http::{HeaderMap, StatusCode},
        routing::{delete, get, post},
    };
    use reqwest::Client;
    use secrecy::SecretString;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use url::Url;

    use crate::{MediaFileType, MediaUploadRequest, MessageRequest, auth::TokenManager};

    use super::{ApiError, OpenApiClient};

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
}
