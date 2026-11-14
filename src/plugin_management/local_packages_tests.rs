use super::*;
use crate::plugin_configuration::fixture::Fixture;

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
