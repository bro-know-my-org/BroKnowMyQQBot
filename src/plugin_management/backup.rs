use super::PluginManager;
use crate::{
    backup::{self, Artifact, Repository, Result, invalid},
    config, plugin_installer,
};
use plugin_host::LogicalArchive;
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

impl PluginManager {
    pub(crate) fn supervise_backup(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let mut operations = self.operations.lock().map_err(invalid)?;
        if operations.closing {
            return Err(super::ManagementError::Busy);
        }
        while let Some(result) = operations.tasks.try_join_next() {
            result.map_err(invalid)?;
        }
        operations.tasks.spawn(task);
        Ok(())
    }
    pub(crate) fn backup_repository(&self) -> Result<Repository> {
        Ok(Repository::new(
            &self.marketplace_context()?.database,
            config::application_config_path(),
        ))
    }
    pub(crate) async fn create_backup(
        &self,
        repository: Repository,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<Artifact> {
        let manager = self.clone();
        self.operation(async move {
            let _permit = permit;
            let _write = manager.writes.lock().await;
            let runtime = manager.snapshot()?;
            let context = manager.marketplace_context()?.clone(); let store = manager.store.clone();
            tokio::task::spawn_blocking(move || {
                // Shared order: manager writes -> config -> packages -> SQLite.
                let _config = config::ConfigFileLock::acquire(&repository.config).map_err(invalid)?;
                let lifecycle = plugin_installer::lock_managed_package_lifecycle(&context.database).map_err(invalid)?;
                let result = (|| {
                    let text = String::from_utf8(backup::read(&repository.config, 1024*1024)?).map_err(invalid)?;
                    let saved: config::BotConfig = toml::from_str(&text).map_err(invalid)?;
                    let mut snapshot = store.logical_snapshot().map_err(invalid)?;
                    snapshot.installations.extend(crate::plugins::backup_declarations(context.declarations.as_deref()).map_err(invalid)?);
                    snapshot.validate().map_err(invalid)?;
                    if runtime.packages.keys().any(|id| !snapshot.installations.iter().any(|i| &i.instance_id == id)) { return Err(super::ManagementError::Backup); }
                    let bindings = runtime.host.instance_snapshots().map_err(invalid)?;
                    if snapshot.state.iter().any(|e| !snapshot.installations.iter().any(|i|i.instance_id==e.instance_id) && !bindings.iter().any(|b|b.instance_id==e.instance_id)) {return Err(super::ManagementError::UnboundBackupState);}
                    let mut configuration = BTreeMap::from([("bot".into(), backup::selected(&saved))]);
                    configuration.insert("bindings".into(), json!(bindings.iter().filter(|b| !snapshot.installations.iter().any(|i| i.instance_id == b.instance_id)).map(|b|
                        json!({"instance_id":b.instance_id,"manifest":b.manifest,"config":b.config,"grants":b.granted_capabilities})).collect::<Vec<_>>()));
                    let mut packages = BTreeMap::new();
                    let mut package_bytes = 0;
                    for item in &mut snapshot.installations {
                        if runtime.packages.get(&item.instance_id).is_some_and(|hash| hash != &item.package_sha256) { return Err(super::ManagementError::Backup); }
                        if !packages.contains_key(&item.package_sha256) {
                            let bytes = backup::read(Path::new(&item.package_path), plugin_host::MAX_PLUGIN_PACKAGE_BYTES.min(backup::LIMIT-package_bytes))?;
                            package_bytes += bytes.len(); packages.insert(item.package_sha256.clone(),bytes);
                        }
                        item.package_path = format!("packages/{}.bkm-plugin", item.package_sha256);
                        if item.source != "declaration" { item.source = "logical-export".into(); }
                    }
                    let archive = LogicalArchive::create(configuration, snapshot, packages, chrono::Utc::now().timestamp_millis()).map_err(invalid)?;
                    repository.save(&archive.to_bytes().map_err(invalid)?)
                })();
                let _ = fs2::FileExt::unlock(&lifecycle); result
            }).await.map_err(invalid)?
        }).await
    }
}
