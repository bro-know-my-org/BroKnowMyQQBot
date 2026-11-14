use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use plugin_host::{LogicalArchive, PluginStore, StaticPluginHost};
use std::collections::{BTreeMap, BTreeSet};
use tower::ServiceExt as _;

const TOKEN: &str = "backup-http-test-token-0123456789abcdef";

#[tokio::test]
async fn unpolled_download_releases_slot_on_deadline_and_shutdown() {
    for deadline in [true, false] {
        let (root, _, manager, slots) = fixture();
        let path = root.join("stream-fixture");
        std::fs::write(&path, vec![0; 256 * 1024]).unwrap();
        let (shutdown, signal) = bot_core::shutdown_channel();
        let backpressure = Arc::new(tokio::sync::Notify::new());
        let state = Backups {
            manager: manager.clone(),
            slots: slots.clone(),
            repository: None,
            shutdown: Some(signal),
            backpressure: Some(backpressure.clone()),
        };
        let body = download_body(
            &state,
            std::fs::File::open(path).unwrap(),
            slots.clone().acquire_owned().await.unwrap(),
            if deadline {
                std::time::Duration::from_secs(3)
            } else {
                std::time::Duration::from_secs(300)
            },
        )
        .ok()
        .expect("stream supervisor accepts fixture");
        tokio::time::timeout(std::time::Duration::from_secs(2), backpressure.notified())
            .await
            .unwrap();
        assert_eq!(slots.available_permits(), 0);
        if !deadline {
            shutdown.shutdown();
        }
        let permit = tokio::time::timeout(std::time::Duration::from_secs(5), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        manager.shutdown().await.unwrap();
        drop((body, permit));
        std::fs::remove_dir_all(root).unwrap();
    }
}
fn fixture() -> (std::path::PathBuf, Router, PluginManager, Arc<Semaphore>) {
    let root = std::env::temp_dir().join(format!("bkm-backup-http-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let config = root.join("bot.toml");
    std::fs::write(&config, "[commands]\nprefix='!!'\n").unwrap();
    let store = PluginStore::in_memory().unwrap();
    let manager = PluginManager::new(
        StaticPluginHost::new(store.clone()),
        store,
        BTreeMap::new(),
        BTreeSet::new(),
    );
    let manager = manager
        .with_marketplace(
            root.join("plugins.db"),
            None,
            "https://example.com/index.json".into(),
        )
        .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let auth: AuthState = super::super::plugins::token_digest(TOKEN).unwrap().into();
    let app = routes(
        Backups {
            manager: manager.clone(),
            slots: slots.clone(),
            shutdown: None,
            backpressure: None,
            repository: Some(Repository::new(&root.join("plugins.db"), config)),
        },
        auth,
    )
    .layer(middleware::from_fn(super::super::plugins::security_headers));
    (root, app, manager, slots)
}
fn request(path: &str, body: Option<&str>, authenticated: bool) -> Request<Body> {
    let mut request = Request::builder()
        .uri(path)
        .header("host", "localhost:52743");
    if authenticated {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    if body.is_some() {
        request = request
            .method("POST")
            .header("content-type", "application/json");
    }
    request
        .body(Body::from(body.unwrap_or("").to_owned()))
        .unwrap()
}

#[tokio::test]
async fn export_auth_confirmation_download_body_slots_and_security_headers() {
    let (root, app, manager, slots) = fixture();
    let rejected = app
        .clone()
        .oneshot(request("/api/backups", Some("malformed"), false))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    let rejected = app
        .clone()
        .oneshot(request(
            "/api/backups",
            Some(r#"{"confirm_sensitive_plugin_data":false}"#),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(request(
            "/api/backups",
            Some(r#"{"confirm_sensitive_plugin_data":true}"#),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let artifact: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    let url = format!("/api/backups/{}", artifact["id"].as_str().unwrap());
    let download = app
        .clone()
        .oneshot(request(&url, None, true))
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.headers()["cache-control"], "no-store");
    assert!(
        download.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("attachment; filename=\"bkmqb-")
    );
    assert_eq!(download.headers()["x-content-type-options"], "nosniff");
    let busy = app
        .clone()
        .oneshot(request(&url, None, true))
        .await
        .unwrap();
    assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);
    let bytes = to_bytes(download.into_body(), crate::backup::LIMIT)
        .await
        .unwrap();
    assert_eq!(
        LogicalArchive::from_bytes(&bytes).unwrap().configuration["bot"]["commands"]["prefix"],
        "!!"
    );
    drop(
        tokio::time::timeout(std::time::Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap(),
    );
    let download = app
        .clone()
        .oneshot(request(&url, None, true))
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    drop(download);
    assert_eq!(
        app.clone()
            .oneshot(request(&url, None, false))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    drop(
        tokio::time::timeout(std::time::Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap(),
    );
    let listing = app
        .oneshot(request("/api/backups", None, true))
        .await
        .unwrap();
    assert_eq!(listing.status(), StatusCode::OK);
    manager.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cookie_free_local_mode_still_refuses_cross_site_export_and_download() {
    let store = PluginStore::in_memory().unwrap();
    let manager = PluginManager::new(
        StaticPluginHost::new(store.clone()),
        store,
        BTreeMap::new(),
        BTreeSet::new(),
    );
    let config = crate::config::ManagementConfig {
        auth: crate::config::ManagementAuth::None,
        ..crate::config::ManagementConfig::default()
    };
    let auth = AuthState::from_config(&config).unwrap();
    let app = router(manager, auth, None);
    for (path, method) in [
        ("/api/backups", "POST"),
        ("/api/backups/11111111-1111-4111-8111-111111111111", "GET"),
    ] {
        let request = Request::builder()
            .uri(path)
            .method(method)
            .header("host", "localhost:52743")
            .header("origin", "https://foreign.invalid")
            .header("sec-fetch-site", "cross-site")
            .body(Body::from("malformed"))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}
