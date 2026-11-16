use super::super::plugins::{router, token_digest};
use crate::{
    plugin_configuration::fixture::Fixture,
    plugin_management::PluginManager,
    plugin_marketplace::{DEFAULT_MARKETPLACE_URL, MarketplaceFixture},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use plugin_host::{StaticPluginHost, ValidatedPluginPackage};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use tower::ServiceExt as _;
const TOKEN: &str = "market-test-token-0123456789abcdef0123456789abcdef";

fn fixture() -> (Fixture, PluginManager, MarketplaceFixture) {
    let fixture = Fixture::new(true);
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let manifest = package.manifest();
    let index = json!({"schema_version":1, "plugins":[{
        "slug":"ping", "plugin_id":manifest.id, "name":"Ping", "summary":"真实包测试夹具", "trust":"community",
        "repository":"https://github.com/example/ping", "commands":["ping"],
        "latest":{"version":manifest.version,"protocol":manifest.protocol,
        "download":format!("https://github.com/example/ping/releases/download/v{}/ping.bkm-plugin",manifest.version),
        "sha256":package.package_sha256()},"capabilities":manifest.requested_capabilities()
    }]});
    let data = Arc::new(Mutex::new((serde_json::to_vec(&index).unwrap(), bytes)));
    let mut manager = PluginManager::new(
        StaticPluginHost::new(fixture.store.clone()),
        fixture.store.clone(),
        BTreeMap::new(),
        BTreeSet::new(),
    )
    .with_marketplace(
        fixture.directory.join("plugins.db"),
        None,
        DEFAULT_MARKETPLACE_URL.to_owned(),
    )
    .unwrap();
    manager.use_marketplace_fixture(data.clone());
    (fixture, manager, data)
}

fn versioned_package(bytes: &[u8], version: &str) -> Vec<u8> {
    use std::io::{Cursor, Write as _};
    use zip::{ZipWriter, write::SimpleFileOptions};
    let package = ValidatedPluginPackage::from_bytes(bytes).unwrap();
    let mut manifest = package.manifest().clone();
    manifest.version = version.to_owned();
    let manifest = toml::to_string(&manifest).unwrap();
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in package.files() {
        archive
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(if name == "plugin.toml" {
                manifest.as_bytes()
            } else {
                content
            })
            .unwrap();
    }
    archive.finish().unwrap().into_inner()
}

fn set_market_version(data: &MarketplaceFixture, version: &str) {
    let mut data = data.lock().unwrap();
    let bytes = versioned_package(&data.1, version);
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let mut index: Value = serde_json::from_slice(&data.0).unwrap();
    index["plugins"][0]["latest"]["version"] = json!(version);
    index["plugins"][0]["latest"]["download"] = json!(format!(
        "https://github.com/example/ping/releases/download/v{version}/ping.bkm-plugin"
    ));
    index["plugins"][0]["latest"]["sha256"] = json!(package.package_sha256());
    *data = (serde_json::to_vec(&index).unwrap(), bytes);
}
fn app(manager: PluginManager) -> Router {
    router(manager, token_digest(TOKEN).unwrap())
}
async fn send(app: Router, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

#[tokio::test]
async fn marketplace_catalogue_search_details_and_bounds() {
    let (_fixture, manager, _) = fixture();
    let app = app(manager);
    let (status, list) = send(app.clone(), "/api/marketplace?q=ping&limit=1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1);
    assert_eq!(list["source"], DEFAULT_MARKETPLACE_URL);
    assert_eq!(list["plugins"][0]["slug"], "ping");
    assert_eq!(list["plugins"][0]["installed"], Value::Null);
    assert_eq!(list["plugins"][0]["selection"].as_str().unwrap().len(), 64);
    let (status, details) = send(app.clone(), "/api/marketplace/details?selector=ping", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(details["plugins"], list["plugins"]);
    let (_, empty) = send(app.clone(), "/api/marketplace?q=no-match", None).await;
    assert_eq!(empty["total"], 0);
    for path in [
        "/api/marketplace?limit=51",
        "/api/marketplace?offset=10001",
        "/api/marketplace/details?selector=../../local",
        "/api/marketplace?url=https://example.com",
    ] {
        assert_eq!(
            send(app.clone(), path, None).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}
#[tokio::test]
async fn marketplace_auth_runs_before_query_parsing_and_safe_index_errors() {
    let (_fixture, manager, data) = fixture();
    let app = app(manager);
    for path in [
        "/api/marketplace?limit=invalid",
        "/api/marketplace/details?selector=ping",
    ] {
        let response = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    data.lock().unwrap().0 = b"malformed secret index".to_vec();
    assert_eq!(
        send(
            app.clone(),
            "/api/marketplace/details?selector=../../local",
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, error) = send(app, "/api/marketplace", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!error.to_string().contains("secret"));
}

async fn intent(app: Router) -> Value {
    let (status, page) = send(app, "/api/marketplace/details?selector=ping", None).await;
    assert_eq!(status, StatusCode::OK);
    let item = &page["plugins"][0];
    json!({"selector":"ping", "selection":item["selection"], "version":item["latest"]["version"],
        "sha256":item["latest"]["sha256"], "expected_updated_at_ms":item["installed"]["updated_at_ms"],
        "grants":[], "confirm_unsigned":true})
}

#[tokio::test]
async fn marketplace_install_is_explicit_disabled_without_grants_and_restart_only() {
    let (fixture, manager, _) = fixture();
    let app = app(manager.clone());
    let body = intent(app.clone()).await;
    let mut rejected = body.clone();
    rejected["confirm_unsigned"] = json!(false);
    assert_eq!(
        send(app.clone(), "/api/marketplace/install", Some(rejected))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, result) = send(app.clone(), "/api/marketplace/install", Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["restart_required"], true);
    assert_eq!(result["loaded_version"], Value::Null);
    assert_eq!(result["runtime_state"], "not_loaded");
    assert_eq!(result["enabled"], false);
    assert_eq!(result["missing_permissions"], json!(["message.reply"]));
    let id = "dev.bkm.wasm-ping/default";
    let record = fixture.store.installation(id).unwrap().unwrap();
    assert_eq!(
        (
            record.trust_level.as_str(),
            record.signature_status.as_str()
        ),
        ("local-wasm", "unsigned")
    );
    assert_eq!(record.granted_permissions, [] as [std::string::String; 0]);
    assert_eq!(
        ValidatedPluginPackage::from_path(std::path::Path::new(&record.package_path))
            .unwrap()
            .package_sha256(),
        record.package_sha256
    );
    assert_eq!(
        send(app.clone(), "/api/marketplace/install", Some(body))
            .await
            .0,
        StatusCode::CONFLICT
    );
    manager.apply(id, record.updated_at_ms).await.unwrap();
    let mut approved = intent(app.clone()).await;
    approved["grants"] = json!(["message.reply"]);
    let (status, _) = send(app, "/api/marketplace/update", Some(approved)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(fixture.store.installation(id).unwrap().unwrap(), record);
    assert_eq!(
        serde_json::to_value(manager.view(id).await.unwrap()).unwrap()["runtime_state"],
        "not_loaded"
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn marketplace_update_preserves_config_grants_and_disabled_state_and_rejects_stale() {
    let (fixture, manager, data) = fixture();
    set_market_version(&data, "1.0.0");
    let mut record = fixture.installation.clone();
    record.instance_id = "dev.bkm.wasm-ping/default".into();
    record.enabled = false;
    record.config.insert("greeting".into(), json!("preserved"));
    fixture.store.upsert_installation(&record).unwrap();
    let app = app(manager.clone());
    let body = intent(app.clone()).await;
    let (status, result) = send(app.clone(), "/api/marketplace/update", Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["enabled"], false);
    let updated = fixture
        .store
        .installation(&record.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(updated.config, record.config);
    assert_eq!(updated.granted_permissions, record.granted_permissions);
    assert!(updated.updated_at_ms > record.updated_at_ms);
    assert_eq!(
        send(app.clone(), "/api/marketplace/update", Some(body))
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut stale = intent(app.clone()).await;
    stale["sha256"] = json!("0".repeat(64));
    assert_eq!(
        send(app.clone(), "/api/marketplace/update", Some(stale))
            .await
            .0,
        StatusCode::CONFLICT
    );
    set_market_version(&data, "2.0.0");
    let mut grants = intent(app.clone()).await;
    grants["grants"] = json!(["unrequested.capability"]);
    assert_eq!(
        send(app, "/api/marketplace/update", Some(grants)).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .store
            .installation(&record.instance_id)
            .unwrap()
            .unwrap()
            .updated_at_ms,
        updated.updated_at_ms
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn marketplace_changed_index_hash_manifest_and_identity_never_overwrite() {
    let (fixture, manager, data) = fixture();
    set_market_version(&data, "1.0.0");
    let mut record = fixture.installation.clone();
    record.instance_id = "dev.bkm.wasm-ping/default".into();
    fixture.store.upsert_installation(&record).unwrap();
    let app = app(manager.clone());
    let original = data.lock().unwrap().clone();
    let body = intent(app.clone()).await;
    let mut index: Value = serde_json::from_slice(&original.0).unwrap();
    index["plugins"][0]["summary"] = json!("changed catalogue");
    data.lock().unwrap().0 = serde_json::to_vec(&index).unwrap();
    assert_eq!(
        send(app.clone(), "/api/marketplace/update", Some(body))
            .await
            .0,
        StatusCode::CONFLICT
    );
    data.lock().unwrap().1 = b"corrupt package".to_vec();
    let body = intent(app.clone()).await;
    assert_eq!(
        send(app.clone(), "/api/marketplace/update", Some(body))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    data.lock().unwrap().1 = original.1;
    index["plugins"][0]["capabilities"] = json!([]);
    data.lock().unwrap().0 = serde_json::to_vec(&index).unwrap();
    let body = intent(app.clone()).await;
    assert_eq!(
        send(app.clone(), "/api/marketplace/update", Some(body))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        fixture
            .store
            .installation(&record.instance_id)
            .unwrap()
            .unwrap()
            .updated_at_ms,
        1
    );
    record.plugin_id = "dev.bkm.other".into();
    fixture.store.upsert_installation(&record).unwrap();
    let body = intent(app.clone()).await;
    assert_eq!(
        send(app, "/api/marketplace/update", Some(body)).await.0,
        StatusCode::NOT_FOUND
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn marketplace_concurrent_updates_use_cas_and_keep_referenced_package() {
    let (fixture, manager, data) = fixture();
    set_market_version(&data, "1.0.0");
    let mut record = fixture.installation.clone();
    record.instance_id = "dev.bkm.wasm-ping/default".into();
    fixture.store.upsert_installation(&record).unwrap();
    let app = app(manager.clone());
    let body = intent(app.clone()).await;
    let (first, second) = tokio::join!(
        send(app.clone(), "/api/marketplace/update", Some(body.clone())),
        send(app, "/api/marketplace/update", Some(body))
    );
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    let saved = fixture
        .store
        .installation(&record.instance_id)
        .unwrap()
        .unwrap();
    assert!(std::path::Path::new(&saved.package_path).is_file());
    assert!(saved.updated_at_ms > record.updated_at_ms);
    manager.shutdown().await.unwrap();
}

#[path = "marketplace_version_tests.rs"]
mod version_tests;

#[tokio::test]
async fn marketplace_mutations_auth_source_and_declarative_boundaries() {
    let (fixture, mut manager, data) = fixture();
    let declarations = fixture.directory.join("declared.toml");
    std::fs::write(&declarations, "[[wasm]]\npackage = 'unused.bkm-plugin'\ninstance_id = 'dev.bkm.wasm-ping/default'\nenabled = false\n").unwrap();
    manager = manager
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            Some(declarations),
            DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    manager.use_marketplace_fixture(data);
    let app = app(manager.clone());
    for path in ["/api/marketplace/install", "/api/marketplace/update"] {
        let response = app
            .clone()
            .oneshot(Request::post(path).body(Body::from("invalid")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::ORIGIN, "https://evil.example")
                    .body(Body::from("invalid"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let body = intent(app.clone()).await;
    assert_eq!(
        send(app.clone(), "/api/marketplace/install", Some(body))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        fixture
            .store
            .installation("dev.bkm.wasm-ping/default")
            .unwrap()
            .is_none()
    );
    let mut body = intent(app.clone()).await;
    body["url"] = json!("https://evil.example/package");
    assert_eq!(
        send(app, "/api/marketplace/install", Some(body)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    manager.shutdown().await.unwrap();
}

fn replace_package_file(data: &MarketplaceFixture, filename: &str, replacement: &[u8]) {
    use std::io::{Cursor, Write as _};
    use zip::{ZipWriter, write::SimpleFileOptions};
    let mut fixture = data.lock().unwrap();
    let package = ValidatedPluginPackage::from_bytes(&fixture.1).unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in package.files() {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(if name == filename { replacement } else { bytes })
            .unwrap();
    }
    fixture.1 = zip.finish().unwrap().into_inner();
    let mut index: Value = serde_json::from_slice(&fixture.0).unwrap();
    index["plugins"][0]["latest"]["sha256"] = json!(hex::encode(Sha256::digest(&fixture.1)));
    fixture.0 = serde_json::to_vec(&index).unwrap();
}

#[tokio::test]
async fn marketplace_incomplete_config_disabled_and_bad_component_has_no_record() {
    let (fixture, manager, data) = fixture();
    replace_package_file(
        &data,
        "config.schema.json",
        br#"{"type":"object","required":["greeting"],"properties":{"greeting":{"type":"string"}}}"#,
    );
    let app = app(manager.clone());
    let mut body = intent(app.clone()).await;
    body["grants"] = json!(["message.reply"]);
    let (status, result) = send(app.clone(), "/api/marketplace/install", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["enabled"], false);
    assert_eq!(result["config_incomplete"], true);
    let id = "dev.bkm.wasm-ping/default";
    let saved = fixture.store.installation(id).unwrap().unwrap();
    assert_eq!(saved.granted_permissions, vec!["message.reply"]);
    set_market_version(&data, "1.0.0");
    replace_package_file(&data, "component.wasm", b"\0asm\x01\0\0\0");
    let body = intent(app.clone()).await;
    assert_eq!(
        send(app, "/api/marketplace/update", Some(body)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        fixture
            .store
            .installation(id)
            .unwrap()
            .unwrap()
            .updated_at_ms,
        saved.updated_at_ms
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn marketplace_storage_failure_is_safe_and_request_cancellation_is_supervised() {
    let (fixture, mut manager, data) = fixture();
    manager = manager
        .with_marketplace(
            std::path::Path::new(&fixture.installation.package_path).join("not-a-directory.db"),
            None,
            DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    manager.use_marketplace_fixture(data.clone());
    let app = app(manager.clone());
    let body = intent(app.clone()).await;
    let (status, error) = send(app, "/api/marketplace/install", Some(body)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !error
            .to_string()
            .contains(&fixture.directory.display().to_string())
    );
    assert!(
        fixture
            .store
            .installation("dev.bkm.wasm-ping/default")
            .unwrap()
            .is_none()
    );
    manager.shutdown().await.unwrap();

    let (fixture, manager, _) = self::fixture();
    let app = self::app(manager.clone());
    let body = intent(app.clone()).await;
    {
        let request = send(app, "/api/marketplace/install", Some(body));
        tokio::pin!(request);
        assert!(futures_util::poll!(request.as_mut()).is_pending());
        // Dropping the response future must leave the registered operation supervised.
    }
    manager.shutdown().await.unwrap();
    assert!(
        fixture
            .store
            .installation("dev.bkm.wasm-ping/default")
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn marketplace_updated_running_instance_can_still_be_stopped() {
    let (fixture, _, data) = fixture();
    set_market_version(&data, "1.0.0");
    let mut record = fixture.installation.clone();
    record.instance_id = "dev.bkm.wasm-ping/default".into();
    fixture.store.upsert_installation(&record).unwrap();
    let mut host = StaticPluginHost::new(fixture.store.clone());
    host.register(
        crate::plugin_configuration::checked_plugin(&record)
            .await
            .unwrap(),
        &record.instance_id,
        record.config.clone(),
        record.granted_permissions.iter().cloned().collect(),
    )
    .await
    .unwrap();
    let mut manager = PluginManager::new(
        host,
        fixture.store.clone(),
        BTreeMap::from([(record.instance_id.clone(), record.package_sha256.clone())]),
        BTreeSet::new(),
    )
    .with_marketplace(
        fixture.directory.join("plugins.db"),
        None,
        DEFAULT_MARKETPLACE_URL.to_owned(),
    )
    .unwrap();
    manager.use_marketplace_fixture(data);
    let app = app(manager.clone());
    let body = intent(app.clone()).await;
    let (status, _) = send(app, "/api/marketplace/update", Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    let saved = fixture
        .store
        .installation(&record.instance_id)
        .unwrap()
        .unwrap();
    manager
        .save_enabled(&record.instance_id, saved.updated_at_ms, false)
        .await
        .unwrap();
    let saved = fixture
        .store
        .installation(&record.instance_id)
        .unwrap()
        .unwrap();
    manager
        .apply(&record.instance_id, saved.updated_at_ms)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(manager.view(&record.instance_id).await.unwrap()).unwrap()["runtime_state"],
        "stopped"
    );
    manager.shutdown().await.unwrap();
}
