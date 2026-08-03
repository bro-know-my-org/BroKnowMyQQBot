//! QQ Official HTTP callback transport.

use std::{
    collections::HashMap,
    future::IntoFuture as _,
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::to_bytes,
    error_handling::HandleErrorLayer,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, EventSendError, EventSender,
    ShutdownSignal,
};
use ed25519_dalek::{Signature, Signer as _, SigningKey};
use qqbot_protocol::{GatewayPayload, OpCode, OpenApiClient};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, oneshot};
use tower::{
    BoxError, ServiceBuilder,
    limit::ConcurrencyLimitLayer,
    load_shed::LoadShedLayer,
    timeout::{TimeoutLayer, error::Elapsed},
};
use tracing::{debug, info, warn};

use crate::{
    mapping::map_dispatch,
    websocket::{QqActionExecutor, compact_id},
};

const SIGNATURE_HEADER: &str = "x-signature-ed25519";
const TIMESTAMP_HEADER: &str = "x-signature-timestamp";
const APP_ID_HEADER: &str = "x-bot-appid";
const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEEN_EVENTS: usize = 4096;
const MAX_TIMESTAMP_TOLERANCE: Duration = Duration::from_secs(3600);
const MAX_PATH_BYTES: usize = 2048;
const MAX_EVENT_ID_BYTES: usize = 512;
const MAX_REQUEST_CONCURRENCY: usize = 1024;
const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BUFFERED_BODY_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_MEMORY_MULTIPLIER: usize = 4;

#[derive(Debug, Clone)]
pub struct QqWebhookConfig {
    pub adapter_id: AdapterId,
    pub listen: SocketAddr,
    pub path: String,
    pub app_id: String,
    pub app_secret: SecretString,
    pub timestamp_tolerance: Duration,
    pub max_body_bytes: usize,
    pub max_request_concurrency: usize,
    pub request_timeout: Duration,
    pub log_message_content: bool,
}

impl QqWebhookConfig {
    pub fn new(
        listen: SocketAddr,
        path: impl Into<String>,
        app_id: impl Into<String>,
        app_secret: SecretString,
    ) -> Self {
        Self {
            adapter_id: AdapterId::new("qq-official-webhook"),
            listen,
            path: path.into(),
            app_id: app_id.into(),
            app_secret,
            timestamp_tolerance: Duration::from_secs(300),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_request_concurrency: 64,
            request_timeout: Duration::from_secs(10),
            log_message_content: false,
        }
    }
}

#[derive(Debug)]
pub struct QqWebhookAdapter {
    config: QqWebhookConfig,
    state: Arc<WebhookState>,
    actions: QqActionExecutor,
}

impl QqWebhookAdapter {
    pub fn new(config: QqWebhookConfig, api: OpenApiClient) -> Result<Self, AdapterError> {
        validate_config(&config)?;
        let signing_key = derive_signing_key(config.app_secret.expose_secret().as_bytes())?;
        let state = Arc::new(WebhookState {
            adapter_id: config.adapter_id.clone(),
            app_id: config.app_id.clone(),
            signing_key,
            timestamp_tolerance: config.timestamp_tolerance,
            request_timeout: config.request_timeout,
            max_body_bytes: config.max_body_bytes,
            lifecycle: StdMutex::new(WebhookLifecycle::default()),
            seen_events: Mutex::new(SeenEvents::default()),
        });
        let actions =
            QqActionExecutor::new(config.adapter_id.clone(), api, config.log_message_content);
        Ok(Self {
            config,
            state,
            actions,
        })
    }

    fn router(&self) -> Router {
        let generation = self
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .as_ref()
            .map_or(0, |active| active.generation);
        Router::new()
            .route(&self.config.path, post(handle_callback))
            .layer(
                ServiceBuilder::new()
                    .layer(HandleErrorLayer::new(handle_middleware_error))
                    .layer(LoadShedLayer::new())
                    .layer(ConcurrencyLimitLayer::new(
                        self.config.max_request_concurrency,
                    ))
                    .layer(TimeoutLayer::new(self.config.request_timeout)),
            )
            .with_state(WebhookRequestState {
                shared: self.state.clone(),
                generation,
            })
    }
}

