//! Authenticated management routes; credentials never enter plugin configuration.

use super::auth::{self, AuthState};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::Duration;

use crate::plugin_management::{ManagementError, PluginManager};

#[derive(Clone)]
pub(super) struct ApiState {
    pub(super) manager: PluginManager,
    pub(super) logs: crate::logging::live::LogBuffer,
    pub(super) observer: bot_core::RuntimeObserver,
}

pub(super) fn token_digest(token: &str) -> Result<[u8; 32], super::ManagementError> {
    if !(32..=256).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(super::ManagementError::InvalidToken);
    }
    Ok(Sha256::digest(token.as_bytes()).into())
}

#[cfg(test)]
pub(super) fn router(manager: PluginManager, token: [u8; 32]) -> Router {
    router_with_logs(
        manager,
        token.into(),
        crate::logging::live::LogBuffer::default(),
    )
}

#[cfg(test)]
pub(super) fn router_with_logs(
    manager: PluginManager,
    auth: AuthState,
    logs: crate::logging::live::LogBuffer,
) -> Router {
    router_with_logs_and_shutdown(manager, auth, logs, None)
}

#[cfg(test)]
pub(super) fn router_with_logs_and_shutdown(
    manager: PluginManager,
    auth: AuthState,
    logs: crate::logging::live::LogBuffer,
    shutdown: Option<bot_core::ShutdownSignal>,
) -> Router {
    router_with_runtime(
        manager,
        auth,
        logs,
        shutdown,
        bot_core::RuntimeObserver::new(),
    )
}

pub(super) fn router_with_runtime(
    manager: PluginManager,
    auth: AuthState,
    logs: crate::logging::live::LogBuffer,
    shutdown: Option<bot_core::ShutdownSignal>,
    observer: bot_core::RuntimeObserver,
) -> Router {
    let streams = super::logs_stream::router(logs.clone(), auth.clone(), shutdown);
    Router::new()
        .route("/api/runtime", get(super::runtime::get))
        .route(
            "/api/marketplace/install",
            post(super::marketplace::install),
        )
        .route("/api/marketplace/update", post(super::marketplace::update))
        .route("/api/marketplace", get(super::marketplace::list))
        .route("/api/marketplace/details", get(super::marketplace::details))
        .route("/api/plugins", get(list))
        .route("/api/plugins/config", post(save_config))
        .route("/api/plugins/enabled", post(save_enabled))
        .route("/api/plugins/apply", post(apply))
        .route("/api/logs", get(super::logs::get))
        .route(
            "/api/settings/commands",
            get(super::settings::get).post(super::settings::save),
        )
        .with_state(ApiState {
            manager,
            logs,
            observer,
        })
        .route_layer(middleware::from_fn(read_body))
        .route_layer(middleware::from_fn_with_state(
            auth.clone(),
            auth::authenticate,
        ))
        .merge(auth::router(auth))
        .merge(streams)
        .layer(
            tower::ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(
                    super::config_check_service_error,
                ))
                .layer(tower::load_shed::LoadShedLayer::new())
                .layer(tower::limit::GlobalConcurrencyLimitLayer::new(4)),
        )
}

const BODY_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub(super) async fn read_body(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    match tokio::time::timeout(
        BODY_READ_TIMEOUT,
        axum::body::to_bytes(body, MAX_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => {
            next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                .await
        }
        Ok(Err(_)) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error":"请求内容过大或无法读取"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            Json(json!({"error":"读取请求内容超时，请重试"})),
        )
            .into_response(),
    }
}

