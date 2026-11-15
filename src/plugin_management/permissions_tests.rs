use super::*;
use crate::plugin_configuration::fixture::Fixture;
use std::sync::Arc;

fn revoke(fixture: &Fixture) -> PermissionRequest {
    PermissionRequest {
        instance_id: fixture.installation.instance_id.clone(),
        expected_updated_at_ms: 1,
        grants: BTreeSet::new(),
        confirm: true,
    }
}

fn managed(manager: PluginManager, fixture: &Fixture) -> PluginManager {
    manager
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap()
}

#[tokio::test]
async fn retirement_failure_does_not_claim_saved_revocation_or_reactivate_old_clones() {
    let fixture = Fixture::new(true);
    let manager = managed(fixture.rejecting_manager_inner(None, true).await, &fixture);
    let id = &fixture.installation.instance_id;
    let old_host = manager.snapshot().unwrap().host;
    assert!(matches!(
        manager.save_permissions(revoke(&fixture)).await,
        Err(ManagementError::Lifecycle)
    ));
    assert_eq!(
        fixture.store.installation(id).unwrap().unwrap(),
        fixture.installation
    );
    let view = manager.view(id).await.unwrap();
    assert_eq!(view.runtime_state, "disabled");
    assert!(view.restart_required);
    assert!(old_host.set_instance_enabled(id, true).await.is_err());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn cas_conflict_after_retirement_preserves_newer_saved_grants_and_restart_boundary() {
    let fixture = Fixture::new(true);
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let manager = managed(
        fixture
            .rejecting_manager_inner(Some(pause.clone()), true)
            .await,
        &fixture,
    );
    let id = &fixture.installation.instance_id;
    let old_host = manager.snapshot().unwrap().host;
    let request = manager.save_permissions(revoke(&fixture));
    tokio::pin!(request);
    assert!(futures_util::poll!(&mut request).is_pending());
    pause.0.notified().await;
    let mut newer = fixture.installation.clone();
    newer.updated_at_ms = 2;
    newer
        .config
        .insert("greeting".to_owned(), serde_json::json!("keep"));
    fixture.store.write_installation(&newer, Some(1)).unwrap();
    pause.1.notify_one();
    assert!(matches!(request.await, Err(ManagementError::Conflict)));
    assert_eq!(fixture.store.installation(id).unwrap().unwrap(), newer);
    let view = manager.view(id).await.unwrap();
    assert_eq!(view.runtime_state, "disabled");
    assert!(view.restart_required);
    assert!(old_host.set_instance_enabled(id, true).await.is_err());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_revocation_survives_request_cancellation_and_preserves_private_state() {
    let fixture = Fixture::new(true);
    let manager = managed(fixture.manager().await, &fixture);
    let id = &fixture.installation.instance_id;
    let old_host = manager.snapshot().unwrap().host;
    fixture
        .store
        .commit(
            id,
            "private",
            &[plugin_api::StateOp::Put {
                key: "private/value".to_owned(),
                value: b"keep".to_vec(),
                expected_revision: None,
            }],
            &[],
            plugin_host::CommitOptions::new(1024),
        )
        .unwrap();
    let write = manager.writes.lock().await;
    let mut request = Box::pin(manager.save_permissions(revoke(&fixture)));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    drop(request);
    drop(write);
    manager.shutdown().await.unwrap();
    let saved = fixture.store.installation(id).unwrap().unwrap();
    assert_eq!(saved.granted_permissions, [] as [std::string::String; 0]);
    assert!(!saved.enabled);
    assert_eq!(saved.config, fixture.installation.config);
    assert_eq!(
        fixture
            .store
            .state_get(id, "private/value")
            .unwrap()
            .unwrap()
            .value,
        b"keep"
    );
    assert!(old_host.set_instance_enabled(id, true).await.is_err());
}
