//! Cursor-based access to bounded logs from this process.

use super::plugins::ApiState;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct LogQuery {
    pub(super) after: Option<u64>,
    pub(super) limit: Option<usize>,
    pub(super) level: Option<String>,
    pub(super) instance_id: Option<String>,
}

impl LogQuery {
    pub(super) fn valid(&self) -> bool {
        (1..=200).contains(&self.limit.unwrap_or(200))
            && !self.level.as_deref().is_some_and(|level| {
                !matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")
            })
            && !self.instance_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
            })
    }
}

pub(super) async fn get(State(state): State<ApiState>, Query(query): Query<LogQuery>) -> Response {
    if !query.valid() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"日志筛选参数无效"})),
        )
            .into_response();
    }
    Json(state.logs.snapshot(
        query.after,
        query.limit.unwrap_or(200),
        query.level.as_deref(),
        query.instance_id.as_deref(),
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::super::{auth::AuthState, plugins};
    use crate::{
        config::{ManagementAuth, ManagementConfig},
        logging::live::LogBuffer,
        plugin_management::PluginManager,
    };
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use plugin_host::{PluginStore, StaticPluginHost};
    use serde_json::Value;
    use tower::ServiceExt as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    const TOKEN: &str = "logs-test-token-0123456789abcdef0123456789abcdef";
    fn app(logs: LogBuffer, none: bool) -> Router {
        let store = PluginStore::in_memory().unwrap();
        let manager = PluginManager::new(
            StaticPluginHost::new(store.clone()),
            store,
            std::collections::BTreeMap::default(),
            std::collections::BTreeSet::default(),
        );
        let auth = if none {
            AuthState::from_config(&ManagementConfig {
                auth: ManagementAuth::None,
                ..Default::default()
            })
            .unwrap()
        } else {
            plugins::token_digest(TOKEN).unwrap().into()
        };
        plugins::router_with_logs(manager, auth, logs)
    }
    async fn get(app: &Router, query: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/logs{query}"))
                    .header("host", "127.0.0.1:9090")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    #[tokio::test]
    async fn logs_require_auth_and_reject_invalid_queries() {
        let logs = LogBuffer::default();
        assert_eq!(
            get(&app(logs.clone(), false), "").await.0,
            StatusCode::UNAUTHORIZED
        );
        let app = app(logs, true);
        for query in [
            "?limit=0",
            "?limit=201",
            "?level=INVALID",
            "?after=-1",
            "?unknown=value",
            "?instance_id=",
            "?instance_id=%0A",
        ] {
            assert_eq!(get(&app, query).await.0, StatusCode::BAD_REQUEST, "{query}");
        }
        assert_eq!(get(&app, "?level=TRACE&limit=200").await.0, StatusCode::OK);
        let response = app
            .oneshot(
                Request::get("/api/logs")
                    .header("host", "evil.test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn tracing_logs_reach_http_with_filters_cursors_and_redaction() {
        let logs = LogBuffer::default();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(logs.layer()),
            || {
                tracing::info!(
                    instance_id = "counter-a",
                    token = "private-value",
                    "counter started"
                );
                tracing::warn!(instance_id = "counter-b", "other instance");
                tracing::error!(instance_id = "counter-a", "counter failed");
            },
        );
        let app = app(logs, true);
        let (status, page) = get(&app, "?instance_id=counter-a&limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["entries"][0]["message"], "counter failed");
        assert_eq!(page["next_cursor"], 3);
        let (_, page) = get(&app, "?after=0&instance_id=counter-a&limit=1").await;
        assert_eq!(page["entries"][0]["message"], "counter started");
        assert_eq!(page["entries"][0]["fields"]["token"], "[REDACTED]");
        assert_eq!(page["has_more"], true);
        assert_eq!(page["next_cursor"], 2);
        let (_, next) = get(&app, "?after=2&instance_id=counter-a&limit=1").await;
        assert_eq!(next["entries"][0]["message"], "counter failed");
        assert_eq!(next["has_more"], false);
        assert_eq!(next["session_id"], page["session_id"]);
        let (_, page) = get(&app, "?level=ERROR").await;
        assert_eq!(page["entries"].as_array().unwrap().len(), 1);
        let (_, page) = get(&app, "?after=9999").await;
        assert_eq!(page["dropped"], true);
        assert_eq!(page["entries"].as_array().unwrap().len(), 3);
    }
}
