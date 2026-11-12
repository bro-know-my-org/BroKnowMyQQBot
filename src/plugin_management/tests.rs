use std::fs;

use serde_json::json;

use super::*;
use crate::plugin_configuration::fixture::Fixture;

impl Fixture {
    async fn manager(&self) -> PluginManager {
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
