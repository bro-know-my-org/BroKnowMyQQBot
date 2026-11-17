use super::*;
use crate::plugin_management::bundled::Change;
use plugin_host::{PluginStore, StaticPluginHost};
use serde_json::json;
use std::collections::BTreeMap;

struct Fixture {
    directory: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("bkm-bundled-init-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let config = directory.join("bot.toml");
        fs::write(&config, "# preserve root\n[qq]\nenabled = false\n[plugins]\nmarketplace_url = \"https://example.test/index.json\"\n").unwrap();
        Self { directory, config }
    }
    async fn manager(&self) -> PluginManager {
        let store = PluginStore::in_memory().unwrap();
        let mut host = StaticPluginHost::new(store.clone());
        let loaded = crate::plugins::load_plugins(&mut host, &store, None)
            .await
            .unwrap();
        PluginManager::new(host, store, loaded.packages, loaded.file_instances)
            .with_marketplace(
                self.directory.join("db"),
                None,
                "https://example.test/index.json".into(),
            )
            .unwrap()
            .with_bundled_config(self.config.clone())
    }
    async fn change(manager: &PluginManager) -> Change {
        let value = serde_json::to_value(manager.bundled_catalogue().await.unwrap()).unwrap();
        serde_json::from_value(json!({"name":"echo", "enabled":false, "confirm":true, "expected_revision":value["revision"]})).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn first_save_materializes_private_file_and_updates_all_manager_clones() {
    let fixture = Fixture::new();
    let manager = fixture.manager().await;
    let clone = manager.clone();
    let root_before = fs::read(&fixture.config).unwrap();
    let change = Fixture::change(&manager).await;
    assert_eq!(fs::read_dir(&fixture.directory).unwrap().count(), 1);
    manager.save_bundled(change).await.unwrap();
    let path = clone.marketplace_context().unwrap().declarations.unwrap();
    let saved_root = fs::read_to_string(&fixture.config).unwrap();
    assert!(
        saved_root.contains("# preserve root")
            && saved_root.contains("https://example.test/index.json")
    );
    assert_ne!(saved_root.as_bytes(), root_before);
    let config: BotConfig = toml::from_str(&saved_root).unwrap();
    assert_eq!(config.plugins.installations.as_ref(), Some(&path));
    assert!(path.is_absolute());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let catalogue = serde_json::to_value(clone.bundled_catalogue().await.unwrap()).unwrap();
    let echo = catalogue["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "echo")
        .unwrap();
    assert_eq!(echo["enabled"], false);
    assert_eq!(echo["loaded"], true);
    assert_eq!(echo["pending"], true);
    clone
        .save_bundled(Fixture::change(&clone).await)
        .await
        .unwrap();
    assert_eq!(clone.declaration_path().unwrap().as_ref(), Some(&path));
    let store = PluginStore::in_memory().unwrap();
    let mut host = StaticPluginHost::new(store.clone());
    crate::plugins::load_plugins(&mut host, &store, Some(&path))
        .await
        .unwrap();
    assert!(
        !host
            .instance_snapshots()
            .unwrap()
            .iter()
            .any(|s| s.instance_id == "dev.bkm.echo/default")
    );
}

#[tokio::test]
async fn stale_root_revision_and_other_writer_lock_do_not_create_declarations() {
    let fixture = Fixture::new();
    let manager = fixture.manager().await;
    let stale = Fixture::change(&manager).await;
    let changed = fs::read_to_string(&fixture.config).unwrap() + "# another writer\n";
    fs::write(&fixture.config, &changed).unwrap();
    assert!(matches!(
        manager.save_bundled(stale).await,
        Err(ManagementError::Conflict)
    ));
    let change = Fixture::change(&manager).await;
    let lock = ConfigFileLock::acquire(&fixture.config).unwrap();
    assert!(matches!(
        manager.save_bundled(change).await,
        Err(ManagementError::Conflict)
    ));
    drop(lock);
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), changed);
    assert!(manager.declaration_path().unwrap().is_none());
    assert!(!fs::read_dir(&fixture.directory).unwrap().any(|p| {
        p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("plugins-")
    }));
}

#[tokio::test]
async fn first_save_never_overwrites_an_existing_target_and_cleans_failed_reference() {
    let fixture = Fixture::new();
    let manager = fixture.manager().await;
    let target = manager.read_bundled_source(true).unwrap();
    let path = target.path.clone();
    fs::write(&path, "existing data").unwrap();
    assert!(target.persist(&manager, "bundled = []\n").is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "existing data");
    let mut target = manager.read_bundled_source(true).unwrap();
    target.path = fixture.directory.join("new.toml");
    let path = target.path.clone();
    fs::rename(&fixture.config, fixture.directory.join("original")).unwrap();
    fs::create_dir(&fixture.config).unwrap();
    assert!(target.persist(&manager, "bundled = []\n").is_err());
    assert!(!path.exists());
    assert!(manager.declaration_path().unwrap().is_none());
}

#[tokio::test]
async fn list_status_and_catalogue_agree_on_saved_enable_and_configuration_changes() {
    let fixture = Fixture::new();
    let manager = fixture.manager().await;
    let mut change = serde_json::to_value(manager.bundled_catalogue().await.unwrap()).unwrap();
    change = json!({"name":"help","config":{"test":"saved"},"confirm":true,"expected_revision":change["revision"]});
    manager
        .save_bundled(serde_json::from_value(change).unwrap())
        .await
        .unwrap();
    let views = manager.list().await.unwrap();
    let help = views
        .iter()
        .find(|p| p.instance_id == "dev.bkm.help/default")
        .unwrap();
    assert!(help.pending && help.restart_required);
    assert_eq!(
        help.config,
        BTreeMap::from([("test".into(), json!("saved"))])
    );
    let (rows, _, revision, _) = manager.status().await.unwrap();
    let rows = serde_json::to_value(rows).unwrap();
    let help = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["instance_id"] == "dev.bkm.help/default")
        .unwrap();
    assert_eq!(help["pending"], true);
    assert_eq!(help["restart_required"], true);
    let catalogue = serde_json::to_value(manager.bundled_catalogue().await.unwrap()).unwrap();
    assert_eq!(revision.as_deref(), catalogue["revision"].as_str());
    manager
        .save_bundled(Fixture::change(&manager).await)
        .await
        .unwrap();
    let echo = manager.view("dev.bkm.echo/default").await.unwrap();
    assert!(!echo.enabled && echo.pending && echo.runtime_state == "ready");
}