#[async_trait]
impl Adapter for QqWebhookAdapter {
    fn id(&self) -> &AdapterId {
        &self.config.adapter_id
    }

    fn platform(&self) -> &'static str {
        "qq.official"
    }

    async fn run(
        &self,
        events: EventSender,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let (terminal_error, terminal_failure) = oneshot::channel();
        let generation = {
            let mut lifecycle = self
                .state
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lifecycle.active.is_some() {
                return Err(AdapterError::Configuration(
                    "QQ Webhook adapter is already running".to_owned(),
                ));
            }
            lifecycle.next_generation = lifecycle.next_generation.wrapping_add(1);
            let generation = lifecycle.next_generation;
            lifecycle.active = Some(ActiveWebhookRun {
                generation,
                events: events.clone(),
                terminal_error: Some(terminal_error),
            });
            generation
        };
        let _run_guard = WebhookRunGuard {
            state: Arc::clone(&self.state),
            generation,
            events: events.clone(),
        };
        let listener = tokio::net::TcpListener::bind(self.config.listen)
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        events.mark_ready();
        info!(
            adapter_id = %self.config.adapter_id,
            listen = %address,
            path = %self.config.path,
            "QQ Webhook callback server is listening"
        );
        let server = axum::serve(listener, self.router())
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .into_future();
        tokio::pin!(server);
        let result = tokio::select! {
            biased;
            terminal = terminal_failure => terminal.map_or_else(
                |_| Err(AdapterError::Transport("QQ Webhook terminal error channel closed".to_owned())),
                Err,
            ),
            result = &mut server => result.map_err(|error| AdapterError::Transport(error.to_string())),
        };
        result
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        self.actions.execute_action(action).await
    }
}

#[derive(Debug)]
struct WebhookState {
    adapter_id: AdapterId,
    app_id: String,
    signing_key: SigningKey,
    timestamp_tolerance: Duration,
    request_timeout: Duration,
    max_body_bytes: usize,
    lifecycle: StdMutex<WebhookLifecycle>,
    seen_events: Mutex<SeenEvents>,
}

#[derive(Debug, Clone)]
struct WebhookRequestState {
    shared: Arc<WebhookState>,
    generation: u64,
}

#[derive(Debug, Default)]
struct WebhookLifecycle {
    next_generation: u64,
    active: Option<ActiveWebhookRun>,
}

#[derive(Debug)]
struct ActiveWebhookRun {
    generation: u64,
    events: EventSender,
    terminal_error: Option<oneshot::Sender<AdapterError>>,
}

struct WebhookRunGuard {
    state: Arc<WebhookState>,
    generation: u64,
    events: EventSender,
}

impl Drop for WebhookRunGuard {
    fn drop(&mut self) {
        self.events.mark_not_ready();
        let mut lifecycle = self
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle
            .active
            .as_ref()
            .is_some_and(|active| active.generation == self.generation)
        {
            lifecycle.active = None;
        }
    }
}

#[derive(Debug, Default)]
struct SeenEvents {
    records: HashMap<String, SeenRecord>,
}

impl SeenEvents {
    fn lookup(&mut self, event_id: &str, now: u64) -> Option<Reservation> {
        let record = self.records.get(event_id).copied()?;
        if record.expires_at < now {
            self.records.remove(event_id);
            return None;
        }
        Some(match record.state {
            SeenState::Pending => Reservation::Pending,
            SeenState::Accepted => Reservation::Accepted,
        })
    }

    fn reserve(&mut self, event_id: &str, expires_at: u64, now: u64) -> Reservation {
        if let Some(record) = self.records.get(event_id).copied() {
            if record.expires_at >= now {
                return match record.state {
                    SeenState::Pending => Reservation::Pending,
                    SeenState::Accepted => Reservation::Accepted,
                };
            }
            self.records.remove(event_id);
        }
        if self.records.len() >= MAX_SEEN_EVENTS {
            self.records.retain(|_, record| record.expires_at >= now);
            if self.records.len() >= MAX_SEEN_EVENTS {
                return Reservation::Saturated;
            }
        }
        self.records.insert(
            event_id.to_owned(),
            SeenRecord {
                state: SeenState::Pending,
                expires_at,
            },
        );
        Reservation::New
    }

