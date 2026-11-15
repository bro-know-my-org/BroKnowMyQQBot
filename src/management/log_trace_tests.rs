use super::*;
use crate::{
    config::{ManagementAuth, ManagementConfig},
    logging::live::LogBuffer,
    plugin_management::PluginManager,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use plugin_host::{PluginStore, StaticPluginHost};
use serde_json::Value;
use tower::ServiceExt as _;
use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};

fn app(logs: LogBuffer, token: bool) -> Router {
    let store = PluginStore::in_memory().unwrap();
    let manager = PluginManager::new(
        StaticPluginHost::new(store.clone()),
        store,
        std::collections::BTreeMap::default(),
        std::collections::BTreeSet::default(),
    );
    let auth = if token {
        super::super::plugins::token_digest("trace-token-0123456789abcdef0123456789abcdef")
            .unwrap()
            .into()
    } else {
        super::super::auth::AuthState::from_config(&ManagementConfig {
            auth: ManagementAuth::None,
            ..Default::default()
        })
        .unwrap()
    };
    super::super::plugins::router_with_logs(manager, auth, logs)
}
async fn request(app: &Router, query: &str, host: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/api/logs/trace?{query}"))
                .header("host", host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}
fn session(logs: &LogBuffer) -> String {
    logs.snapshot(None, 1, None, None).session_id
}
fn emit(event: &str, invocation: &str, instance: &str, command: &str, kind: &str, status: &str) {
    let span = tracing::info_span!(
        "plugin.action",
        event_id = event,
        invocation_id = invocation,
        instance_id = instance,
        command_id = command,
        plugin_id = "dev.bkm.ping"
    );
    let _entered = span.enter();
    tracing::info!(
        record_kind = kind,
        status,
        error_class = "resource_exhausted",
        raw = "hidden-payload",
        token = "hidden-token",
        "hidden-error"
    );
}
#[tokio::test]
async fn trace_requires_auth_same_origin_and_bounded_exact_ids() {
    let logs = LogBuffer::default();
    let query = format!("session_id={}&event_id=event", session(&logs));
    assert_eq!(
        request(&app(logs.clone(), true), &query, "localhost")
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let app = app(logs, false);
    assert_eq!(
        request(&app, &query, "evil.test").await.0,
        StatusCode::FORBIDDEN
    );
    for bad in [
        "event_id=event",
        "session_id=s",
        "session_id=s&event_id=e&invocation_id=i",
        "session_id=s&event_id=",
        "session_id=s&event_id=%0A",
        "session_id=s&event_id=%5BREDACTED%5D",
        "session_id=s&event_id=e&limit=201",
        "session_id=s&event_id=e&limit=0",
        "session_id=s&event_id=e&include_content=1",
        "session_id=s&event_id=e&unknown=1",
        "session_id=s&event_id=e&event_id=f",
    ] {
        assert_eq!(
            request(&app, bad, "localhost").await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
    let long = format!("session_id=s&event_id={}", "x".repeat(512));
    assert_eq!(
        request(&app, &long, "localhost").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "session_id=old&event_id=e", "localhost")
            .await
            .0,
        StatusCode::GONE
    );
}
#[tokio::test]
async fn trace_keeps_instances_commands_outcomes_and_async_order_without_guessing() {
    let logs = LogBuffer::default();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(logs.layer()), || {
        emit(
            "e",
            "i-a",
            "ping-a",
            "reply",
            "plugin.action.completed",
            "unknown",
        );
        emit(
            "other",
            "i-other",
            "ping-a",
            "reply",
            "plugin.action.completed",
            "succeeded",
        );
        emit(
            "e",
            "i-b",
            "ping-b",
            "reply",
            "plugin.completed",
            "no_output",
        );
        emit("e", "i-a", "ping-a", "reply", "plugin.invoked", "invoked");
        emit(
            "e",
            "i-a",
            "ping-a",
            "reply",
            "plugin.action.completed",
            "succeeded",
        );
        emit(
            "e",
            "recovered",
            "ping-a",
            "scheduled",
            "plugin.failed",
            "failed",
        );
        let span = tracing::info_span!("eventless", invocation_id = "i-a");
        let _entered = span.enter();
        tracing::debug!(
            record_kind = "plugin.completed",
            status = "no_output",
            "completed"
        );
        tracing::info!("unrelated log inside same invocation");
    });
    let query = format!("session_id={}&event_id=e", session(&logs));
    let app = app(logs, false);
    let (status, page) = request(&app, &query, "localhost").await;
    assert_eq!(status, StatusCode::OK);
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 6);
    assert_eq!(entries[0]["fields"]["status"], "unknown");
    assert_eq!(entries[1]["instance_id"], "ping-b");
    assert_eq!(entries[3]["fields"]["status"], "succeeded");
    assert_eq!(entries[4]["fields"]["error_class"], "resource_exhausted");
    assert_eq!(entries[5]["level"], "DEBUG");
    assert_eq!(page["filter_scope"], "all_retained_levels_and_instances");
    assert_eq!(page["partial"], true);
    assert!(!page.to_string().contains("hidden-"));
    let invocation = query.replace("event_id=e", "invocation_id=i-a");
    assert_eq!(
        request(&app, &invocation, "localhost").await.1["entries"],
        page["entries"]
    );
    assert_eq!(
        request(
            &app,
            &query.replace("event_id=e", "event_id=missing"),
            "localhost"
        )
        .await
        .1["entries"]
            .as_array()
            .unwrap()
            .as_slice(),
        [] as [serde_json::Value; 0]
    );
}
#[tokio::test]
async fn trace_paginates_and_reports_eviction_without_collecting_body_or_bypassing_filters() {
    let logs = LogBuffer::default();
    let capture = logs.capture_content();
    tracing::subscriber::with_default(
        tracing_subscriber::registry().with(
            logs.layer()
                .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
        ),
        || {
            tracing::debug!(record_kind = "plugin.invoked", event_id = "e", "filtered");
            for _ in 0..1001 {
                tracing::info!(target: "bkm::message_trace", record_kind = "message.received", event_id = "e", content = "retained-body", "received");
            }
        },
    );
    let query = format!("session_id={}&event_id=e", session(&logs));
    let app = app(logs.clone(), false);
    let (_, page) = request(&app, &query, "localhost").await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 200);
    assert_eq!(page["oldest_id"], 2);
    assert_eq!(page["latest_id"], 1001);
    assert_eq!(page["retention_evicted"], true);
    assert_eq!(page["has_more"], true);
    assert!(!page.to_string().contains("retained-body"));
    let (_, next) = request(
        &app,
        &format!("{query}&after={}&include_content=true", page["next_cursor"]),
        "localhost",
    )
    .await;
    assert_eq!(next["entries"][0]["id"], 202);
    assert_eq!(next["entries"][0]["content"], "retained-body");
    assert_eq!(
        request(&app, &format!("{query}&after=1002"), "localhost")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    drop(capture);
    assert!(
        !request(&app, &format!("{query}&include_content=true"), "localhost")
            .await
            .1
            .to_string()
            .contains("retained-body")
    );
    tracing::subscriber::with_default(tracing_subscriber::registry().with(logs.layer()), || {
        tracing::info!(target: "bkm::message_trace", record_kind = "message.received", event_id = "fresh", content = "not-collected", "received");
    });
    assert!(
        !request(
            &app,
            &query.replace("event_id=e", "event_id=fresh&include_content=true"),
            "localhost"
        )
        .await
        .1
        .to_string()
        .contains("not-collected")
    );
}

#[tokio::test]
async fn multibyte_truncation_never_joins_distinct_ids() {
    let logs = LogBuffer::default();
    let prefix = "x".repeat(509);
    tracing::subscriber::with_default(tracing_subscriber::registry().with(logs.layer()), || {
        emit(
            &format!("{prefix}🦀a"),
            "i-a",
            "a",
            "c",
            "plugin.completed",
            "no_output",
        );
        emit(
            &format!("{prefix}🦀b"),
            "i-b",
            "b",
            "c",
            "plugin.completed",
            "no_output",
        );
    });
    let query = format!("session_id={}&event_id={prefix}", session(&logs));
    let app = app(logs, false);
    assert_eq!(
        request(&app, &query, "localhost").await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, page) = request(
        &app,
        &query.replace(&format!("event_id={prefix}"), "invocation_id=i-a"),
        "localhost",
    )
    .await;
    assert_eq!(page["event_id"], Value::Null);
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
}
