use super::super::plugins::{router, token_digest};
use crate::plugin_configuration::fixture::Fixture;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt as _;

const TOKEN: &str = "local-package-test-token-0123456789abcdef";

async fn local_request(
    app: &axum::Router,
    path: &str,
    body: Body,
    binary: bool,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Authorization", format!("Bearer {TOKEN}"))
                .header(
                    "Content-Type",
                    if binary {
                        "application/octet-stream"
                    } else {
                        "application/json"
                    },
                )
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap(),
    )
}

async fn inspect(app: &axum::Router, bytes: &[u8]) -> Value {
    let (status, inspection) = local_request(
        app,
        "/api/plugins/local/inspect",
        Body::from(bytes.to_vec()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    inspection
}

fn commit_body(inspection: &Value, id: &str, revision: Option<i64>) -> Value {
    serde_json::json!({"selection":inspection["selection"], "sha256":inspection["sha256"], "plugin_id":inspection["plugin_id"], "version":inspection["version"], "instance_id":id, "expected_updated_at_ms":revision, "grants":[], "confirm_unsigned":true, "confirm":true})
}

async fn commit(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    local_request(
        app,
        "/api/plugins/local/commit",
        Body::from(body.to_string()),
        false,
    )
    .await
}

#[tokio::test]
async fn local_commit_is_fixed_confirmed_cas_protected_and_preserves_disabled_update() {
    let fixture = Fixture::new(false);
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    let app = router(manager.clone(), token_digest(TOKEN).unwrap());
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let id = &fixture.installation.instance_id;
    let inspection = inspect(&app, &bytes).await;
    let mut body = commit_body(&inspection, id, Some(1));
    body["confirm"] = false.into();
    assert_eq!(commit(&app, body.clone()).await.0, StatusCode::BAD_REQUEST);
    body["confirm"] = true.into();
    body["confirm_unsigned"] = false.into();
    assert_eq!(commit(&app, body.clone()).await.0, StatusCode::BAD_REQUEST);
    body["confirm_unsigned"] = true.into();
    body["sha256"] = "0".repeat(64).into();
    assert_eq!(commit(&app, body).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        fixture.store.installation(id).unwrap().unwrap(),
        fixture.installation
    );
    let inspection = inspect(&app, &bytes).await;
    assert_eq!(
        commit(&app, commit_body(&inspection, id, None)).await.0,
        StatusCode::CONFLICT
    );
    let inspection = inspect(&app, &bytes).await;
    let (status, installed) = commit(&app, commit_body(&inspection, id, Some(1))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(installed["enabled"], false);
    assert_eq!(installed["restart_required"], true);
    assert_eq!(installed["loaded_version"], Value::Null);
    let saved = fixture.store.installation(id).unwrap().unwrap();
    assert_eq!(saved.config, fixture.installation.config);
    assert_eq!(
        saved.granted_permissions,
        fixture.installation.granted_permissions
    );
    assert_eq!(
        commit(
            &app,
            commit_body(&inspection, id, Some(saved.updated_at_ms))
        )
        .await
        .0,
        StatusCode::GONE
    );
    let inspection = inspect(&app, &bytes).await;
    let (status, installed) = commit(
        &app,
        commit_body(&inspection, "dev.bkm.wasm-ping/new", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(installed["enabled"], false);
    assert!(
        !installed["missing_permissions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_commit_rechecks_declarations_and_identity_without_accepting_host_paths() {
    let fixture = Fixture::new(false);
    let declaration = fixture.directory.join("declarations.toml");
    std::fs::write(&declaration, "bundled = []\n").unwrap();
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            Some(declaration.clone()),
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    let app = router(manager, token_digest(TOKEN).unwrap());
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let id = &fixture.installation.instance_id;
    for (key, value, status) in [
        (
            "instance_id",
            serde_json::json!("../../escape"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "path",
            serde_json::json!("/host/private"),
            StatusCode::BAD_REQUEST,
        ),
        (
            "grants",
            serde_json::json!(["arbitrary.permission"]),
            StatusCode::BAD_REQUEST,
        ),
        (
            "instance_id",
            serde_json::json!("dev.bkm.ping/default"),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let inspection = inspect(&app, &bytes).await;
        let mut body = commit_body(&inspection, id, Some(1));
        body[key] = value;
        assert_eq!(commit(&app, body).await.0, status);
        // Invalid pre-admission requests intentionally preserve a usable ticket.
        local_request(
            &app,
            "/api/plugins/local/discard",
            Body::from(serde_json::json!({"selection":inspection["selection"]}).to_string()),
            false,
        )
        .await;
    }
    let inspection = inspect(&app, &bytes).await;
    std::fs::write(
        &declaration,
        format!(
            "bundled = []\n[[wasm]]\npackage = 'unused'\ninstance_id = '{id}'\nenabled = false\n"
        ),
    )
    .unwrap();
    assert_eq!(
        commit(&app, commit_body(&inspection, id, Some(1))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture.store.installation(id).unwrap().unwrap(),
        fixture.installation
    );
    assert!(!fixture.directory.join("plugins").exists());
}

#[tokio::test]
async fn irregular_chunks_cannot_grow_retained_upload_allocation_past_budget() {
    let stream = futures_util::stream::iter(
        [17, 1].map(|n| Ok::<_, std::io::Error>(vec![0; n * 1024 * 1024])),
    );
    let bytes = super::read_upload(Body::from_stream(stream)).await.unwrap();
    assert_eq!(bytes.len(), 18 * 1024 * 1024);
    assert_eq!(bytes.capacity(), super::MAX_UPLOAD_BYTES);
}

#[tokio::test]
async fn inspect_uses_real_http_auth_source_limits_and_never_installs() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let app = router(manager.clone(), token_digest(TOKEN).unwrap());
    let send = |body: Body, authorized: bool, origin: &'static str| {
        let app = app.clone();
        async move {
            let mut request = Request::post("/api/plugins/local/inspect")
                .header("Host", "localhost:52743")
                .header("Origin", origin)
                .header("Content-Type", "application/octet-stream");
            if authorized {
                request = request.header("Authorization", format!("Bearer {TOKEN}"));
            }
            app.oneshot(request.body(body).unwrap()).await.unwrap()
        }
    };
    assert_eq!(
        send(Body::from("invalid"), false, "http://localhost:52743")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(Body::from("invalid"), true, "https://evil.example")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // Missing Content-Length, streaming oversize: hard bound still applies.
    let failed = futures_util::stream::once(async {
        Err::<Vec<u8>, _>(std::io::Error::other("fixture read failure"))
    });
    assert_eq!(
        send(Body::from_stream(failed), true, "http://localhost:52743")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let stream =
        futures_util::stream::iter((0..33).map(|_| Ok::<_, std::io::Error>(vec![0; 1024 * 1024])));
    assert_eq!(
        send(Body::from_stream(stream), true, "http://localhost:52743")
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for invalid in [b"invalid".as_slice(), b"PK\x03\x04".as_slice()] {
        assert_eq!(
            send(Body::from(invalid.to_vec()), true, "http://localhost:52743")
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let response = send(Body::from(bytes), true, "http://localhost:52743").await;
    assert_eq!(response.status(), StatusCode::OK);
    let data: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(data["sha256"], fixture.installation.package_sha256);
    assert_eq!(data["plugin_id"], fixture.installation.plugin_id);
    assert_eq!(data["signature_status"], "unsigned");
    assert!(uuid::Uuid::parse_str(data["selection"].as_str().unwrap()).is_ok());
    assert_eq!(
        fixture.store.installations().unwrap(),
        vec![fixture.installation.clone()]
    );
    assert!(!fixture.directory.join("plugins").exists());
    assert!(
        manager
            .list()
            .await
            .unwrap()
            .iter()
            .all(|p| serde_json::to_value(p).unwrap()["loaded_version"].is_null())
    );
}

fn package_with_required_config(fixture: &Fixture) -> Vec<u8> {
    use std::io::Write as _;
    let original =
        plugin_host::ValidatedPluginPackage::from_path(&fixture.installation.package_path).unwrap();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in original.files() {
        if name == "config.schema.json" {
            continue;
        }
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.start_file(
        "config.schema.json",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(
        br#"{"type":"object","required":["greeting"],"properties":{"greeting":{"type":"string"}}}"#,
    )
    .unwrap();
    zip.start_file(
        "fixture.bin",
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .unwrap();
    zip.write_all(&vec![7; 1024 * 1024 + 1]).unwrap();
    zip.finish().unwrap().into_inner()
}

#[tokio::test]
async fn large_binary_route_ignores_false_length_and_missing_config_is_saved_disabled() {
    let fixture = Fixture::new(false);
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    let app = router(manager.clone(), token_digest(TOKEN).unwrap());
    let bytes = package_with_required_config(&fixture);
    assert!(bytes.len() > 1024 * 1024);
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/plugins/local/inspect")
                .header("Authorization", format!("Bearer {TOKEN}"))
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", "1")
                .body(Body::from(bytes))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let checked: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let mut body = commit_body(&checked, "dev.bkm.wasm-ping/config-required", None);
    body["grants"] = checked["requested_permissions"].clone();
    let (status, installed) = commit(&app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(installed["config_incomplete"], true);
    assert_eq!(installed["enabled"], false);
    let stored = fixture
        .store
        .installation("dev.bkm.wasm-ping/config-required")
        .unwrap()
        .unwrap();
    assert!(!stored.enabled);
    assert!(stored.config.is_empty());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn uploads_share_the_existing_four_request_slots() {
    let fixture = Fixture::new(false);
    let app = router(fixture.manager().await, token_digest(TOKEN).unwrap());
    let (ready, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let mut held = Vec::new();
    for _ in 0..4 {
        let ready = ready.clone();
        let mut announced = false;
        let stream = futures_util::stream::poll_fn(move |_| {
            if !announced {
                ready.send(()).unwrap();
                announced = true;
            }
            std::task::Poll::Pending::<Option<Result<Vec<u8>, std::io::Error>>>
        });
        let app = app.clone();
        held.push(tokio::spawn(async move {
            app.oneshot(
                Request::post("/api/plugins/config")
                    .header("Authorization", format!("Bearer {TOKEN}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from_stream(stream))
                    .unwrap(),
            )
            .await
        }));
    }
    for _ in 0..4 {
        tokio::time::timeout(std::time::Duration::from_secs(2), observed.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let (status, _) = local_request(
        &app,
        "/api/plugins/local/inspect",
        Body::from("invalid"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    for request in held {
        request.abort();
        let _ = request.await;
    }
}

#[tokio::test]
async fn auth_none_uploads_still_require_loopback_same_origin() {
    use super::super::{auth::AuthState, plugins::router_with_logs};
    let fixture = Fixture::new(false);
    let config = crate::config::ManagementConfig {
        auth: crate::config::ManagementAuth::None,
        ..Default::default()
    };
    let auth = AuthState::from_config(&config).unwrap();
    let app = router_with_logs(
        fixture.manager().await,
        auth,
        crate::logging::live::LogBuffer::default(),
    );
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    for (host, origin, expected) in [
        ("localhost:52743", "http://localhost:52743", StatusCode::OK),
        (
            "localhost:52743",
            "https://evil.example",
            StatusCode::FORBIDDEN,
        ),
        (
            "public.example:52743",
            "http://public.example:52743",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/plugins/local/inspect")
                    .header("Host", host)
                    .header("Origin", origin)
                    .header("Content-Type", "application/octet-stream")
                    .body(Body::from(bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(fixture.store.installations().unwrap().len(), 1);
}

#[tokio::test]
async fn local_update_cannot_replace_an_existing_different_identity() {
    let fixture = Fixture::new(false);
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    let app = router(manager.clone(), token_digest(TOKEN).unwrap());
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let checked = inspect(&app, &bytes).await;
    let mut other = fixture.installation.clone();
    other.plugin_id = "dev.different.identity".to_owned();
    fixture.store.upsert_installation(&other).unwrap();
    assert_eq!(
        commit(&app, commit_body(&checked, &other.instance_id, Some(1)))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fixture
            .store
            .installation(&other.instance_id)
            .unwrap()
            .unwrap(),
        other
    );
    assert!(!fixture.directory.join("plugins").exists());
    manager.shutdown().await.unwrap();
}