async fn list(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"plugins":state.manager.list().await?})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceRequest {
    instance_id: String,
    expected_updated_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigRequest {
    instance_id: String,
    expected_updated_at_ms: i64,
    config: std::collections::BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledRequest {
    instance_id: String,
    expected_updated_at_ms: i64,
    enabled: bool,
}

async fn save_config(
    State(state): State<ApiState>,
    Json(request): Json<ConfigRequest>,
) -> Result<Json<Value>, ApiError> {
    state
        .manager
        .save_config(
            &request.instance_id,
            request.expected_updated_at_ms,
            request.config,
        )
        .await?;
    Ok(Json(
        json!({"plugin":state.manager.view(&request.instance_id).await?}),
    ))
}

async fn save_enabled(
    State(state): State<ApiState>,
    Json(request): Json<EnabledRequest>,
) -> Result<Json<Value>, ApiError> {
    state
        .manager
        .save_enabled(
            &request.instance_id,
            request.expected_updated_at_ms,
            request.enabled,
        )
        .await?;
    Ok(Json(
        json!({"plugin":state.manager.view(&request.instance_id).await?}),
    ))
}

async fn apply(
    State(state): State<ApiState>,
    Json(request): Json<InstanceRequest>,
) -> Result<Json<Value>, ApiError> {
    state
        .manager
        .apply(&request.instance_id, request.expected_updated_at_ms)
        .await?;
    Ok(Json(
        json!({"plugin":state.manager.view(&request.instance_id).await?}),
    ))
}

pub(super) struct ApiError(ManagementError);

impl From<ManagementError> for ApiError {
    fn from(error: ManagementError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            ManagementError::NotManaged => StatusCode::NOT_FOUND,
            ManagementError::InvalidMarketRequest => StatusCode::BAD_REQUEST,
            ManagementError::Conflict | ManagementError::StaleMarketplace => StatusCode::CONFLICT,
            ManagementError::Busy => StatusCode::TOO_MANY_REQUESTS,
            ManagementError::Store => StatusCode::INTERNAL_SERVER_ERROR,
            ManagementError::Lifecycle | ManagementError::MarketplaceUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        (status, Json(json!({"error":self.0.to_string()}))).into_response()
    }
}

pub(super) fn assets() -> Router {
    Router::new()
        .route(
            "/",
            get(async || axum::response::Html(include_str!("../webui/index.html"))),
        )
        .route(
            "/app.js",
            get(async || {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../webui/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(async || {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../webui/style.css"),
                )
            }),
        )
}

pub(super) async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ] {
        response
            .headers_mut()
            .insert(name, axum::http::HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use plugin_host::{PluginStore, StaticPluginHost};
    use tower::ServiceExt as _;

    const TOKEN: &str = "local-test-token-0123456789abcdef0123456789abcdef";

    fn app() -> Router {
        let store = PluginStore::in_memory().unwrap();
        let manager = PluginManager::new(
            StaticPluginHost::new(store.clone()),
            store,
            std::collections::BTreeMap::new(),
            std::collections::BTreeSet::new(),
        );
        router(manager, token_digest(TOKEN).unwrap())
            .merge(assets())
            .layer(axum::extract::DefaultBodyLimit::max(1024))
            .layer(middleware::from_fn(security_headers))
    }

    #[tokio::test]
    async fn all_plugin_operations_require_bearer_auth_before_body_parsing() {
        for (path, method) in [
            ("/api/plugins", "GET"),
            ("/api/logs", "GET"),
            ("/api/plugins/config", "POST"),
            ("/api/plugins/enabled", "POST"),
            ("/api/plugins/apply", "POST"),
            ("/api/settings/commands", "GET"),
            ("/api/settings/commands", "POST"),
        ] {
            for authorization in [None, Some("Bearer wrong"), Some(TOKEN)] {
                let mut request = Request::builder().uri(path).method(method);
                if let Some(authorization) = authorization {
                    request = request.header(header::AUTHORIZATION, authorization);
                }
                let response = app()
                    .oneshot(request.body(Body::from("invalid json")).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
        let response = app()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/plugins?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_list_works_and_static_assets_have_restrictive_headers() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/api/plugins")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"plugins":[]})
        );
        for path in ["/", "/app.js", "/style.css"] {
            let response = app()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                response.headers()[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("frame-ancestors 'none'")
            );
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }

    #[tokio::test]
    async fn authenticated_mutations_reject_oversized_bodies() {
        let response = app()
            .oneshot(
                Request::post("/api/plugins/config")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b' '; 1025]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn http_configuration_edits_are_saved_before_explicit_runtime_application() {
        use crate::plugin_configuration::{checked_plugin, fixture::Fixture};
        let fixture = Fixture::new(true);
        let mut host = StaticPluginHost::new(fixture.store.clone());
        host.register(
            checked_plugin(&fixture.installation).await.unwrap(),
            &fixture.installation.instance_id,
            std::collections::BTreeMap::new(),
            fixture
                .installation
                .granted_permissions
                .iter()
                .cloned()
                .collect(),
        )
        .await
        .unwrap();
        let packages = std::collections::BTreeMap::from([(
            fixture.installation.instance_id.clone(),
            fixture.installation.package_sha256.clone(),
        )]);
        let manager = PluginManager::new(
            host,
            fixture.store.clone(),
            packages,
            std::collections::BTreeSet::new(),
        );
        let app = router(manager.clone(), token_digest(TOKEN).unwrap());
        let send = |path: &'static str, body: Value| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::post(path)
                            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let id = &fixture.installation.instance_id;
        let (status, saved) = send(
            "/api/plugins/config",
            json!({
                "instance_id":id,"expected_updated_at_ms":1,"config":{"greeting":"hello"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["plugin"]["pending"], true);
        assert_eq!(saved["plugin"]["runtime_state"], "ready");
        let revision = saved["plugin"]["updated_at_ms"].as_i64().unwrap();
        let (status, _) = send(
            "/api/plugins/apply",
            json!({
                "instance_id":id,"expected_updated_at_ms":1
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, applied) = send(
            "/api/plugins/apply",
            json!({
                "instance_id":id,"expected_updated_at_ms":revision
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(applied["plugin"]["pending"], false);
        assert_eq!(applied["plugin"]["config"]["greeting"], "hello");
        manager.shutdown().await.unwrap();
    }

    #[test]
    fn management_tokens_cannot_be_empty_short_or_header_incompatible() {
        for token in [
            "",
            "short",
            "this token has spaces and is not valid",
            "line\nbreak-012345678901234567890123456789",
        ] {
            assert!(token_digest(token).is_err());
        }
        assert!(token_digest(TOKEN).is_ok());
    }

    #[tokio::test]
    async fn stalled_bodies_time_out_and_release_all_request_slots() {
        let app = app();
        let (polled, mut signals) = tokio::sync::mpsc::channel(4);
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let polled = polled.clone();
            let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
                let _ = polled.try_send(());
                std::task::Poll::<Option<Result<axum::body::Bytes, std::io::Error>>>::Pending
            }));
            let request = Request::post("/api/plugins/config")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .unwrap();
            requests.spawn(app.clone().oneshot(request));
        }
        for _ in 0..4 {
            signals.recv().await.unwrap();
        }
        let get = || {
            Request::get("/api/plugins")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(get()).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        while let Some(result) = requests.join_next().await {
            assert_eq!(
                result.unwrap().unwrap().status(),
                StatusCode::REQUEST_TIMEOUT
            );
        }
        assert_eq!(app.oneshot(get()).await.unwrap().status(), StatusCode::OK);
    }
}
