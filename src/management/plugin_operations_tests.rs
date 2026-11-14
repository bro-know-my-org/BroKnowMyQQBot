use std::fmt::Write as _;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::super::plugins::{router, token_digest};
use crate::{plugin_configuration::fixture::Fixture, plugin_management::PluginManager};

const TOKEN: &str = "plugin-operations-test-0123456789abcdef";

async fn send(app: &Router, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Content-Type", "application/json")
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn operations_require_confirmation_and_revision_and_protect_file_sources() {
    let fixture = Fixture::new(false);
    let id = &fixture.installation.instance_id;
    let declaration = fixture.directory.join("declarations.toml");
    std::fs::write(
        &declaration,
        format!(
            "bundled = []\n[[wasm]]\npackage = 'unused'\ninstance_id = '{id}'\nenabled = false\n"
        ),
    )
    .unwrap();
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
    for operation in ["uninstall", "recover"] {
        let path = format!("/api/plugins/{operation}");
        assert_eq!(
            send(
                &app,
                &path,
                Some(json!({"instance_id":id,"expected_updated_at_ms":1,"confirm":false}))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(
                &app,
                &path,
                Some(json!({"instance_id":id,"expected_updated_at_ms":1,"confirm":true}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    std::fs::write(&declaration, "bundled = []\n").unwrap();
    for operation in ["uninstall", "recover"] {
        let path = format!("/api/plugins/{operation}");
        assert_eq!(
            send(
                &app,
                &path,
                Some(json!({"instance_id":id,"expected_updated_at_ms":0,"confirm":true}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(send(&app, &path, Some(json!({"instance_id":"dev.bkm.ping/default","expected_updated_at_ms":0,"confirm":true}))).await.0, StatusCode::NOT_FOUND);
    }
    let (status, recovered) = send(
        &app,
        "/api/plugins/recover",
        Some(json!({"instance_id":id,"expected_updated_at_ms":1,"confirm":true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(recovered["restart_required"], true);
    assert_eq!(recovered["plugin"]["runtime_state"], "not_loaded");
    assert_eq!(recovered["recovered"], 0);
    let (status, removed) = send(&app, "/api/plugins/uninstall", Some(json!({"instance_id":id,"expected_updated_at_ms":recovered["plugin"]["updated_at_ms"],"confirm":true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed["private_state_retained"], true);
    assert!(fixture.store.installation(id).unwrap().is_none());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn failure_list_is_paginated_and_validated_for_existing_instances() {
    let fixture = Fixture::new(false);
    let mut other = fixture.installation.clone();
    other.instance_id = "other-instance".to_owned();
    fixture.store.upsert_installation(&other).unwrap();
    // Seed only this temporary database, including deliberately private sentinels.
    let connection = rusqlite::Connection::open(fixture.directory.join("plugins.db")).unwrap();
    for index in 1..=5 {
        connection.execute(
            "INSERT INTO plugin_deliveries (instance_id,event_id,delivery_id,invocation_id,attempt,envelope,origin,status,next_attempt_ms,last_error,error_class,updated_at_ms)
             VALUES (?1,?2,'secret-delivery','secret-invocation',?3,'secret-body','secret-origin','dead_letter',0,'secret-diagnostic',?4,?3)",
            rusqlite::params![fixture.installation.instance_id, format!("secret-event-{index}"), index, if index == 5 { "secret-class" } else { "guest_trap" }],
        ).unwrap();
        for (instance, status) in [
            (other.instance_id.as_str(), "dead_letter"),
            (fixture.installation.instance_id.as_str(), "pending"),
        ] {
            connection.execute(
                "INSERT INTO plugin_deliveries (instance_id,event_id,delivery_id,invocation_id,attempt,envelope,origin,status,next_attempt_ms,last_error,error_class,updated_at_ms)
                 VALUES (?1,?2,'secret-delivery','secret-invocation',999,'secret-body','secret-origin',?3,0,'secret-diagnostic','guest_trap',999)",
                rusqlite::params![instance, format!("secret-excluded-{index}"), status],
            ).unwrap();
        }
    }
    drop(connection);
    let manager = fixture.manager().await;
    let app = router(manager, token_digest(TOKEN).unwrap());
    let id = &fixture.installation.instance_id;
    let mut before = None;
    let mut attempts = Vec::new();
    loop {
        let mut path = format!("/api/plugins/failures?instance_id={id}&limit=2");
        if let Some(cursor) = before {
            write!(path, "&before={cursor}").unwrap();
        }
        let (status, page) = send(&app, &path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!page.to_string().contains("secret"));
        let entries = page["entries"].as_array().unwrap();
        assert!(entries.len() <= 2);
        for entry in entries {
            assert_eq!(entry.as_object().unwrap().len(), 3);
            assert_eq!(entry["updated_at_ms"], entry["attempts"]);
            assert_eq!(
                entry["error_class"],
                if entry["attempts"] == 5 {
                    "unknown"
                } else {
                    "guest_trap"
                }
            );
            attempts.push(entry["attempts"].as_u64().unwrap());
        }
        let next = page["next_before"].as_i64();
        if let (Some(previous), Some(next)) = (before, next) {
            assert!(next < previous);
        }
        before = next;
        if before.is_none() {
            break;
        }
        assert!(attempts.len() <= 5);
    }
    assert_eq!(attempts, vec![5, 4, 3, 2, 1]);
    for query in [
        "limit=101",
        "limit=0",
        "before=0",
        "before=-1",
        "extra=secret",
    ] {
        assert_eq!(
            send(
                &app,
                &format!("/api/plugins/failures?instance_id={id}&{query}"),
                None
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(&app, "/api/plugins/failures?instance_id=absent", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn recorded_file_instance_remains_readonly_even_when_disabled_and_declaration_removed() {
    let fixture = Fixture::new(false);
    let id = &fixture.installation.instance_id;
    let manager = PluginManager::new(
        plugin_host::StaticPluginHost::new(fixture.store.clone()),
        fixture.store.clone(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeSet::from([id.clone()]),
    )
    .with_marketplace(
        fixture.directory.join("plugins.db"),
        None,
        crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
    )
    .unwrap();
    let app = router(manager, token_digest(TOKEN).unwrap());
    for operation in ["uninstall", "recover"] {
        assert_eq!(
            send(
                &app,
                &format!("/api/plugins/{operation}"),
                Some(json!({"instance_id":id,"expected_updated_at_ms":1,"confirm":true}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    assert!(fixture.store.installation(id).unwrap().is_some());
}