    fn accept(&mut self, event_id: &str) {
        if let Some(record) = self.records.get_mut(event_id) {
            if record.state == SeenState::Pending {
                record.state = SeenState::Accepted;
            }
        }
    }

    fn release(&mut self, event_id: &str) {
        if self.records.get(event_id).map(|record| record.state) == Some(SeenState::Pending) {
            self.records.remove(event_id);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeenRecord {
    state: SeenState,
    expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeenState {
    Pending,
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reservation {
    New,
    Pending,
    Accepted,
    Saturated,
}

async fn handle_callback(
    State(request_state): State<WebhookRequestState>,
    request: Request,
) -> Response {
    let state = request_state.shared;
    let (parts, body) = request.into_parts();
    let Ok(body) = to_bytes(body, state.max_body_bytes).await else {
        return callback_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large");
    };
    let timestamp = match authenticate(&state, &parts.headers, &body) {
        Ok(timestamp) => timestamp,
        Err(error) => {
            warn!(adapter_id = %state.adapter_id, error, "rejected QQ Webhook callback");
            return callback_error(StatusCode::UNAUTHORIZED, error);
        }
    };
    let payload: GatewayPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return callback_error(StatusCode::BAD_REQUEST, "invalid callback JSON"),
    };
    match payload.op {
        OpCode::CALLBACK_VALIDATION => validation_response(&state, &payload),
        OpCode::DISPATCH => {
            dispatch_response(&state, request_state.generation, payload, timestamp).await
        }
        _ => callback_error(StatusCode::BAD_REQUEST, "unsupported callback opcode"),
    }
}

fn authenticate(
    state: &WebhookState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<u64, &'static str> {
    let app_id = header_text(headers, APP_ID_HEADER)?;
    if app_id != state.app_id {
        return Err("callback app ID does not match");
    }
    let timestamp_text = header_text(headers, TIMESTAMP_HEADER)?;
    let timestamp = timestamp_text
        .parse::<u64>()
        .map_err(|_| "invalid callback timestamp")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before Unix epoch")?
        .as_secs();
    if now.abs_diff(timestamp) > state.timestamp_tolerance.as_secs() {
        return Err("callback timestamp is outside the accepted window");
    }
    let signature = decode_signature(header_text(headers, SIGNATURE_HEADER)?)?;
    let mut signed = Vec::with_capacity(timestamp_text.len() + body.len());
    signed.extend_from_slice(timestamp_text.as_bytes());
    signed.extend_from_slice(body);
    state
        .signing_key
        .verifying_key()
        .verify_strict(&signed, &signature)
        .map_err(|_| "invalid callback signature")?;
    Ok(timestamp)
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, &'static str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or("required callback header is missing")?;
    if values.next().is_some() {
        return Err("callback header must occur exactly once");
    }
    value
        .to_str()
        .map_err(|_| "callback header is not valid ASCII")
}

fn validation_response(state: &WebhookState, payload: &GatewayPayload) -> Response {
    #[derive(Deserialize)]
    struct ValidationData {
        plain_token: String,
        event_ts: String,
    }

    let data: ValidationData = match serde_json::from_value::<ValidationData>(payload.d.clone()) {
        Ok(data) if !data.plain_token.is_empty() && !data.event_ts.is_empty() => data,
        _ => return callback_error(StatusCode::BAD_REQUEST, "invalid validation payload"),
    };
    let signature = state
        .signing_key
        .sign(format!("{}{}", data.event_ts, data.plain_token).as_bytes());
    Json(json!({
        "plain_token": data.plain_token,
        "signature": encode_hex(&signature.to_bytes()),
    }))
    .into_response()
}

#[allow(clippy::too_many_lines)] // Keep reservation and callback acknowledgement atomic and linear.
async fn dispatch_response(
    state: &WebhookState,
    request_generation: u64,
    payload: GatewayPayload,
    signed_at: u64,
) -> Response {
    let Some(event_id) = payload
        .id
        .as_deref()
        .filter(|event_id| !event_id.trim().is_empty() && event_id.len() <= MAX_EVENT_ID_BYTES)
    else {
        return callback_error(
            StatusCode::BAD_REQUEST,
            "dispatch event ID is missing or invalid",
        );
    };
    let event_id = event_id.to_owned();
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_secs(),
        Err(_) => {
            return callback_error(StatusCode::INTERNAL_SERVER_ERROR, "system clock is invalid");
        }
    };
    match state.seen_events.lock().await.lookup(&event_id, now) {
        Some(Reservation::Accepted) => {
            debug!(adapter_id = %state.adapter_id, event_id = %compact_id(&event_id), "acknowledging duplicate QQ Webhook event");
            return ack_response();
        }
        Some(Reservation::Pending) => {
            return callback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "matching callback is still being accepted",
            );
        }
        Some(Reservation::New | Reservation::Saturated) | None => {}
    }
    let mapped = match map_dispatch(&state.adapter_id, &payload) {
        Ok(mapped) => mapped,
        Err(error) => {
            warn!(adapter_id = %state.adapter_id, event_id = %compact_id(&event_id), %error, "rejected malformed QQ Webhook dispatch");
            return callback_error(StatusCode::BAD_REQUEST, "invalid dispatch payload");
        }
    };
    let Some(event) = mapped else {
        return ack_response();
    };
    let expires_at = signed_at
        .saturating_add(state.timestamp_tolerance.as_secs())
        .max(now.saturating_add(state.request_timeout.as_secs()));
    let mut seen_events = state.seen_events.lock().await;
    match seen_events.reserve(&event_id, expires_at, now) {
        Reservation::New => {}
        Reservation::Accepted => {
            debug!(adapter_id = %state.adapter_id, event_id = %compact_id(&event_id), "acknowledging duplicate QQ Webhook event");
            return ack_response();
        }
        Reservation::Pending => {
            return callback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "matching callback is still being accepted",
            );
        }
        Reservation::Saturated => {
            return callback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "callback deduplication capacity is exhausted",
            );
        }
    }
    let (delivery, terminal_error) = {
        let mut lifecycle = state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(active) = lifecycle
            .active
            .as_mut()
            .filter(|active| active.generation == request_generation)
        else {
            seen_events.release(&event_id);
            return callback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "runtime is not accepting events",
            );
        };
        let delivery = active.events.try_send(event);
        let terminal_error = matches!(delivery, Err(EventSendError::AdapterMismatch { .. }))
            .then(|| active.terminal_error.take())
            .flatten();
        (delivery, terminal_error)
    };
    match delivery {
        Ok(()) => {}
        Err(EventSendError::AdapterMismatch { expected, actual }) => {
            seen_events.release(&event_id);
            drop(seen_events);
            if let Some(terminal_error) = terminal_error {
                let _ =
                    terminal_error.send(AdapterError::EventAdapterMismatch { expected, actual });
            }
            return callback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook adapter identity is invalid",
            );
        }
        Err(EventSendError::QueueFull | EventSendError::QueueClosed) => {
            seen_events.release(&event_id);
            return callback_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "runtime event queue is unavailable",
            );
        }
    }
    seen_events.accept(&event_id);
    info!(adapter_id = %state.adapter_id, event_id = %compact_id(&event_id), "accepted QQ Webhook event");
    ack_response()
}

