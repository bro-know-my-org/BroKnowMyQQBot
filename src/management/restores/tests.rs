use super::*;
use crate::{config, plugin_management::PluginManager};
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request as HttpRequest},
};
use plugin_host::{LogicalArchive, LogicalSnapshot, PluginStore, StaticPluginHost};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Instant,
};
use tower::ServiceExt as _;
const TOKEN: &str = "restore-inspection-fixture-token-0123456789";
struct Fixture {
    root: PathBuf,
    state: State,
    store: PluginStore,
    app: Router,
    shutdown: bot_core::ShutdownHandle,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bkm-inspect-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let config = root.join("bot.toml");
        std::fs::write(
            &config,
            "# untouched\n[commands]\nprefix='!!'\n[plugins]\ndatabase='keep.db'\n",
        )
        .unwrap();
        let database = root.join("plugins.db");
        let store = PluginStore::open(&database).unwrap();
        let manager = PluginManager::new(
            StaticPluginHost::new(store.clone()),
            store.clone(),
            BTreeMap::new(),
            BTreeSet::new(),
        )
        .with_marketplace(database, None, "https://example.com/index.json".into())
        .unwrap();
        let (shutdown, signal) = bot_core::shutdown_channel();
        let mut state = State::new(manager, Some(signal));
        state.repository = Some(crate::backup::Repository::new(
            &root.join("plugins.db"),
            config,
        ));
        let app = router(
            state.clone(),
            super::super::plugins::token_digest(TOKEN).unwrap().into(),
        );
        Self {
            root,
            state,
            store,
            app,
            shutdown,
        }
    }
    async fn call(&self, path: &str, method: Method, body: Body, token: bool) -> Response {
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:52743")
            .header("content-type", "application/octet-stream");
        if token {
            request = request.header("authorization", format!("Bearer {TOKEN}"));
        }
        self.app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn bytes() -> Vec<u8> {
    LogicalArchive::create(
        BTreeMap::from([
            (
                "bot".into(),
                crate::backup::selected(&config::BotConfig::default()),
            ),
            ("bindings".into(), serde_json::json!([])),
        ]),
        LogicalSnapshot {
            installations: vec![],
            state: vec![],
        },
        BTreeMap::new(),
        1,
    )
    .unwrap()
    .to_bytes()
    .unwrap()
}
#[tokio::test]
async fn inspection_is_authenticated_read_only_boot_bound_expiring_and_discardable() {
    let f = Fixture::new();
    let path = "/api/restores/inspect";
    assert_eq!(
        f.call(path, Method::POST, Body::from("broken"), false)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.call(path, Method::POST, Body::from("broken"), true)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let before = std::fs::read(f.root.join("bot.toml")).unwrap();
    let response = f.call(path, Method::POST, Body::from(bytes()), true).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        f.call(
            "/api/restores/11111111-1111-4111-8111-111111111111",
            Method::GET,
            Body::empty(),
            true
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let id = value["operation_id"].as_str().unwrap();
    let url = format!("/api/restores/{id}");
    assert_eq!(value["boot_id"], crate::restart::boot_id());
    assert_eq!(value["settings_before"]["commands"]["prefix"], "!!");
    assert_eq!(value["settings_after"]["commands"]["prefix"], "/");
    assert_eq!(
        f.call(&url, Method::GET, Body::empty(), false)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.call(&url, Method::GET, Body::empty(), true)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(before, std::fs::read(f.root.join("bot.toml")).unwrap());
    assert_eq!(
        f.store.logical_snapshot().unwrap().installations,
        [] as [plugin_host::PluginInstallation; 0]
    );
    assert!(!f.state.repository.as_ref().unwrap().root.exists());
    f.state.tickets.lock().unwrap().as_mut().unwrap().expires = Instant::now();
    assert_eq!(
        f.call(&url, Method::GET, Body::empty(), true)
            .await
            .status(),
        StatusCode::GONE
    );
    assert!(f.state.tickets.lock().unwrap().is_none());
    let response = f.call(path, Method::POST, Body::from(bytes()), true).await;
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let url = format!("/api/restores/{}", value["operation_id"].as_str().unwrap());
    assert_eq!(
        f.call(&url, Method::DELETE, Body::empty(), true)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        f.call(&url, Method::GET, Body::empty(), true)
            .await
            .status(),
        StatusCode::GONE
    );
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}
#[tokio::test]
async fn independent_binary_limit_and_shutdown_do_not_leak_slots_or_write_targets() {
    static CHUNK: [u8; 65536] = [0; 65536];
    let f = Fixture::new();
    let body = Body::from_stream(futures_util::stream::iter(
        (0..2049).map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from_static(&CHUNK))),
    ));
    assert_eq!(
        f.call("/api/restores/inspect", Method::POST, body, true)
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(f.state.slots.available_permits(), 1);
    let (handle, signal) = bot_core::shutdown_channel();
    let mut state = f.state.clone();
    state.shutdown = Some(signal);
    handle.shutdown();
    let app = router(
        state,
        super::super::plugins::token_digest(TOKEN).unwrap().into(),
    );
    let response = app
        .oneshot(
            HttpRequest::post("/api/restores/inspect")
                .header("host", "localhost:52743")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/octet-stream")
                .body(Body::from_stream(futures_util::stream::pending::<
                    Result<axum::body::Bytes, std::io::Error>,
                >()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(f.state.slots.available_permits(), 1);
    assert!(f.state.tickets.lock().unwrap().is_none());
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn valid_large_archive_uses_dedicated_binary_route_and_never_installs_a_guest() {
    use base64::Engine as _;
    use std::io::Write as _;
    let f = Fixture::new();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
        )
        .unwrap();
    let package = plugin_host::ValidatedPluginPackage::from_bytes(&raw).unwrap();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in package.files() {
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.start_file("assets/padding", options).unwrap();
    writer.write_all(&vec![0; 2 * 1024 * 1024]).unwrap();
    let raw = writer.finish().unwrap().into_inner();
    let package = plugin_host::ValidatedPluginPackage::from_bytes(&raw).unwrap();
    let manifest = package.manifest();
    let item=serde_json::from_value(serde_json::json!({"plugin_id":manifest.id,"metadata":manifest.metadata,"instance_id":"dev.bkm.wasm-ping/imported","version":manifest.version,"package_path":format!("packages/{}.bkm-plugin",package.package_sha256()),"package_sha256":package.package_sha256(),"source":"logical-export","trust_level":"local-wasm","signature_status":"unsigned","requested_permissions":manifest.requested_capabilities(),"granted_permissions":manifest.requested_capabilities(),"config":{},"enabled":true,"installed_at_ms":1,"updated_at_ms":1})).unwrap();
    let mut archive = LogicalArchive::from_bytes(&bytes()).unwrap();
    archive.snapshot.installations.push(item);
    let archive = LogicalArchive::create(
        archive.configuration,
        archive.snapshot,
        BTreeMap::from([(package.package_sha256().into(), raw)]),
        1,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    assert!(archive.len() > 2 * 1024 * 1024);
    let response = f
        .call(
            "/api/restores/inspect",
            Method::POST,
            Body::from(archive),
            true,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let result: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(result["plugins"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["plugins"][0]["retained_grants"]
            .as_array()
            .unwrap()
            .as_slice(),
        [] as [serde_json::Value; 0]
    );
    assert_eq!(
        f.store.installations().unwrap(),
        [] as [plugin_host::PluginInstallation; 0]
    );
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_after_inspection_starts_returns_promptly_without_releasing_the_workers_slot() {
    let f = Fixture::new();
    let gate = f.state.manager.hold_writes_for_test().await;
    let app = f.app.clone();
    let request = HttpRequest::post("/api/restores/inspect")
        .header("host", "localhost:52743")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/octet-stream")
        .body(Body::from(bytes()))
        .unwrap();
    let response = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.state.slots.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.shutdown.shutdown();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap()
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(f.state.slots.available_permits(), 0);
    drop(gate);
    drop(
        tokio::time::timeout(std::time::Duration::from_secs(2), f.state.slots.acquire())
            .await
            .unwrap()
            .unwrap(),
    );
    assert!(f.state.tickets.lock().unwrap().is_none());
    f.state.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_private_ticket_is_released_without_a_followup_request() {
    let mut f = Fixture::new();
    f.state.ttl = std::time::Duration::from_millis(1);
    let permit = std::sync::Arc::new(f.state.slots.clone().acquire_owned().await.unwrap());
    let prepared = f
        .state
        .manager
        .inspect_restore(f.state.repository.clone().unwrap(), bytes(), permit)
        .await
        .unwrap();
    f.state.retain(prepared).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.state.tickets.lock().unwrap().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}

impl Fixture {
    async fn inspect_id(&self) -> String {
        let response = self
            .call(
                "/api/restores/inspect",
                Method::POST,
                Body::from(bytes()),
                true,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        value["operation_id"].as_str().unwrap().into()
    }
    async fn confirm(&self, id: &str, boot: &str, consent: bool, origin: &str) -> Response {
        self.app.clone().oneshot(HttpRequest::post(format!("/api/restores/{id}/commit")).header("host","localhost:52743").header("origin",origin).header("authorization",format!("Bearer {TOKEN}")).header("content-type","application/json").body(Body::from(serde_json::json!({"expected_boot_id":boot,"confirm_sensitive_plugin_data":consent,"confirm_unsigned_disabled":consent}).to_string())).unwrap()).await.unwrap()
    }
}
#[tokio::test]
async fn final_confirmation_only_stages_retained_work_and_selects_one_graceful_restart() {
    let f = Fixture::new();
    let id = f.inspect_id().await;
    let before = std::fs::read(f.root.join("bot.toml")).unwrap();
    assert_eq!(
        f.confirm(
            &id,
            crate::restart::boot_id(),
            false,
            "http://localhost:52743"
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.confirm(&id, "stale", true, "http://localhost:52743")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.confirm(&id, crate::restart::boot_id(), true, "http://evil.example")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.confirm(
            &id,
            crate::restart::boot_id(),
            true,
            "http://localhost:52743"
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        f.confirm(
            &id,
            crate::restart::boot_id(),
            true,
            "http://localhost:52743"
        )
        .await
        .status(),
        StatusCode::GONE
    );
    assert!(f.state.controller.cancelled().await == crate::restart::Action::Restart);
    assert_eq!(before, std::fs::read(f.root.join("bot.toml")).unwrap());
    assert_eq!(
        f.store.logical_snapshot().unwrap().installations,
        [] as [plugin_host::PluginInstallation; 0]
    );
    let response = f
        .call(
            &format!("/api/restores/{id}/outcome"),
            Method::GET,
            Body::empty(),
            true,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let outcome: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(outcome["status"], "pending");
    assert_eq!(outcome["operation_id"], id);
    assert_eq!(
        f.state
            .controller
            .request(crate::restart::boot_id(), crate::restart::Action::Stop),
        Err(crate::restart::Rejection::Conflict)
    );
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}
#[tokio::test]
async fn stale_config_and_first_action_conflict_refuse_to_apply() {
    for stopped in [true, false] {
        let f = Fixture::new();
        let id = f.inspect_id().await;
        if stopped {
            f.state
                .controller
                .request(crate::restart::boot_id(), crate::restart::Action::Stop)
                .unwrap();
        } else {
            std::fs::write(f.root.join("bot.toml"), "[commands]\nprefix='changed'\n").unwrap();
        }
        let before = std::fs::read(f.root.join("bot.toml")).unwrap();
        assert_eq!(
            f.confirm(
                &id,
                crate::restart::boot_id(),
                true,
                "http://localhost:52743"
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(before, std::fs::read(f.root.join("bot.toml")).unwrap());
        if stopped {
            let files =
                crate::restore::files::Files::new(f.state.repository.as_ref().unwrap()).unwrap();
            assert_eq!(
                files
                    .read::<crate::restore::offline::Outcome>("outcome.json", 4096)
                    .unwrap()
                    .status,
                "rejected"
            );
        }
        f.shutdown.shutdown();
        f.state.manager.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn disconnect_after_final_confirmation_does_not_cancel_the_retained_operation() {
    let f = std::sync::Arc::new(Fixture::new());
    let id = f.inspect_id().await;
    let guard = f.state.manager.hold_writes_for_test().await;
    let owner = f.clone();
    let selected = id.clone();
    let request = tokio::spawn(async move {
        owner
            .confirm(
                &selected,
                crate::restart::boot_id(),
                true,
                "http://localhost:52743",
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.state.tickets.lock().unwrap().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    request.abort();
    let _ = request.await;
    drop(guard);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.state.controller.cancelled()
        )
        .await
        .unwrap()
            == crate::restart::Action::Restart
    );
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
    assert_eq!(f.state.slots.available_permits(), 1);
    assert_eq!(
        std::fs::read_to_string(f.root.join("bot.toml"))
            .unwrap()
            .matches("prefix='!!'")
            .count(),
        1
    );
}

#[tokio::test]
async fn busy_manager_preserves_ticket_before_admission() {
    let f = Fixture::new();
    let id = f.inspect_id().await;
    let occupied = f.state.manager.hold_operations_for_test();
    assert_eq!(
        f.confirm(
            &id,
            crate::restart::boot_id(),
            true,
            "http://localhost:52743"
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(f.state.inspection(&id).unwrap().operation_id, id);
    drop(occupied);
    assert_eq!(
        f.confirm(
            &id,
            crate::restart::boot_id(),
            true,
            "http://localhost:52743"
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn private_directory_without_outcome_is_a_gone_operation() {
    let f = Fixture::new();
    crate::restore::files::Files::new(f.state.repository.as_ref().unwrap()).unwrap();
    assert_eq!(
        f.call(
            "/api/restores/11111111-1111-4111-8111-111111111111/outcome",
            Method::GET,
            Body::empty(),
            true
        )
        .await
        .status(),
        StatusCode::GONE
    );
    f.shutdown.shutdown();
    f.state.manager.shutdown().await.unwrap();
}
