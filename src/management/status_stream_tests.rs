use super::*;
use axum::{
    body::{Body, BodyDataStream},
    http::{Request, header},
};
use futures_util::StreamExt as _;
use plugin_host::{PluginStore, StaticPluginHost};
use tower::ServiceExt as _;
const TOKEN: &str = "status-fixture-token-0123456789abcdef0123456789";
fn manager() -> PluginManager {
    let store = PluginStore::in_memory().unwrap();
    PluginManager::new(
        StaticPluginHost::new(store.clone()),
        store,
        std::collections::BTreeMap::default(),
        std::collections::BTreeSet::default(),
    )
}
fn auth() -> AuthState {
    super::super::plugins::token_digest(TOKEN).unwrap().into()
}
fn fixture(
    manager: PluginManager,
    observer: RuntimeObserver,
    auth: AuthState,
    shutdown: Option<ShutdownSignal>,
) -> (Router, tokio::task::JoinHandle<()>, oneshot::Sender<()>) {
    let (router, worker, stop) = router(manager, observer, auth.clone(), shutdown);
    (
        router
            .merge(super::super::auth::router(auth))
            .layer(middleware::from_fn(super::super::plugins::security_headers)),
        tokio::spawn(worker.run()),
        stop,
    )
}
async fn open(app: &Router) -> Response {
    app.clone()
        .oneshot(
            Request::get("/api/status/stream")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn frame(body: &mut BodyDataStream) -> String {
    String::from_utf8(
        tokio::time::timeout(Duration::from_secs(3), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
fn data(frame: &str) -> Value {
    assert!(frame.contains("event: status"), "{frame}");
    serde_json::from_str(
        frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn disabled_bundled_configuration_changes_push_only_a_safe_revision() {
    let directory = std::env::temp_dir().join(format!("bundled-status-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("plugins.toml");
    std::fs::write(&path, "bundled = []\n").unwrap();
    let manager = manager()
        .with_marketplace(
            directory.join("db"),
            Some(path.clone()),
            "https://example.test/index.json".into(),
        )
        .unwrap();
    let (app, task, stop) = fixture(manager, RuntimeObserver::new(), auth(), None);
    let mut stream = open(&app).await.into_body().into_data_stream();
    let first = data(&frame(&mut stream).await);
    std::fs::write(
        &path,
        "bundled = []\n[bundled_config.admin]\nowners = ['not-in-safe-stream']\n",
    )
    .unwrap();
    let next = data(&frame(&mut stream).await);
    assert_ne!(next["bundled_revision"], first["bundled_revision"]);
    assert_eq!(next["plugins"], first["plugins"]);
    assert!(!next.to_string().contains("not-in-safe-stream"));
    stop.send(()).unwrap();
    task.await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
#[tokio::test]
async fn full_snapshots_are_shared_idle_is_quiet_and_observer_updates_are_real() {
    let observer = RuntimeObserver::new();
    let (app, task, stop) = fixture(manager(), observer.clone(), auth(), None);
    let response = open(&app).await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    let mut first = response.into_body().into_data_stream();
    let baseline = data(&frame(&mut first).await);
    let mut second = open(&app).await.into_body().into_data_stream();
    assert_eq!(data(&frame(&mut second).await), baseline);
    assert_eq!(baseline["settings_restart_required"], Value::Null);
    assert_eq!(baseline["plugins_truncated"], false);
    assert!(
        tokio::time::timeout(Duration::from_millis(600), first.next())
            .await
            .is_err()
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let sender =
        bot_core::EventSender::new(tx, bot_core::AdapterId::new("fixture"), observer).unwrap();
    sender.mark_ready();
    let changed = data(&frame(&mut first).await);
    assert_eq!(
        changed["runtime"]["snapshot"]["adapters"]["fixture"]["online"],
        true
    );
    assert_ne!(changed["revision"], baseline["revision"]);
    assert_eq!(data(&frame(&mut second).await), changed);
    let mut resumed = app
        .oneshot(
            Request::get("/api/status/stream")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("last-event-id", "old-boot:999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .into_body()
        .into_data_stream();
    assert_eq!(data(&frame(&mut resumed).await), changed);
    stop.send(()).unwrap();
    task.await.unwrap();
}
#[tokio::test]
async fn safe_plugin_sample_tracks_cli_writes_apply_faults_and_deletion_without_package_reads() {
    let fixture = crate::plugin_configuration::fixture::Fixture::new(true);
    let manager = fixture.manager().await;
    let (app, task, stop) = self::fixture(manager.clone(), RuntimeObserver::new(), auth(), None);
    let mut body = open(&app).await.into_body().into_data_stream();
    let first = data(&frame(&mut body).await);
    assert_eq!(first["plugins"][0]["runtime_state"], "ready");
    std::fs::remove_file(&fixture.installation.package_path).unwrap();
    let mut installation = fixture.installation.clone();
    installation.enabled = false;
    installation.updated_at_ms = 2;
    installation
        .config
        .insert("private-secret".into(), json!("do-not-return"));
    fixture.store.upsert_installation(&installation).unwrap();
    let saved = data(&frame(&mut body).await);
    assert_eq!(saved["plugins"][0]["enabled"], false);
    assert_eq!(saved["plugins"][0]["pending"], true);
    assert!(!saved.to_string().contains("do-not-return"));
    assert!(saved["plugins"][0].get("config").is_none());
    manager.apply(&installation.instance_id, 2).await.unwrap();
    assert_eq!(
        data(&frame(&mut body).await)["plugins"][0]["runtime_state"],
        "stopped"
    );
    fixture
        .store
        .remove_installation(&installation.instance_id)
        .unwrap();
    let orphan = data(&frame(&mut body).await);
    assert_eq!(orphan["plugins"][0]["restart_required"], true);
    assert_eq!(orphan["plugins"][0]["runtime_state"], "stopped");
    stop.send(()).unwrap();
    task.await.unwrap();
    manager.shutdown().await.unwrap();
}
#[tokio::test]
async fn streams_hold_independent_slots_and_release_on_drop_and_shutdown() {
    let (handle, signal) = bot_core::shutdown_channel();
    let (app, task, stop) = fixture(manager(), RuntimeObserver::new(), auth(), Some(signal));
    assert_eq!(
        app.clone()
            .oneshot(
                Request::get("/api/status/stream")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut bodies = Vec::new();
    for _ in 0..MAX_STREAMS {
        let response = open(&app).await;
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(response.into_body());
    }
    assert_eq!(open(&app).await.status(), StatusCode::TOO_MANY_REQUESTS);
    bodies.pop();
    assert_eq!(open(&app).await.status(), StatusCode::OK);
    let mut body = bodies.pop().unwrap().into_data_stream();
    data(&frame(&mut body).await);
    handle.shutdown();
    assert!(body.next().await.is_none());
    assert!(
        bodies
            .pop()
            .unwrap()
            .into_data_stream()
            .next()
            .await
            .is_none()
    );
    task.await.unwrap();
    drop(stop);
}
#[tokio::test]
async fn logout_revokes_idle_stream_without_waiting_for_an_update() {
    let (app, task, stop) = fixture(manager(), RuntimeObserver::new(), auth(), None);
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
        .unwrap();
    let mut body = app
        .clone()
        .oneshot(
            Request::get("/api/status/stream")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .into_body()
        .into_data_stream();
    data(&frame(&mut body).await);
    app.oneshot(
        Request::post("/api/session/logout")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(frame(&mut body).await.contains("event: auth-required"));
    assert!(body.next().await.is_none());
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn status_truncation_is_explicit_and_does_not_read_plugin_packages() {
    let fixture = crate::plugin_configuration::fixture::Fixture::new(false);
    let manager = fixture.manager().await;
    for index in 0..257 {
        let mut row = fixture.installation.clone();
        row.instance_id = format!("entry-{index:03}");
        fixture.store.upsert_installation(&row).unwrap();
    }
    std::fs::remove_file(&fixture.installation.package_path).unwrap();
    let (plugins, truncated, _) = manager.status().await.unwrap();
    assert!(truncated);
    assert_eq!(plugins.len(), 256);
    let encoded = serde_json::to_value(plugins).unwrap();
    assert!(encoded[0].get("schema").is_none());
    assert!(encoded[0].get("granted_permissions").is_none());
    assert!(encoded[0].get("error").is_none());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn stalled_handshakes_hold_status_slots_but_allow_short_requests_and_reject_cross_site() {
    let (app, task, stop) = fixture(manager(), RuntimeObserver::new(), auth(), None);
    for (name, value) in [
        ("origin", "http://evil.test"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/status/stream")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let mut pending = (0..MAX_STREAMS)
        .map(|_| {
            Box::pin(
                app.clone().oneshot(
                    Request::get("/api/status/stream")
                        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                        .body(Body::from_stream(futures_util::stream::pending::<
                            Result<axum::body::Bytes, Infallible>,
                        >()))
                        .unwrap(),
                ),
            )
        })
        .collect::<Vec<_>>();
    for request in &mut pending {
        assert!(futures_util::poll!(request.as_mut()).is_pending());
    }
    assert_eq!(open(&app).await.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        app.clone()
            .oneshot(Request::get("/api/session").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    drop(pending);
    assert_eq!(open(&app).await.status(), StatusCode::OK);
    stop.send(()).unwrap();
    task.await.unwrap();
}