fn ack_response() -> Response {
    Json(json!({
        "op": OpCode::HTTP_CALLBACK_ACK.value(),
        "d": 0,
    }))
    .into_response()
}

fn validate_config(config: &QqWebhookConfig) -> Result<(), AdapterError> {
    if config.listen.port() == 0 {
        return Err(AdapterError::Configuration(
            "QQ Webhook listener must use a non-zero port".to_owned(),
        ));
    }
    if !is_literal_http_path(&config.path) {
        return Err(AdapterError::Configuration(
            "QQ Webhook path must be a literal absolute HTTP path".to_owned(),
        ));
    }
    if config.app_id.trim().is_empty() {
        return Err(AdapterError::Configuration(
            "QQ Webhook app ID must not be empty".to_owned(),
        ));
    }
    if config.max_body_bytes == 0 || config.max_body_bytes > MAX_BODY_BYTES {
        return Err(AdapterError::Configuration(format!(
            "QQ Webhook body limit must be between 1 and {MAX_BODY_BYTES}"
        )));
    }
    if config.max_request_concurrency == 0
        || config.max_request_concurrency > MAX_REQUEST_CONCURRENCY
    {
        return Err(AdapterError::Configuration(format!(
            "QQ Webhook request concurrency must be between 1 and {MAX_REQUEST_CONCURRENCY}"
        )));
    }
    if config
        .max_body_bytes
        .checked_mul(config.max_request_concurrency)
        .and_then(|bytes| bytes.checked_mul(REQUEST_MEMORY_MULTIPLIER))
        .is_none_or(|bytes| bytes > MAX_BUFFERED_BODY_BYTES)
    {
        return Err(AdapterError::Configuration(format!(
            "QQ Webhook estimated aggregate request memory must not exceed {MAX_BUFFERED_BODY_BYTES} bytes"
        )));
    }
    if config.request_timeout < Duration::from_secs(1)
        || config.request_timeout > MAX_REQUEST_TIMEOUT
    {
        return Err(AdapterError::Configuration(
            "QQ Webhook request timeout must be between 1 second and 1 minute".to_owned(),
        ));
    }
    if config.timestamp_tolerance < Duration::from_secs(1)
        || config.timestamp_tolerance > MAX_TIMESTAMP_TOLERANCE
    {
        return Err(AdapterError::Configuration(
            "QQ Webhook timestamp tolerance must be between 1 second and 1 hour".to_owned(),
        ));
    }
    Ok(())
}

