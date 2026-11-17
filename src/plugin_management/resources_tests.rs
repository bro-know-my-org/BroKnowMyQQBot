use super::*;
use crate::plugin_configuration::fixture::Fixture;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use base64::Engine as _;
use plugin_api::StateOp;
use plugin_host::{CommitOptions, StaticPluginHost, ValidatedPluginPackage};
use serde_json::json;
use std::io::{Cursor, Write as _};
use tower::ServiceExt as _;
use zip::{ZipWriter, write::SimpleFileOptions};

const TOKEN: &str = "management-resource-token-0123456789abcdef0123456789";
pub(super) fn fixture() -> Fixture {
    let mut fixture = Fixture::new(false);
    let original = ValidatedPluginPackage::from_path(&fixture.installation.package_path).unwrap();
    let mut manifest = original.manifest().clone();
    manifest.permissions.storage_quota_bytes = 1024;
    let component = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../crates/plugin-host/src/wasm/fixtures/management.component.b64")
                .trim(),
        )
        .unwrap();
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    for (path, bytes) in original.files() {
        if !matches!(
            path,
            "plugin.toml" | "component.wasm" | "config.schema.json"
        ) {
            archive
                .start_file(path, SimpleFileOptions::default())
                .unwrap();
            archive.write_all(bytes).unwrap();
        }
    }
    for (path,bytes) in [
        ("plugin.toml",toml::to_string(&manifest).unwrap().into_bytes()),
        ("component.wasm",component),
        ("management.schema.json",serde_json::to_vec(&json!({"schema_version":1,"resources":[{"id":"repos","title":"仓库","schema":{"type":"object"},"columns":[{"key":"config","title":"设置"}],"operations":["list","create","update","delete"]}]})).unwrap()),
        ("config.schema.json",br#"{"type":"object"}"#.to_vec()),
    ] {
        archive.start_file(path,SimpleFileOptions::default()).unwrap();
        archive.write_all(&bytes).unwrap();
    }
    let bytes = archive.finish().unwrap().into_inner();
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    fixture.installation.package_sha256 = package.package_sha256().to_owned();
    fixture
        .installation
        .config
        .insert("setting".into(), json!("applied-setting"));
    std::fs::write(&fixture.installation.package_path, bytes).unwrap();
    fixture
        .store
        .upsert_installation(&fixture.installation)
        .unwrap();
    fixture
        .store
        .commit(
            &fixture.installation.instance_id,
            "seed",
            &[put("repos/A", b"original/repository")],
            &[],
            CommitOptions::new(1024),
        )
        .unwrap();
    fixture
}
pub(super) fn manager(fixture: &Fixture) -> PluginManager {
    PluginManager::new(
        StaticPluginHost::new(fixture.store.clone()),
        fixture.store.clone(),
        BTreeMap::new(),
        BTreeSet::new(),
    )
    .with_marketplace(
        fixture.directory.join("plugins.db"),
        None,
        "https://example.test/index.json".into(),
    )
    .unwrap()
}
fn put(key: &str, value: &[u8]) -> StateOp {
    StateOp::Put {
        key: key.into(),
        value: value.to_vec(),
        expected_revision: None,
    }
}
fn write(id: &str, revision: u64) -> ResourceWrite {
    serde_json::from_value(json!({"instance_id":id,"resource":"repos","operation":"update","record_id":"A","input":{},"expected_revision":revision.to_string(),"expected_updated_at_ms":1})).unwrap()
}
async fn send(app: &Router, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    let body = if let Some(body) = body {
        request = request
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn authenticated_http_reads_and_writes_unloaded_guest_without_business_hooks() {
    let fixture = fixture();
    let manager = manager(&fixture);
    let id = &fixture.installation.instance_id;
    let app = crate::management::plugin_test_router(manager.clone(), TOKEN);
    let (status, descriptor) = send(
        &app,
        &format!("/api/plugins/management?instance_id={id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(descriptor["descriptor"]["resources"][0]["title"], "仓库");
    assert_eq!(descriptor["editable"], true);
    let path = format!("/api/plugins/management/resource?instance_id={id}&resource=repos");
    let (status, listed) = send(&app, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed["data"]["records"][0]["values"]["config"],
        "applied-setting"
    );
    assert!(listed.get("state_ops").is_none());
    let request = json!({"instance_id":id,"resource":"repos","operation":"update","record_id":"A","input":{},"expected_revision":listed["revision"],"expected_updated_at_ms":1});
    let (status, result) = send(
        &app,
        "/api/plugins/management/resource",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_ne!(result["revision"], listed["revision"]);
    assert_eq!(
        send(&app, "/api/plugins/management/resource", Some(request))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert!(fixture.store.state_get(id, "repos/A").unwrap().is_none());
    assert_eq!(
        fixture
            .store
            .state_get(id, "repos/B")
            .unwrap()
            .unwrap()
            .value,
        b"original/repository"
    );
    assert_eq!(
        send(&app, &path, None).await.1["data"]["records"],
        json!([])
    );
    assert!(!fixture.store.installation(id).unwrap().unwrap().enabled);
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_chat_state_list_mutations_schema_and_confirmations_are_rejected() {
    let fixture = fixture();
    let manager = manager(&fixture);
    let id = &fixture.installation.instance_id;
    let (revision, _) = fixture.store.management_snapshot(id).unwrap();
    fixture
        .store
        .commit(
            id,
            "chat",
            &[put("repos/C", b"chat/repository")],
            &[],
            CommitOptions::new(1024),
        )
        .unwrap();
    assert!(matches!(
        manager.resource_write(write(id, revision)).await,
        Err(ManagementError::Conflict)
    ));
    for search in ["bad_list", "error"] {
        let request = ManagementRequest {
            resource: "repos".into(),
            operation: ManagementOperation::List,
            search: search.into(),
            cursor: None,
            limit: 20,
            record_id: None,
            input: None,
        };
        assert!(manager.resource_list(id.clone(), request).await.is_err());
    }
    let current = fixture.store.management_snapshot(id).unwrap().0;
    let mut deletion = write(id, current);
    deletion.operation = ManagementOperation::Delete;
    assert!(matches!(
        manager.resource_write(deletion).await,
        Err(ManagementError::InvalidOperation)
    ));
    let mut bad_revision = write(id, current);
    bad_revision.expected_revision = "01".into();
    assert!(matches!(
        manager.resource_write(bad_revision).await,
        Err(ManagementError::InvalidOperation)
    ));
    let mut stale_config = write(id, current);
    stale_config.expected_updated_at_ms = 0;
    assert!(matches!(
        manager.resource_write(stale_config).await,
        Err(ManagementError::Conflict)
    ));
    assert_eq!(fixture.store.management_snapshot(id).unwrap().0, current);
    assert!(fixture.store.state_get(id, "repos/B").unwrap().is_none());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_http_caller_does_not_cancel_a_supervised_write() {
    let fixture = fixture();
    let manager = manager(&fixture);
    let id = fixture.installation.instance_id.clone();
    let revision = fixture.store.management_snapshot(&id).unwrap().0;
    let lock = manager.hold_writes_for_test().await;
    let mut request = Box::pin(manager.resource_write(write(&id, revision)));
    assert!(futures_util::poll!(&mut request).is_pending());
    drop(request);
    drop(lock);
    manager.shutdown().await.unwrap();
    assert!(fixture.store.state_get(&id, "repos/A").unwrap().is_none());
    assert!(fixture.store.state_get(&id, "repos/B").unwrap().is_some());
}

#[tokio::test]
async fn saved_package_mismatch_is_readonly_and_legacy_plugins_keep_config_only() {
    let fixture = fixture();
    let manager = manager(&fixture);
    let id = &fixture.installation.instance_id;
    let mut host = StaticPluginHost::new(fixture.store.clone());
    host.register_trusted(
        std::sync::Arc::new(builtin_plugins::PingPlugin::default()),
        id,
        BTreeMap::new(),
    )
    .await
    .unwrap();
    {
        let mut runtime = manager.runtime.write().unwrap();
        runtime.host = host;
        runtime
            .packages
            .insert(id.clone(), "previous-package-hash".into());
    }
    let descriptor = manager.resource_descriptor(id).await.unwrap();
    assert!(!descriptor.editable);
    assert!(descriptor.unavailable_reason.is_some());
    let revision = fixture.store.management_snapshot(id).unwrap().0;
    assert!(matches!(
        manager.resource_write(write(id, revision)).await,
        Err(ManagementError::RestartRequired)
    ));
    manager.shutdown().await.unwrap();
    let legacy = Fixture::new(false);
    let manager = self::manager(&legacy);
    assert!(
        manager
            .resource_descriptor(&legacy.installation.instance_id)
            .await
            .unwrap()
            .descriptor
            .is_none()
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn unloaded_unknown_state_schema_is_readonly_without_implicit_migration() {
    let mut fixture = fixture();
    let original = ValidatedPluginPackage::from_path(&fixture.installation.package_path).unwrap();
    let mut manifest = original.manifest().clone();
    manifest.state_version = 2;
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    for (path, bytes) in original.files().filter(|(path, _)| *path != "plugin.toml") {
        archive
            .start_file(path, SimpleFileOptions::default())
            .unwrap();
        archive.write_all(bytes).unwrap();
    }
    archive
        .start_file("plugin.toml", SimpleFileOptions::default())
        .unwrap();
    archive
        .write_all(toml::to_string(&manifest).unwrap().as_bytes())
        .unwrap();
    let bytes = archive.finish().unwrap().into_inner();
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    fixture.installation.package_sha256 = package.package_sha256().into();
    std::fs::write(&fixture.installation.package_path, bytes).unwrap();
    fixture
        .store
        .upsert_installation(&fixture.installation)
        .unwrap();
    let manager = manager(&fixture);
    let id = &fixture.installation.instance_id;
    let descriptor = manager.resource_descriptor(id).await.unwrap();
    assert!(!descriptor.editable);
    let revision = fixture.store.management_snapshot(id).unwrap().0;
    assert!(matches!(
        manager.resource_write(write(id, revision)).await,
        Err(ManagementError::RestartRequired)
    ));
    assert_eq!(fixture.store.management_snapshot(id).unwrap().0, revision);
    manager.shutdown().await.unwrap();
}
