use super::*;
use axum::{body::Body, http::Request};
use futures_util::StreamExt as _;
use plugin_host::{PluginStore, StaticPluginHost};
use serde_json::Value;
use tower::ServiceExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

const TOKEN: &str = "stream-test-token-0123456789abcdef0123456789abcdef";

fn auth() -> AuthState {
    super::super::plugins::token_digest(TOKEN).unwrap().into()
}

fn app(logs: &LogBuffer, auth: AuthState, shutdown: Option<ShutdownSignal>) -> Router {
    let store = PluginStore::in_memory().unwrap();
    let manager = crate::plugin_management::PluginManager::new(
        StaticPluginHost::new(store.clone()),
        store,
        std::collections::BTreeMap::default(),
        std::collections::BTreeSet::default(),
    );
    super::super::plugins::router_with_logs_and_shutdown(manager, auth, logs.clone(), shutdown)
}

fn emit(logs: &LogBuffer, count: usize) {
    tracing::subscriber::with_default(tracing_subscriber::registry().with(logs.layer()), || {
        for sequence in 0..count {
            tracing::info!(instance_id = "one", token = "hidden", sequence, "event");
        }
    });
}

async fn call(app: &Router, path: &str, headers: &[(&str, &str)]) -> Response {
    let mut request = Request::get(path).header(header::HOST, "localhost:9090");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn open(app: &Router, query: &str) -> Response {
    call(
        app,
        &format!("/api/logs/stream{query}"),
        &[("authorization", &format!("Bearer {TOKEN}"))],
    )
    .await
}

async fn frame(body: &mut axum::body::BodyDataStream) -> String {
    let bytes = tokio::time::timeout(Duration::from_secs(2), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn page(frame: &str) -> Value {
    assert!(frame.contains("event: logs\n"), "{frame}");
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let page: Value = serde_json::from_str(data).unwrap();
    assert!(frame.contains(&format!(
        "id: {}:{}\n",
        page["session_id"].as_str().unwrap(),
        page["next_cursor"]
    )));
    page
}

#[tokio::test]
async fn tracing_wakes_stream_and_idle_only_sends_keepalive() {
    let logs = LogBuffer::default();
    emit(&logs, 1);
    let app = app(&logs, auth(), None);
    let response = open(&app, "?instance_id=one").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    let mut body = response.into_body().into_data_stream();
    let initial = page(&frame(&mut body).await);
    assert_eq!(initial["entries"][0]["fields"]["token"], "[REDACTED]");
    assert!(
        tokio::time::timeout(MIN_PAGE_INTERVAL + Duration::from_millis(20), body.next())
            .await
            .is_err()
    );
    // Establish the pending wait before the tracing event, exercising watch wakeup.
    let next = body.next();
    tokio::pin!(next);
    assert!(futures_util::poll!(&mut next).is_pending());
    emit(&logs, 1);
    let update = tokio::time::timeout(Duration::from_secs(1), next)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let update = page(std::str::from_utf8(&update).unwrap());
    assert_eq!(update["entries"][0]["id"], 2);
    let comment = tokio::time::timeout(Duration::from_secs(16), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(comment.starts_with(b":"));
    assert!(!comment.windows(5).any(|window| window == b"data:"));
}

#[tokio::test]
async fn resume_pages_drain_and_last_event_id_takes_precedence() {
    let logs = LogBuffer::default();
    emit(&logs, 450);
    let session = logs.snapshot(None, 1, None, None).session_id;
    let app = app(&logs, auth(), None);
    let mut recent = open(&app, "").await.into_body().into_data_stream();
    let recent = page(&frame(&mut recent).await);
    assert_eq!(recent["entries"].as_array().unwrap().len(), 200);
    assert_eq!(recent["entries"][0]["id"], 251);
    let response = call(
        &app,
        &format!("/api/logs/stream?after=440&session_id={session}"),
        &[
            ("authorization", &format!("Bearer {TOKEN}")),
            ("last-event-id", &format!("{session}:0")),
        ],
    )
    .await;
    let mut body = response.into_body().into_data_stream();
    for (cursor, len, more) in [(200, 200, true), (400, 200, true), (450, 50, false)] {
        let page = page(&frame(&mut body).await);
        assert_eq!(page["next_cursor"], cursor);
        assert_eq!(page["entries"].as_array().unwrap().len(), len);
        assert_eq!(page["has_more"], more);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), body.next())
            .await
            .is_err()
    );
    let mut resumed = open(&app, &format!("?after=450&session_id={session}"))
        .await
        .into_body()
        .into_data_stream();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), resumed.next())
            .await
            .is_err()
    );
    emit(&logs, 1);
    assert_eq!(page(&frame(&mut resumed).await)["entries"][0]["id"], 451);
}

