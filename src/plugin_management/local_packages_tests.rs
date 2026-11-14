use super::*;
use crate::plugin_configuration::fixture::Fixture;
use std::io::Write as _;

#[tokio::test]
async fn selection_capacity_includes_inflight_transfers_and_expires_without_a_worker() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let first = manager.reserve_upload().unwrap();
    let second = manager.reserve_upload().unwrap();
    assert!(matches!(
        manager.reserve_upload(),
        Err(ManagementError::Busy)
    ));
    let inspection = manager.inspect_local_package(bytes, first).await.unwrap();
    assert_eq!(inspection.sha256, fixture.installation.package_sha256);
    assert_eq!(inspection.signature_status, "unsigned");
    assert!(matches!(
        manager.reserve_upload(),
        Err(ManagementError::Busy)
    ));
    manager
        .local_packages
        .selections
        .lock()
        .unwrap()
        .get_mut(&inspection.selection)
        .unwrap()
        .expires = Instant::now();
    let released = manager.reserve_upload().unwrap();
    assert!(manager.local_packages.selections.lock().unwrap().is_empty());
    drop((second, released));
    assert_eq!(
        manager.local_packages.slots.available_permits(),
        MAX_SELECTIONS
    );
}

fn request(inspection: &LocalInspection, fixture: &Fixture) -> LocalCommit {
    LocalCommit {
        selection: inspection.selection.clone(),
        sha256: inspection.sha256.clone(),
        plugin_id: inspection.plugin_id.clone(),
        version: inspection.version.clone(),
        instance_id: fixture.installation.instance_id.clone(),
        expected_updated_at_ms: Some(1),
        grants: std::collections::BTreeSet::default(),
        confirm_unsigned: true,
        confirm: true,
    }
}

#[tokio::test]
async fn expired_or_mutated_selection_never_installs() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let mut zip = zip::ZipWriter::new_append(std::io::Cursor::new(bytes.clone())).unwrap();
    zip.start_file(
        "fixture-integrity.txt",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(b"changed but still a valid package").unwrap();
    let changed = zip.finish().unwrap().into_inner();
    let parsed = ValidatedPluginPackage::from_bytes(&changed).unwrap();
    assert_eq!(
        parsed.manifest().id.as_str(),
        fixture.installation.plugin_id
    );
    assert_eq!(parsed.manifest().version, fixture.installation.version);
    assert_ne!(parsed.package_sha256(), fixture.installation.package_sha256);
    for expire in [true, false] {
        let inspection = manager
            .inspect_local_package(bytes.clone(), manager.reserve_upload().unwrap())
            .await
            .unwrap();
        {
            let mut staged = manager.local_packages.selections.lock().unwrap();
            let entry = staged.get_mut(&inspection.selection).unwrap();
            if expire {
                entry.expires = Instant::now();
            } else {
                entry.bytes.clone_from(&changed);
            }
        }
        let result = manager
            .commit_local_package(request(&inspection, &fixture))
            .await;
        if expire {
            assert!(matches!(result, Err(ManagementError::ExpiredSelection)));
        } else {
            assert!(matches!(result, Err(ManagementError::InvalidPackage)));
        }
        assert_eq!(
            fixture
                .store
                .installation(&fixture.installation.instance_id)
                .unwrap()
                .unwrap(),
            fixture.installation
        );
    }
    assert!(!fixture.directory.join("plugins").exists());
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_local_commit_survives_cancellation_and_pins_loaded_package_until_shutdown() {
    let mut fixture = Fixture::new(true);
    std::fs::create_dir(fixture.directory.join("plugins")).unwrap();
    let old_path = fixture.directory.join("plugins/old.bkm-plugin");
    std::fs::rename(&fixture.installation.package_path, &old_path).unwrap();
    fixture.installation.package_path = old_path.to_str().unwrap().to_owned();
    fixture
        .store
        .upsert_installation(&fixture.installation)
        .unwrap();
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    let bytes = std::fs::read(&old_path).unwrap();
    let inspection = manager
        .inspect_local_package(bytes, manager.reserve_upload().unwrap())
        .await
        .unwrap();
    let write = manager.writes.lock().await;
    let mut operation = Box::pin(manager.commit_local_package(request(&inspection, &fixture)));
    assert!(futures_util::poll!(operation.as_mut()).is_pending());
    drop(operation);
    drop(write);
    // Drain accepted writes without stopping the loaded guest yet.
    let mut operations = std::mem::take(&mut manager.operations.lock().unwrap().tasks);
    while let Some(operation) = operations.join_next().await {
        operation.unwrap();
    }
    let saved = fixture
        .store
        .installation(&fixture.installation.instance_id)
        .unwrap()
        .unwrap();
    assert!(saved.updated_at_ms > 1);
    assert_ne!(saved.package_path, fixture.installation.package_path);
    assert!(old_path.exists());
    assert!(
        manager
            .snapshot()
            .unwrap()
            .retained_packages
            .contains(&old_path)
    );
    let view = manager
        .view(&fixture.installation.instance_id)
        .await
        .unwrap();
    assert_eq!(view.runtime_state, "ready");
    assert!(view.restart_required);
    manager.shutdown().await.unwrap();
    assert!(!old_path.exists());
    assert!(std::path::Path::new(&saved.package_path).exists());
}
