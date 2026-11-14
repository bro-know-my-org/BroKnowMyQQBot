use super::PluginManager;
use crate::{
    backup::{Repository, Result, invalid},
    config, plugin_installer,
    restore::plan::Prepared,
};

impl PluginManager {
    pub(crate) async fn inspect_restore(
        &self,
        repository: Repository,
        bytes: Vec<u8>,
        permit: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<Prepared> {
        let manager = self.clone();
        self.operation(async move {
            let _permit = permit;
            let _write = manager.writes.lock().await;
            let context = manager.marketplace_context()?.clone();
            let runtime = manager
                .snapshot()?
                .host
                .instance_snapshots()
                .map_err(invalid)?;
            let store = manager.store.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let _config =
                    config::ConfigFileLock::acquire(&repository.config).map_err(invalid)?;
                let lifecycle = plugin_installer::lock_managed_package_lifecycle(&context.database)
                    .map_err(invalid)?;
                let result = (|| {
                    let archive =
                        plugin_host::LogicalArchive::from_bytes(&bytes).map_err(invalid)?;
                    drop(bytes);
                    let source =
                        String::from_utf8(crate::backup::read(&repository.config, 1024 * 1024)?)
                            .map_err(invalid)?;
                    let expected = store.logical_snapshot().map_err(invalid)?.installations;
                    let revision =
                        crate::restore::declaration_revision(context.declarations.as_deref())?;
                    let declarations =
                        crate::plugins::backup_declarations(context.declarations.as_deref())
                            .map_err(invalid)?;
                    if crate::restore::declaration_revision(context.declarations.as_deref())?
                        != revision
                    {
                        return Err(super::ManagementError::Conflict);
                    }
                    let mut prepared =
                        Prepared::new(archive, source, expected, declarations, &runtime)?;
                    prepared.declaration_revision = revision;
                    Ok(prepared)
                })();
                let _ = fs2::FileExt::unlock(&lifecycle);
                result
            })
            .await
            .map_err(invalid)??;
            prepared.validate_guests().await?;
            Ok(prepared)
        })
        .await
    }
}