#[tokio::test]
async fn restart_overflow_and_nonmatching_updates_advance_control_pages() {
    let logs = LogBuffer::default();
    emit(&logs, 1005);
    let session = logs.snapshot(None, 1, None, None).session_id;
    let app = app(&logs, auth(), None);
    // Cursor 3 is below the current cursor: session mismatch alone must reset.
    let mut reset = open(&app, "?after=3&session_id=previous-process&limit=2")
        .await
        .into_body()
        .into_data_stream();
    let reset = page(&frame(&mut reset).await);
    assert_eq!(reset["dropped"], true);
    assert_eq!(reset["entries"][0]["id"], 1004);
    let mut overflow = open(&app, &format!("?after=0&session_id={session}&limit=2"))
        .await
        .into_body()
        .into_data_stream();
    let overflow = page(&frame(&mut overflow).await);
    assert_eq!(overflow["dropped"], true);
    assert_eq!(overflow["entries"][0]["id"], 6);
    let mut filtered = open(&app, "?level=ERROR&instance_id=one")
        .await
        .into_body()
        .into_data_stream();
    assert_eq!(page(&frame(&mut filtered).await)["next_cursor"], 1005);
    emit(&logs, 1);
    let control = page(&frame(&mut filtered).await);
    assert_eq!(control["entries"], json!([]));
    assert_eq!(control["next_cursor"], 1006);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), filtered.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn validates_resume_filters_and_auth_source_policy() {
    let logs = LogBuffer::default();
    let app = app(&logs, auth(), None);
    for query in [
        "?after=0",
        "?session_id=old",
        "?after=0&session_id=",
        "?limit=0",
        "?limit=201",
        "?level=BAD",
        "?instance_id=%0A",
        "?unknown=x",
    ] {
        assert_eq!(
            open(&app, query).await.status(),
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    for id in [
        "",
        "bad",
        "old:bad",
        "old:-1",
        "old:1:2",
        "old:18446744073709551616",
    ] {
        assert_eq!(
            call(
                &app,
                "/api/logs/stream",
                &[
                    ("authorization", &format!("Bearer {TOKEN}")),
                    ("last-event-id", id)
                ]
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(&app, "/api/logs/stream", &[]).await.status(),
        StatusCode::UNAUTHORIZED
    );
    for (name, value) in [
        ("origin", "http://evil.test"),
        ("sec-fetch-site", "cross-site"),
        ("host", "bad:port"),
    ] {
        assert_eq!(
            call(
                &app,
                "/api/logs/stream",
                &[("authorization", &format!("Bearer {TOKEN}")), (name, value)]
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let none = AuthState::from_config(&crate::config::ManagementConfig {
        auth: crate::config::ManagementAuth::None,
        ..Default::default()
    })
    .unwrap();
    let app = super::router(logs, none, None);
    assert_eq!(
        call(&app, "/api/logs/stream", &[("host", "evil.test")])
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, "/api/logs/stream", &[]).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn sixteen_body_lifetimes_are_independent_of_short_api_slots() {
    let logs = LogBuffer::default();
    let app = app(&logs, auth(), None);
    let mut bodies = Vec::new();
    for _ in 0..MAX_STREAMS {
        let response = open(&app, "").await;
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(response.into_body()); // Never polled: the slot must already be held.
    }
    assert_eq!(open(&app, "").await.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        call(
            &app,
            "/api/logs",
            &[("authorization", &format!("Bearer {TOKEN}"))]
        )
        .await
        .status(),
        StatusCode::OK
    );
    bodies.pop();
    assert_eq!(open(&app, "").await.status(), StatusCode::OK);
    let mut body = bodies.pop().unwrap().into_data_stream();
    page(&frame(&mut body).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), body.next())
            .await
            .is_err()
    );
    drop(body);
    assert_eq!(open(&app, "").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn stalled_stream_handshakes_share_the_bounded_request_slots() {
    let logs = LogBuffer::default();
    let app = app(&logs, auth(), None);
    let mut pending = (0..4)
        .map(|_| {
            Box::pin(
                app.clone().oneshot(
                    Request::get("/api/logs/stream")
                        .header(header::HOST, "localhost:9090")
                        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                        .body(Body::from_stream(futures_util::stream::pending::<
                            Result<axum::body::Bytes, Infallible>,
                        >()))
                        .unwrap(),
                ),
            )
        })
        .collect::<Vec<_>>();
    for future in &mut pending {
        assert!(futures_util::poll!(future.as_mut()).is_pending());
    }
    assert_eq!(open(&app, "").await.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(pending);
    assert_eq!(open(&app, "").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn logout_terminates_idle_and_active_streams() {
    let logs = LogBuffer::default();
    let app = app(&logs, auth(), None);
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/session/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"token":TOKEN}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let mut idle = call(&app, "/api/logs/stream", &[("cookie", &cookie)])
        .await
        .into_body()
        .into_data_stream();
    let mut active = call(&app, "/api/logs/stream", &[("cookie", &cookie)])
        .await
        .into_body()
        .into_data_stream();
    page(&frame(&mut idle).await);
    page(&frame(&mut active).await);
    app.clone()
        .oneshot(
            Request::post("/api/session/logout")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let event = tokio::time::timeout(Duration::from_secs(16), async {
        loop {
            let event = idle.next().await.unwrap().unwrap();
            if event.starts_with(b":") {
                continue;
            }
            break String::from_utf8(event.to_vec()).unwrap();
        }
    })
    .await
    .unwrap();
    assert!(event.contains("event: auth-required"));
    assert!(idle.next().await.is_none());
    emit(&logs, 1);
    assert!(frame(&mut active).await.contains("event: auth-required"));
    assert!(active.next().await.is_none());
}

#[tokio::test]
async fn shutdown_ends_waiting_and_unpolled_streams() {
    let logs = LogBuffer::default();
    let (handle, signal) = bot_core::shutdown_channel();
    let app = app(&logs, auth(), Some(signal));
    let unpolled = open(&app, "").await;
    let mut body = open(&app, "").await.into_body().into_data_stream();
    page(&frame(&mut body).await);
    let end = body.next();
    tokio::pin!(end);
    assert!(futures_util::poll!(&mut end).is_pending());
    handle.shutdown();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), end)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        unpolled
            .into_body()
            .into_data_stream()
            .next()
            .await
            .is_none()
    );
}
