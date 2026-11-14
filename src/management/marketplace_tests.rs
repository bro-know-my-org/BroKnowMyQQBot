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
    .with_marketplace(DEFAULT_MARKETPLACE_URL.to_owned())
    .unwrap();
    manager.use_marketplace_fixture(data.clone());
    (fixture, manager, data)
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
