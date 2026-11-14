use std::fs;

use plugin_api::{
    HandlerOutput, HostQueries, InitContext, PluginError, PluginEventEnvelope, PluginManifest,
    StaticPlugin,
};
use serde_json::json;

use super::*;
use crate::plugin_configuration::fixture::Fixture;

impl Fixture {
    pub(crate) async fn manager(&self) -> PluginManager {
        let mut host = StaticPluginHost::new(self.store.clone());
        if self.installation.enabled {
            host.register(
                checked_plugin(&self.installation).await.unwrap(),
                &self.installation.instance_id,
                self.installation.config.clone(),
                self.installation
                    .granted_permissions
                    .iter()
                    .cloned()
                    .collect(),
            )
            .await
            .unwrap();
        }
        let packages = if self.installation.enabled {
            BTreeMap::from([(
                self.installation.instance_id.clone(),
                self.installation.package_sha256.clone(),
            )])
        } else {
            BTreeMap::new()
        };
        PluginManager::new(host, self.store.clone(), packages, BTreeSet::new())
    }
}

#[tokio::test]
async fn uninstall_stops_active_host_retains_private_state_and_survives_request_cancellation() {
    let mut fixture = Fixture::new(true);
    let directory = fixture.directory.join("plugins");
    fs::create_dir(&directory).unwrap();
    let package = directory.join("installed.bkm-plugin");
    fs::rename(&fixture.installation.package_path, &package).unwrap();
    fixture.installation.package_path = package.to_str().unwrap().to_owned();
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
    let mut request = Box::pin(manager.uninstall(id, 1));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    drop(request);
    drop(write);
    manager.shutdown().await.unwrap();
    assert!(fixture.store.installation(id).unwrap().is_none());
    assert!(!package.exists());
    assert_eq!(
        fixture
            .store
            .state_get(id, "private/value")
            .unwrap()
            .unwrap()
            .value,
        b"keep"
    );
    assert!(manager.list().await.unwrap().is_empty());
    assert_eq!(
        old_host.instance_snapshots().unwrap()[0].state,
        PluginInstanceState::Disabled
    );
    assert!(old_host.set_instance_enabled(id, true).await.is_err());
}

#[tokio::test]
async fn recovery_stops_current_guest_and_only_prepares_restart_without_reusing_it() {
    let fixture = Fixture::new(true);
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    let id = &fixture.installation.instance_id;
    assert!(matches!(
        manager.recover(id, 0).await,
        Err(ManagementError::Conflict)
    ));
    let old_host = manager.snapshot().unwrap().host;
    manager.recover(id, 1).await.unwrap();
    assert!(old_host.set_instance_enabled(id, true).await.is_err());
    let view = manager.view(id).await.unwrap();
    assert_eq!(view.runtime_state, "disabled");
    assert!(view.restart_required);
    assert!(view.updated_at_ms > 1);
    assert!(matches!(
        manager.apply(id, view.updated_at_ms).await,
        Err(ManagementError::RestartRequired)
    ));
    assert!(matches!(
        manager.recover(id, 1).await,
        Err(ManagementError::Conflict)
    ));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn package_update_keeps_a_previous_package_pinned_by_a_loaded_guest() {
    use crate::plugin_installer::{PendingInstallation, finish_installation_retaining};
    let fixture = Fixture::new(false);
    let package = checked_package(&fixture.installation).await.unwrap();
    let mut old = fixture.installation.clone();
    fs::create_dir(fixture.directory.join("plugins")).unwrap();
    let old_path = fixture.directory.join("plugins/old.bkm-plugin");
    fs::copy(&old.package_path, &old_path).unwrap();
    old.package_path = old_path.to_str().unwrap().to_owned();
    old.package_sha256 = "old-loaded-package".to_owned();
    fixture.store.upsert_installation(&old).unwrap();
    let pending = PendingInstallation {
        manifest: package.manifest().clone(),
        instance_id: old.instance_id.clone(),
        package_sha256: package.package_sha256().to_owned(),
        package_bytes: fs::read(&fixture.installation.package_path).unwrap(),
        source: "local".to_owned(),
        requested: BTreeSet::new(),
        granted: BTreeSet::new(),
        config: BTreeMap::new(),
        permissions_complete: true,
        config_error: None,
        update: true,
    };
    let installed = finish_installation_retaining(
        &fixture.store,
        &fixture.directory.join("plugins.db"),
        Some(&old),
        &pending,
        &BTreeSet::from([old.package_sha256.clone()]),
    )
    .unwrap();
    assert_ne!(installed.package_path, old.package_path);
    assert!(old_path.exists());
    let manager = fixture
        .manager()
        .await
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
        )
        .unwrap();
    manager
        .runtime
        .write()
        .unwrap()
        .retained_packages
        .insert(old_path.clone());
    manager.shutdown().await.unwrap();
    assert!(!old_path.exists());
}

