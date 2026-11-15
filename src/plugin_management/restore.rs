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
            let context = manager.marketplace_context()?;
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

impl PluginManager {
    pub(crate) async fn accept_restore(
        &self,
        repository: Repository,
        selection: impl FnOnce() -> Result<Prepared> + Send + 'static,
        controller: crate::restart::Controller,
        session: Option<crate::management::SessionHandoff>,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let manager = self.clone();
        self.operation(async move {
            let _permit = permit;
            let prepared = selection()?;
            let _write = manager.writes.lock().await;
            let context = manager.marketplace_context()?;
            let accepted = tokio::task::spawn_blocking(move || {
                crate::restore::offline::stage(
                    repository,
                    context.database,
                    context.declarations,
                    prepared,
                )
            })
            .await
            .map_err(invalid)??;
            if controller
                .request_restore(accepted.clone(), session)
                .is_err()
            {
                tokio::task::spawn_blocking(move || accepted.record_status("rejected"))
                    .await
                    .map_err(invalid)??;
                return Err(super::ManagementError::Conflict);
            }
            Ok(())
        })
        .await
    }
    pub(crate) async fn restore_outcome(
        &self,
        repository: Repository,
        id: String,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<crate::restore::offline::Outcome> {
        self.operation(async move {
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if !repository
                    .root
                    .join("restore")
                    .try_exists()
                    .map_err(invalid)?
                {
                    return Err(super::ManagementError::ExpiredSelection);
                }
                let files = crate::restore::files::Files::new(&repository)?;
                if !files.exists("outcome.json")? {
                    return Err(super::ManagementError::ExpiredSelection);
                }
                let outcome: crate::restore::offline::Outcome = files.read("outcome.json", 4096)?;
                if outcome.operation_id != id {
                    return Err(super::ManagementError::ExpiredSelection);
                }
                Ok(outcome)
            })
            .await
            .map_err(invalid)?
        })
        .await
    }
}
