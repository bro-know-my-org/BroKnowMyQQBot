use super::super::plugins::{router, token_digest};
use crate::plugin_configuration::fixture::Fixture;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const TOKEN: &str = "permissions-fixture-token-0123456789abcdef";

async fn send(app: &Router, id: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(if body.is_some() {
            "/api/plugins/permissions".to_owned()
        } else {
            format!("/api/plugins/permissions?instance_id={id}")
        })
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn permissions_revoke_live_authority_and_preserve_disabled_config_on_addition() {
    let fixture = Fixture::new(true);
    let id = &fixture.installation.instance_id;
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
    let (_, permissions) = send(&app, id, None).await;
    assert_eq!(
        permissions["granted_permissions"],
        json!(fixture.installation.granted_permissions)
    );
    let body = |revision, grants: Value, confirm| json!({"instance_id": id, "expected_updated_at_ms": revision, "grants": grants, "confirm": confirm});
    assert_eq!(
        send(&app, id, Some(body(1, json!([]), false))).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            id,
            Some(body(1, json!(["arbitrary.permission"]), true))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, id, Some(body(0, json!([]), true))).await.0,
        StatusCode::CONFLICT
    );
    assert!(fixture.store.installation(id).unwrap().unwrap().enabled);
    let (status, saved) = send(&app, id, Some(body(1, json!([]), true))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["plugin"]["runtime_state"], "disabled");
    assert_eq!(saved["plugin"]["enabled"], false);
    assert_eq!(saved["plugin"]["restart_required"], true);
    let revision = saved["plugin"]["updated_at_ms"].as_i64().unwrap();
    let (status, saved) = send(
        &app,
        id,
        Some(body(
            revision,
            permissions["requested_permissions"].clone(),
            true,
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["plugin"]["enabled"], false);
    assert_eq!(saved["plugin"]["runtime_state"], "disabled");
    assert_eq!(
        saved["plugin"]["config"],
        json!(fixture.installation.config)
    );
    assert!(
        manager
            .apply(id, saved["plugin"]["updated_at_ms"].as_i64().unwrap())
            .await
            .is_ok()
    ); // disabled remains retired
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn permissions_are_verified_not_taken_from_mutable_installation_metadata() {
    let fixture = Fixture::new(false);
    let id = &fixture.installation.instance_id;
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
    let app = router(manager.clone(), token_digest(TOKEN).unwrap());
    let mut item = fixture.installation.clone();
    item.requested_permissions = vec!["arbitrary.permission".to_owned()];
    fixture.store.upsert_installation(&item).unwrap();
    let (status, permissions) = send(&app, id, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !permissions["requested_permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("arbitrary.permission"))
    );
    std::fs::write(
        &declaration,
        format!(
            "bundled = []\n[[wasm]]\npackage = 'unused'\ninstance_id = '{id}'\nenabled = false\n"
        ),
    )
    .unwrap();
    assert_eq!(send(&app, id, None).await.0, StatusCode::NOT_FOUND);
    std::fs::write(&declaration, "bundled = []\n").unwrap();
    std::fs::write(&item.package_path, "tampered").unwrap();
    assert_eq!(
        send(&app, id, None).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(
            &app,
            id,
            Some(json!({"instance_id":id,"expected_updated_at_ms":1,"grants":[],"confirm":true}))
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(fixture.store.installation(id).unwrap().unwrap(), item);
}