#[tokio::test]
async fn saved_configuration_is_validated_versioned_durable_and_applied_separately() {
    let fixture = Fixture::new(true);
    let manager = fixture.manager().await;
    let id = &fixture.installation.instance_id;
    let config = BTreeMap::from([("greeting".to_owned(), json!("你好"))]);
    manager.save_config(id, 1, config.clone()).await.unwrap();
    let saved = manager.view(id).await.unwrap();
    assert!(saved.pending);
    assert!(runtime_config(&manager).is_empty());
    assert!(matches!(
        manager.save_config(id, 1, BTreeMap::new()).await,
        Err(ManagementError::Conflict)
    ));
    assert!(matches!(
        manager
            .save_config(
                id,
                saved.updated_at_ms,
                BTreeMap::from([("greeting".to_owned(), json!(false))])
            )
            .await,
        Err(ManagementError::InvalidConfig)
    ));
    manager.apply(id, saved.updated_at_ms).await.unwrap();
    assert!(!manager.view(id).await.unwrap().pending);
    assert_eq!(runtime_config(&manager), config);
    let reopened = PluginStore::open(fixture.directory.join("plugins.db")).unwrap();
    assert_eq!(reopened.installation(id).unwrap().unwrap().config, config);
    manager
        .save_enabled(id, saved.updated_at_ms, false)
        .await
        .unwrap();
    let stopped = manager.view(id).await.unwrap();
    assert!(stopped.pending);
    assert_eq!(stopped.runtime_state, "ready");
    manager.apply(id, stopped.updated_at_ms).await.unwrap();
    assert_eq!(manager.view(id).await.unwrap().runtime_state, "stopped");
    manager
        .save_config(id, stopped.updated_at_ms, BTreeMap::new())
        .await
        .unwrap();
    let configured = manager.view(id).await.unwrap();
    manager
        .save_enabled(id, configured.updated_at_ms, true)
        .await
        .unwrap();
    let enabled = manager.view(id).await.unwrap();
    manager.apply(id, enabled.updated_at_ms).await.unwrap();
    let active = manager.view(id).await.unwrap();
    assert_eq!(active.runtime_state, "ready");
    assert!(!active.pending);
    manager.shutdown().await.unwrap();
    let declaration = fixture.directory.join("plugins.toml");
    fs::write(&declaration, "bundled = []\n").unwrap();
    let mut restarted = StaticPluginHost::new(reopened.clone());
    crate::plugins::load_plugins(&mut restarted, &reopened, Some(&declaration))
        .await
        .unwrap();
    assert_eq!(
        restarted.instance_snapshots().unwrap()[0].config,
        BTreeMap::new()
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn enabling_an_unloaded_plugin_publishes_it_to_subsequent_dispatch() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let clone = manager.clone();
    let id = &fixture.installation.instance_id;
    manager.save_enabled(id, 1, true).await.unwrap();
    let saved = manager.view(id).await.unwrap();
    assert_eq!(saved.runtime_state, "not_loaded");
    manager.apply(id, saved.updated_at_ms).await.unwrap();
    assert!(
        clone
            .snapshot()
            .unwrap()
            .host
            .instance_manifest(id)
            .is_some()
    );
    assert_eq!(clone.view(id).await.unwrap().runtime_state, "ready");
    manager.shutdown().await.unwrap();
}

type Pause = Arc<(tokio::sync::Notify, tokio::sync::Notify)>;

impl Fixture {
    async fn rejecting_manager(&self, pause: Option<Pause>) -> PluginManager {
        self.rejecting_manager_inner(pause, false).await
    }
    async fn rejecting_manager_inner(&self, pause: Option<Pause>, stop: bool) -> PluginManager {
        let mut host = StaticPluginHost::new(self.store.clone());
        let manifest = checked_package(&self.installation)
            .await
            .unwrap()
            .manifest()
            .clone();
        host.register_trusted(
            Arc::new(RejectingPlugin {
                manifest,
                pause,
                stop,
            }),
            &self.installation.instance_id,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        let packages = if self.installation.enabled {
            BTreeMap::from([(
                self.installation.instance_id.clone(),
                self.installation.package_sha256.clone(),
            )])
        } else {
            BTreeMap::new()
        };
        PluginManager::new(host, self.store.clone(), packages, BTreeSet::new())
    }
}

fn runtime_config(manager: &PluginManager) -> BTreeMap<String, Value> {
    manager
        .snapshot()
        .unwrap()
        .host
        .instance_snapshots()
        .unwrap()
        .remove(0)
        .config
}

struct RejectingPlugin {
    manifest: PluginManifest,
    pause: Option<Pause>,
    stop: bool,
}

#[async_trait]
impl StaticPlugin for RejectingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn init(&self, context: InitContext) -> Result<(), PluginError> {
        if context.config.get("greeting") == Some(&json!("wait")) {
            if let Some(pause) = &self.pause {
                pause.0.notify_one();
                pause.1.notified().await;
            }
        }
        if context.config.get("greeting") == Some(&json!("reject")) {
            return Err(PluginError::InvalidConfig(
                "initialization rejected".to_owned(),
            ));
        }
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), PluginError> {
        if self.stop {
            let Some(pause) = self.pause.as_ref() else {
                return Err(PluginError::Permanent("shutdown failed".to_owned()));
            };
            pause.0.notify_one();
            pause.1.notified().await;
        }
        Ok(())
    }
    async fn on_event(
        &self,
        _: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        Ok(HandlerOutput {
            disposition: plugin_api::Disposition::Ignore,
            ..HandlerOutput::default()
        })
    }
}

#[tokio::test]
async fn failed_shutdown_operations_keep_installation_and_report_restart_required() {
    for recovering in [false, true] {
        let fixture = Fixture::new(true);
        let manager = fixture
            .rejecting_manager_inner(None, true)
            .await
            .with_marketplace(
                fixture.directory.join("plugins.db"),
                None,
                crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
            )
            .unwrap();
        let id = &fixture.installation.instance_id;
        let result = if recovering {
            manager.recover(id, 1).await.map(|_| ())
        } else {
            manager.uninstall(id, 1).await
        };
        assert!(matches!(result, Err(ManagementError::Lifecycle)));
        let view = manager.view(id).await.unwrap();
        assert_eq!(view.runtime_state, "disabled");
        assert!(view.restart_required);
        assert!(fixture.store.installation(id).unwrap().is_some());
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn operation_cas_conflict_after_stop_keeps_the_restart_boundary_visible() {
    for recovering in [false, true] {
        let fixture = Fixture::new(true);
        let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let manager = fixture
            .rejecting_manager_inner(Some(pause.clone()), true)
            .await
            .with_marketplace(
                fixture.directory.join("plugins.db"),
                None,
                crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.to_owned(),
            )
            .unwrap();
        let id = &fixture.installation.instance_id;
        let removal = async {
            if recovering {
                manager.recover(id, 1).await.map(|_| ())
            } else {
                manager.uninstall(id, 1).await
            }
        };
        tokio::pin!(removal);
        assert!(futures_util::poll!(&mut removal).is_pending());
        pause.0.notified().await;
        let mut edited = fixture.installation.clone();
        edited.updated_at_ms = 2;
        fixture.store.write_installation(&edited, Some(1)).unwrap();
        pause.1.notify_one();
        assert!(matches!(removal.await, Err(ManagementError::Conflict)));
        let view = manager.view(id).await.unwrap();
        assert_eq!(view.runtime_state, "disabled");
        assert!(view.restart_required);
        assert!(fixture.store.installation(id).unwrap().is_some());
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn initialization_failure_keeps_previous_runtime_config_and_saved_draft() {
    let fixture = Fixture::new(true);
    let manager = fixture.rejecting_manager(None).await;
    let id = &fixture.installation.instance_id;
    manager
        .save_config(
            id,
            1,
            BTreeMap::from([("greeting".to_owned(), json!("reject"))]),
        )
        .await
        .unwrap();
    let saved = manager.view(id).await.unwrap();
    assert!(matches!(
        manager.apply(id, saved.updated_at_ms).await,
        Err(ManagementError::Lifecycle)
    ));
    let view = manager.view(id).await.unwrap();
    assert!(view.pending);
    assert_eq!(view.runtime_state, "ready");
    assert!(runtime_config(&manager).is_empty());
    assert_eq!(view.config["greeting"], json!("reject"));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn package_changes_and_missing_grants_cannot_be_applied() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let id = &fixture.installation.instance_id;
    let mut record = fixture.installation.clone();
    record.granted_permissions.clear();
    fixture.store.upsert_installation(&record).unwrap();
    assert!(matches!(
        manager.save_enabled(id, 1, true).await,
        Err(ManagementError::MissingGrants)
    ));
    fixture
        .store
        .upsert_installation(&fixture.installation)
        .unwrap();
    fs::write(&fixture.installation.package_path, b"changed").unwrap();
    assert!(matches!(
        manager.save_config(id, 1, BTreeMap::new()).await,
        Err(ManagementError::InvalidPackage)
    ));
    assert!(manager.view(id).await.unwrap().error.is_some());
    assert_eq!(
        fixture
            .store
            .installation(id)
            .unwrap()
            .unwrap()
            .updated_at_ms,
        1
    );
}

#[tokio::test]
async fn disabled_installation_cannot_override_a_file_managed_instance() {
    let fixture = Fixture::new(false);
    let manager = fixture.rejecting_manager(None).await;
    let id = &fixture.installation.instance_id;
    assert!(!manager.view(id).await.unwrap().editable);
    assert!(matches!(
        manager.save_config(id, 1, BTreeMap::new()).await,
        Err(ManagementError::NotManaged)
    ));
    assert!(matches!(
        manager.apply(id, 1).await,
        Err(ManagementError::NotManaged)
    ));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnecting_a_request_does_not_cancel_an_in_progress_lifecycle_change() {
    let fixture = Fixture::new(true);
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let manager = fixture.rejecting_manager(Some(pause.clone())).await;
    let id = fixture.installation.instance_id.clone();
    let config = BTreeMap::from([("greeting".to_owned(), json!("wait"))]);
    manager.save_config(&id, 1, config.clone()).await.unwrap();
    let saved = manager.view(&id).await.unwrap();
    let request_manager = manager.clone();
    let request_id = id.clone();
    let request = tokio::spawn(async move {
        request_manager
            .apply(&request_id, saved.updated_at_ms)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.0.notified())
        .await
        .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    pause.1.notify_one();
    manager.shutdown().await.unwrap();
    let snapshots = manager
        .snapshot()
        .unwrap()
        .host
        .instance_snapshots()
        .unwrap();
    assert_eq!(snapshots[0].config, config);
    assert_eq!(snapshots[0].state, PluginInstanceState::Stopped);
    assert!(matches!(
        manager.apply(&id, saved.updated_at_ms).await,
        Err(ManagementError::Busy)
    ));
}

#[tokio::test]
async fn disabled_declarative_instance_cannot_be_loaded_from_a_colliding_database_record() {
    let fixture = Fixture::new(false);
    let declaration = fixture.directory.join("plugins.toml");
    let id = &fixture.installation.instance_id;
    fs::write(
        &declaration,
        format!(
            "bundled = []\n[[wasm]]\npackage = 'unused'\ninstance_id = '{id}'\nenabled = false\n"
        ),
    )
    .unwrap();
    let mut host = StaticPluginHost::new(fixture.store.clone());
    let loaded = crate::plugins::load_plugins(&mut host, &fixture.store, Some(&declaration))
        .await
        .unwrap();
    assert!(host.instance_snapshots().unwrap().is_empty());
    let manager = PluginManager::new(
        host,
        fixture.store.clone(),
        loaded.packages,
        loaded.file_instances,
    );
    assert!(!manager.view(id).await.unwrap().editable);
    assert!(!manager.list().await.unwrap()[0].editable);
    assert!(matches!(
        manager.save_enabled(id, 1, true).await,
        Err(ManagementError::NotManaged)
    ));
    assert!(matches!(
        manager.save_config(id, 1, BTreeMap::new()).await,
        Err(ManagementError::NotManaged)
    ));
    assert!(matches!(
        manager.apply(id, 1).await,
        Err(ManagementError::NotManaged)
    ));
    assert_eq!(
        fixture.store.installation(id).unwrap().unwrap(),
        fixture.installation
    );
    manager.shutdown().await.unwrap();
    let mut enabled = fixture.installation.clone();
    enabled.enabled = true;
    fixture.store.upsert_installation(&enabled).unwrap();
    let mut restarted = StaticPluginHost::new(fixture.store.clone());
    let error = crate::plugins::load_plugins(&mut restarted, &fixture.store, Some(&declaration))
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<crate::plugins::PluginConfigError>(),
        Some(crate::plugins::PluginConfigError::DuplicateInstance(_))
    ));
}

#[tokio::test]
async fn shutdown_drains_first_registration_before_capturing_the_host_registry() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let id = &fixture.installation.instance_id;
    manager.save_enabled(id, 1, true).await.unwrap();
    let revision = manager.view(id).await.unwrap().updated_at_ms;
    let write = manager.writes.lock().await;
    let apply = manager.apply(id, revision);
    tokio::pin!(apply);
    assert!(futures_util::poll!(&mut apply).is_pending());
    let shutdown = manager.shutdown();
    tokio::pin!(shutdown);
    assert!(futures_util::poll!(&mut shutdown).is_pending());
    assert!(
        manager
            .snapshot()
            .unwrap()
            .host
            .instance_snapshots()
            .unwrap()
            .is_empty()
    );
    drop(write);
    tokio::time::timeout(std::time::Duration::from_secs(5), apply)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap();
    let snapshots = manager
        .snapshot()
        .unwrap()
        .host
        .instance_snapshots()
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].state, PluginInstanceState::Stopped);
}

#[tokio::test]
async fn cancelled_shutdown_keeps_apply_and_host_shutdown_under_supervision() {
    let fixture = Fixture::new(true);
    let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let manager = fixture.rejecting_manager(Some(pause.clone())).await;
    let id = fixture.installation.instance_id.clone();
    let config = BTreeMap::from([("greeting".to_owned(), json!("wait"))]);
    manager.save_config(&id, 1, config.clone()).await.unwrap();
    let revision = manager.view(&id).await.unwrap().updated_at_ms;
    let request_manager = manager.clone();
    let request = tokio::spawn(async move { request_manager.apply(&id, revision).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.0.notified())
        .await
        .unwrap();
    {
        let shutdown = manager.shutdown();
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
    }
    assert!(!request.is_finished());
    pause.1.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), manager.shutdown())
        .await
        .unwrap()
        .unwrap();
    request.await.unwrap().unwrap();
    let snapshots = manager
        .snapshot()
        .unwrap()
        .host
        .instance_snapshots()
        .unwrap();
    assert_eq!(snapshots[0].state, PluginInstanceState::Stopped);
    assert_eq!(snapshots[0].config, config);
}

#[tokio::test]
async fn startup_provenance_uses_the_loaded_package_even_if_the_installation_record_changes() {
    use std::io::{Cursor, Write as _};
    use zip::{ZipWriter, write::SimpleFileOptions};
    let fixture = Fixture::new(true);
    let mut host = StaticPluginHost::new(fixture.store.clone());
    host.register(
        checked_plugin(&fixture.installation).await.unwrap(),
        &fixture.installation.instance_id,
        BTreeMap::new(),
        fixture
            .installation
            .granted_permissions
            .iter()
            .cloned()
            .collect(),
    )
    .await
    .unwrap();
    let loaded = BTreeMap::from([(
        fixture.installation.instance_id.clone(),
        fixture.installation.package_sha256.clone(),
    )]);
    let package = checked_package(&fixture.installation).await.unwrap();
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in package.files() {
        archive
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        archive.write_all(bytes).unwrap();
    }
    archive
        .start_file("extra.txt", SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"replacement package").unwrap();
    let bytes = archive.finish().unwrap().into_inner();
    let replacement = plugin_host::ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let mut record = fixture.installation.clone();
    record.package_path = fixture
        .directory
        .join("replacement.bkm-plugin")
        .to_str()
        .unwrap()
        .to_owned();
    fs::write(&record.package_path, bytes).unwrap();
    record.package_sha256 = replacement.package_sha256().to_owned();
    record.enabled = false;
    fixture.store.upsert_installation(&record).unwrap();
    let manager = PluginManager::new(host, fixture.store.clone(), loaded, BTreeSet::new());
    let view = manager.view(&record.instance_id).await.unwrap();
    assert!(view.editable);
    assert!(
        view.restart_required,
        "same version with different package is pending"
    );
    assert_eq!(view.saved_version.as_deref(), Some(record.version.as_str()));
    assert_eq!(
        view.loaded_version.as_deref(),
        Some(record.version.as_str())
    );
    record.version = "9.9.9".to_owned();
    record.enabled = true;
    fixture.store.upsert_installation(&record).unwrap();
    let view = manager.view(&record.instance_id).await.unwrap();
    assert_eq!(view.saved_version.as_deref(), Some("9.9.9"));
    assert_eq!(
        view.loaded_version,
        Some(fixture.installation.version.clone())
    );
    assert!(matches!(
        manager.apply(&record.instance_id, 1).await,
        Err(ManagementError::RestartRequired)
    ));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn version_status_uses_host_identity_and_explicit_restart_boundaries() {
    let fixture = Fixture::new(true);
    let manager = fixture.manager().await;
    let id = &fixture.installation.instance_id;
    let view = manager.view(id).await.unwrap();
    assert_eq!(
        view.loaded_version,
        Some(fixture.installation.version.clone())
    );
    assert!(!view.restart_required);
    manager
        .runtime
        .write()
        .unwrap()
        .restart_required
        .insert(id.clone());
    assert!(manager.view(id).await.unwrap().restart_required);
    manager.shutdown().await.unwrap();
    // A fresh process establishes a new package identity and clears the marker.
    let restarted = fixture.manager().await;
    assert!(!restarted.view(id).await.unwrap().restart_required);
    restarted.shutdown().await.unwrap();

    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let id = &fixture.installation.instance_id;
    manager
        .runtime
        .write()
        .unwrap()
        .restart_required
        .insert(id.clone());
    let view = manager.view(id).await.unwrap();
    assert_eq!(view.runtime_state, "not_loaded");
    assert_eq!(view.loaded_version, None);
    assert_eq!(
        view.saved_version,
        Some(fixture.installation.version.clone())
    );
    assert!(view.restart_required);
}

#[tokio::test]
async fn a_single_plugin_view_does_not_process_unrelated_installations() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let mut unrelated = fixture.installation.clone();
    unrelated.instance_id = "unrelated".to_owned();
    unrelated.metadata.locales.clear();
    fixture.store.upsert_installation(&unrelated).unwrap();
    let view = manager
        .view(&fixture.installation.instance_id)
        .await
        .unwrap();
    assert_eq!(view.instance_id, fixture.installation.instance_id);
    assert!(view.schema.is_some());
    let views = manager.list().await.unwrap();
    assert_eq!(views.len(), 2);
    let unrelated = views
        .iter()
        .find(|view| view.instance_id == "unrelated")
        .unwrap();
    assert_eq!(unrelated.name, unrelated.plugin_id);
    assert!(unrelated.error.as_ref().unwrap().contains("元数据"));
}
