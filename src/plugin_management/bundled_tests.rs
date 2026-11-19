use super::*;
use plugin_host::{PluginStore, StaticPluginHost};
use serde_json::json;
use std::{collections::BTreeSet, path::PathBuf};

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
}
impl Fixture {
    fn new(source: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("bkm-bundled-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("plugins.toml");
        fs::write(&path, source).unwrap();
        Self { directory, path }
    }
    async fn manager(&self) -> PluginManager {
        let store = PluginStore::in_memory().unwrap();
        let mut host = StaticPluginHost::new(store.clone());
        let loaded = crate::plugins::load_plugins(&mut host, &store, Some(&self.path))
            .await
            .unwrap();
        PluginManager::new(host, store, loaded.packages, loaded.file_instances)
            .with_marketplace(
                self.directory.join("db"),
                Some(self.path.clone()),
                "https://bkmqb-plugins.github.io/marketplace/index.json".into(),
            )
            .unwrap()
    }
    fn change(&self, name: &str, values: Value) -> Change {
        let Value::Object(values) = values else {
            panic!("expected object")
        };
        let mut body = json!({"name":name,"expected_revision":config::config_revision(&fs::read(&self.path).unwrap()),"confirm":true});
        body.as_object_mut().unwrap().extend(values);
        serde_json::from_value(body).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn catalogue_includes_unselected_plugins_and_compiled_feature_boundary() {
    let fixture = Fixture::new("bundled = ['ping']\n");
    let manager = fixture.manager().await;
    let catalogue = manager.bundled_catalogue().await.unwrap();
    assert_eq!(
        catalogue.plugins.len(),
        if cfg!(feature = "coverage-probes") {
            13
        } else {
            7
        }
    );
    let ping = catalogue.plugins.iter().find(|p| p.key == "ping").unwrap();
    assert!(ping.loaded && ping.enabled && !ping.pending);
    let admin = catalogue.plugins.iter().find(|p| p.key == "admin").unwrap();
    assert!(!admin.loaded && !admin.enabled && !admin.pending);
    assert!(
        admin.schema.as_ref().unwrap()["required"]
            .as_array()
            .unwrap()
            .contains(&json!("owners"))
    );
}

#[tokio::test]
async fn save_preserves_other_declarations_and_only_changes_state_after_restart() {
    let source = "# preserve comment\nbundled = ['ping']\n\n[[wasm]]\npackage = 'do-not-open'\ninstance_id = 'dev.example.disabled/one'\nenabled = false # preserved\n\n[bundled_config.admin]\nowners = ['owner']\n";
    let fixture = Fixture::new(source);
    let manager = fixture.manager().await;
    let saved = manager
        .save_bundled(fixture.change("echo", json!({"enabled":true})))
        .await
        .unwrap();
    let echo = saved.plugins.iter().find(|p| p.key == "echo").unwrap();
    assert!(echo.enabled && !echo.loaded && echo.restart_required);
    let updated = fs::read_to_string(&fixture.path).unwrap();
    assert!(updated.contains("# preserve comment") && updated.contains("false # preserved"));
    assert!(updated.contains("owners = ['owner']") && updated.contains("package = 'do-not-open'"));
    assert_eq!(
        manager
            .snapshot()
            .unwrap()
            .host
            .instance_snapshots()
            .unwrap()
            .len(),
        1
    );
    let restarted = fixture.manager().await;
    let current = restarted.bundled_catalogue().await.unwrap();
    let echo = current.plugins.iter().find(|p| p.key == "echo").unwrap();
    assert!(echo.loaded && echo.enabled && !echo.pending);
    let disabled = restarted
        .save_bundled(fixture.change("echo", json!({"enabled":false})))
        .await
        .unwrap();
    let echo = disabled.plugins.iter().find(|p| p.key == "echo").unwrap();
    assert!(echo.loaded && !echo.enabled && echo.pending);
    assert_eq!(echo.runtime_state, "ready");
}

#[tokio::test]
async fn config_replace_removes_deleted_fields_and_help_uses_saved_config() {
    let fixture = Fixture::new(
        "bundled = ['help']\n[bundled_config.help]\nremove_me = true\nkeep = 'value'\n",
    );
    let manager = fixture.manager().await;
    let saved = manager
        .save_bundled(fixture.change("help", json!({"config":{"keep":"new"}})))
        .await
        .unwrap();
    let help = saved.plugins.iter().find(|p| p.key == "help").unwrap();
    assert_eq!(help.config, BTreeMap::from([("keep".into(), json!("new"))]));
    assert!(help.pending);
    let restarted = fixture.manager().await;
    assert!(
        !restarted
            .bundled_catalogue()
            .await
            .unwrap()
            .plugins
            .iter()
            .find(|p| p.key == "help")
            .unwrap()
            .pending
    );
    assert!(
        !fs::read_to_string(&fixture.path)
            .unwrap()
            .contains("remove_me")
    );
}

#[tokio::test]
async fn invalid_owner_config_unknown_selection_and_stale_revision_never_write() {
    let fixture = Fixture::new("bundled = ['ping']\n");
    let manager = fixture.manager().await;
    let before = fs::read(&fixture.path).unwrap();
    for change in [
        fixture.change("admin", json!({"enabled":true})),
        fixture.change("ping", json!({"enabled":false,"confirm":false})),
        fixture.change("unknown", json!({"enabled":true})),
    ] {
        assert!(manager.save_bundled(change).await.is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), before);
    }
    let stale = fixture.change("ping", json!({"enabled":false}));
    manager
        .save_bundled(fixture.change(
            "admin",
            json!({"enabled":true,"config":{"owners":["owner"]}}),
        ))
        .await
        .unwrap();
    let updated = fs::read(&fixture.path).unwrap();
    assert!(matches!(
        manager.save_bundled(stale).await,
        Err(ManagementError::Conflict)
    ));
    assert_eq!(fs::read(&fixture.path).unwrap(), updated);
}

#[tokio::test]
async fn saved_namespace_rejects_conflict_with_managed_wasm_without_initializing_it() {
    let external = crate::plugin_configuration::fixture::Fixture::new(true);
    let fixture = Fixture::new("bundled = []\n");
    let manager = PluginManager::new(
        StaticPluginHost::new(external.store.clone()),
        external.store.clone(),
        BTreeMap::new(),
        BTreeSet::new(),
    )
    .with_marketplace(
        fixture.directory.join("db"),
        Some(fixture.path.clone()),
        "https://bkmqb-plugins.github.io/marketplace/index.json".into(),
    )
    .unwrap();
    assert!(matches!(
        manager
            .save_bundled(fixture.change("ping", json!({"enabled":true})))
            .await,
        Err(ManagementError::InvalidConfig)
    ));
    assert_eq!(fs::read_to_string(&fixture.path).unwrap(), "bundled = []\n");
    assert!(
        manager
            .snapshot()
            .unwrap()
            .host
            .instance_snapshots()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_and_readonly_directory_fail_without_reporting_a_save() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let fixture = Fixture::new("bundled = ['ping']\n");
    let manager = fixture.manager().await;
    let change = fixture.change("ping", json!({"enabled":false}));
    let target = fixture.directory.join("actual.toml");
    fs::rename(&fixture.path, &target).unwrap();
    symlink(&target, &fixture.path).unwrap();
    assert!(manager.bundled_catalogue().await.is_err());
    assert!(manager.save_bundled(change).await.is_err());
    fs::remove_file(&fixture.path).unwrap();
    fs::rename(target, &fixture.path).unwrap();
    if rustix::process::geteuid().is_root() {
        return;
    }
    let change = fixture.change("ping", json!({"enabled":false}));
    fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(0o500)).unwrap();
    let result = manager.save_bundled(change).await;
    fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(&fixture.path).unwrap(),
        "bundled = ['ping']\n"
    );
}

#[cfg(feature = "coverage-probes")]
#[tokio::test]
async fn diagnostic_selection_requires_admin_even_if_plugin_is_currently_ready() {
    let fixture = Fixture::new(
        "bundled = ['admin', 'devtools']\n[bundled_config.admin]\nowners = ['owner']\n",
    );
    let manager = fixture.manager().await;
    assert!(matches!(
        manager
            .save_bundled(fixture.change("admin", json!({"enabled":false})))
            .await,
        Err(ManagementError::InvalidConfig)
    ));
}

#[cfg(unix)]
#[test]
fn replaced_nonregular_descriptor_is_rejected_without_reading() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let fixture = Fixture::new("bundled = []\n");
    let fifo = fixture.directory.join("replacement");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    // Model replacement after the pathname check: opening must never await a writer.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    assert!(read_opened(file).is_err());
    assert!(read_source(&fifo).is_err());
}