async fn handle_middleware_error(error: BoxError) -> Response {
    if error.is::<Elapsed>() {
        callback_error(StatusCode::REQUEST_TIMEOUT, "callback request timed out")
    } else {
        callback_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "callback server is at capacity",
        )
    }
}

pub fn is_literal_http_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_PATH_BYTES || bytes[0] != b'/' {
        return false;
    }
    if path
        .split('/')
        .any(|component| matches!(component, "." | ".."))
    {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte != b'/'
            && byte != b'-'
            && byte != b'.'
            && byte != b'_'
            && byte != b'~'
            && !byte.is_ascii_alphanumeric()
        {
            return false;
        }
        index += 1;
    }
    true
}

fn derive_signing_key(secret: &[u8]) -> Result<SigningKey, AdapterError> {
    if secret.is_empty() {
        return Err(AdapterError::Configuration(
            "QQ Webhook app secret must not be empty".to_owned(),
        ));
    }
    let mut seed = [0_u8; 32];
    for (index, byte) in seed.iter_mut().enumerate() {
        *byte = secret[index % secret.len()];
    }
    Ok(SigningKey::from_bytes(&seed))
}

fn decode_signature(encoded: &str) -> Result<Signature, &'static str> {
    if encoded.len() != 128 {
        return Err("callback signature has an invalid length");
    }
    let mut bytes = [0_u8; 64];
    for (index, output) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        *output = u8::from_str_radix(&encoded[offset..offset + 2], 16)
            .map_err(|_| "callback signature is not hexadecimal")?;
    }
    Ok(Signature::from_bytes(&bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn callback_error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use axum::http::{Request, StatusCode};
    use bot_core::{Adapter as _, EventSender, RuntimeObserver};
    use ed25519_dalek::Signer as _;
    use secrecy::SecretString;
    use serde_json::{Value, json};
    use tower::ServiceExt as _;

    use super::{
        APP_ID_HEADER, ActiveWebhookRun, Reservation, SIGNATURE_HEADER, SeenEvents,
        TIMESTAMP_HEADER, encode_hex, is_literal_http_path,
    };
    use crate::webhook::{QqWebhookAdapter, QqWebhookConfig, derive_signing_key};

    fn signed_request(body: &Value) -> Request<axum::body::Body> {
        let body = serde_json::to_vec(&body).unwrap();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let key = derive_signing_key(b"test-secret").unwrap();
        let mut signed = timestamp.as_bytes().to_vec();
        signed.extend_from_slice(&body);
        let signature = encode_hex(&key.sign(&signed).to_bytes());
        Request::post("/callbacks/qq")
            .header(APP_ID_HEADER, "app-id")
            .header(TIMESTAMP_HEADER, timestamp)
            .header(SIGNATURE_HEADER, signature)
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    fn adapter() -> QqWebhookAdapter {
        let tokens =
            qqbot_protocol::TokenManager::new("app-id", SecretString::from("test-secret")).unwrap();
        let api = qqbot_protocol::OpenApiClient::new(
            qqbot_protocol::OpenApiEnvironment::Production,
            tokens,
        )
        .unwrap();
        QqWebhookAdapter::new(
            QqWebhookConfig::new(
                "127.0.0.1:8080".parse().unwrap(),
                "/callbacks/qq",
                "app-id",
                SecretString::from("test-secret"),
            ),
            api,
        )
        .unwrap()
    }

    #[test]
    fn duplicate_is_not_accepted_while_original_is_pending() {
        let mut seen = SeenEvents::default();
        assert_eq!(seen.reserve("event", 200, 100), Reservation::New);
        assert_eq!(seen.reserve("event", 200, 100), Reservation::Pending);
        seen.accept("event");
        assert_eq!(seen.reserve("event", 200, 100), Reservation::Accepted);
        assert_eq!(seen.reserve("event", 300, 201), Reservation::New);
    }

    #[test]
    fn validates_literal_callback_paths() {
        assert!(is_literal_http_path("/callbacks/qq-v1"));
        assert!(!is_literal_http_path("/callbacks/%71q"));
        assert!(!is_literal_http_path("callbacks/qq"));
        assert!(!is_literal_http_path("/callbacks/qq?token=x"));
        assert!(!is_literal_http_path("/callbacks/%zz"));
        assert!(!is_literal_http_path("/callbacks/../admin"));
        assert!(!is_literal_http_path("/callbacks/./qq"));
        assert!(!is_literal_http_path("/callbacks/qq\n"));
    }

    #[tokio::test]
    async fn returns_callback_validation_signature() {
        let response = adapter()
            .router()
            .oneshot(signed_request(&json!({
                "op": 13,
                "d": {"plain_token": "plain", "event_ts": "123"}
            })))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["plain_token"], "plain");
        assert_eq!(response["signature"].as_str().unwrap().len(), 128);
    }

    #[tokio::test]
    async fn dispatch_is_enqueued_once_and_acknowledged() {
        let adapter = adapter();
        let (events, mut received) = tokio::sync::mpsc::channel(4);
        let (terminal_error, _terminal_failure) = tokio::sync::oneshot::channel();
        adapter
            .state
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active = Some(ActiveWebhookRun {
            generation: 1,
            events: EventSender::new(events, adapter.id().clone(), RuntimeObserver::new()).unwrap(),
            terminal_error: Some(terminal_error),
        });
        let payload = json!({
            "id": "event-id",
            "op": 0,
            "d": {
                "id": "message-id",
                "content": "/ping",
                "group_openid": "group-id",
                "author": {"member_openid": "member-id"}
            },
            "s": 1,
            "t": "GROUP_AT_MESSAGE_CREATE"
        });
        for _ in 0..2 {
            let response = adapter
                .router()
                .oneshot(signed_request(&payload))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let ack: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(ack, json!({"op": 12, "d": 0}));
        }
        assert_eq!(received.recv().await.unwrap().id.as_str(), "event-id");
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn rejects_invalid_signature() {
        let mut request = signed_request(&json!({"op": 13, "d": {}}));
        request
            .headers_mut()
            .insert(SIGNATURE_HEADER, "00".repeat(64).parse().unwrap());
        let response = adapter().router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_repeated_authentication_header() {
        let mut request = signed_request(&json!({
            "op": 13,
            "d": {"plain_token": "plain", "event_ts": "123"}
        }));
        request
            .headers_mut()
            .append(APP_ID_HEADER, "app-id".parse().unwrap());
        let response = adapter().router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
