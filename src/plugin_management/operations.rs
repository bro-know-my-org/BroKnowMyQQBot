//! Explicit, supervised operations; HTTP cancellation never cancels a write.
use std::path::Path;

use plugin_host::FailedDeliveryPage;
use serde::Serialize;

use super::{ManagementError, PluginManager, PluginView};
use crate::plugin_installer::{
    cleanup_unreferenced_managed_package, lock_managed_package_lifecycle,
};

#[derive(Serialize)]
pub(crate) struct RecoveryResult {
    plugin: PluginView,
    recovered: u64,
    restart_required: bool,
}

impl PluginManager {
    // Called only after supervised Host shutdown has drained all live guests.
    pub(super) async fn cleanup_retained_packages(&self) -> Result<(), ManagementError> {
        let paths = self.snapshot()?.retained_packages;
        if paths.is_empty() {
            return Ok(());
        }
        let database = self.marketplace_context()?.database.clone();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _lock =
                lock_managed_package_lifecycle(&database).map_err(|_| ManagementError::Store)?;
            for path in paths {
                cleanup_unreferenced_managed_package(&store, &database, &path)
                    .map_err(|_| ManagementError::Store)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| ManagementError::Store)?
    }

    pub(super) async fn operation_lock(&self, id: &str) -> Result<std::fs::File, ManagementError> {
        let context = self.marketplace_context()?.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let lock = lock_managed_package_lifecycle(&context.database).map_err(|error| {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    ManagementError::Busy
                } else {
                    ManagementError::Store
                }
            })?;
            if crate::plugins::is_declared_instance_id(context.declarations.as_deref(), &id)
                .map_err(|_| ManagementError::NotManaged)?
            {
                return Err(ManagementError::NotManaged);
            }
            Ok(lock)
        })
        .await
        .map_err(|_| ManagementError::Store)?
    }

    pub(crate) async fn uninstall(&self, id: &str, expected: i64) -> Result<(), ManagementError> {
        let manager = self.clone();
        let id = id.to_owned();
        self.operation(async move { manager.uninstall_inner(&id, expected).await })
            .await
    }

    async fn uninstall_inner(&self, id: &str, expected: i64) -> Result<(), ManagementError> {
        let _write = self.writes.lock().await;
        let lock = self.operation_lock(id).await?;
        let installation = self.current(id, expected).await?;
        let mut runtime = self.snapshot()?;
        if runtime.host.instance_manifest(id).is_some() {
            let retired = runtime.host.unregister_instance(id).await;
            // Detachment is irreversible even if a concurrent CLI edit makes
            // the subsequent database CAS fail. Keep that boundary visible.
            self.runtime
                .write()
                .map_err(|_| ManagementError::Store)?
                .restart_required
                .insert(id.to_owned());
            retired.map_err(|_| ManagementError::Lifecycle)?;
        }
        let protected = runtime
            .packages
            .iter()
            .any(|(other, hash)| other != id && hash == &installation.package_sha256);
        if protected {
            runtime
                .retained_packages
                .insert(installation.package_path.clone().into());
        }
        let store = self.store.clone();
        let database = self.marketplace_context()?.database.clone();
        let retained = tokio::task::spawn_blocking(move || {
            // Closing this File releases the package lifecycle lock. Clippy 1.85
            // incorrectly treats moving the guard into this scope as no effect.
            #[allow(clippy::no_effect_underscore_binding)]
            let _lock = lock;
            if !store.remove_installation_if_updated(&installation.instance_id, expected)? {
                return Err(ManagementError::Conflict);
            }
            if !protected {
                // Cleanup is best effort; a failed cleanup leaves a harmless package,
                // never an installation whose old guest can still receive events.
                if cleanup_unreferenced_managed_package(
                    &store,
                    &database,
                    Path::new(&installation.package_path),
                )
                .is_err()
                {
                    tracing::warn!("uninstalled plugin package retained for later cleanup");
                    return Ok(Some(installation.package_path));
                }
            }
            Ok(None)
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        if let Some(path) = retained {
            runtime.retained_packages.insert(path.into());
        }
        runtime.packages.remove(id);
        runtime.restart_required.remove(id);
        *self.runtime.write().map_err(|_| ManagementError::Store)? = runtime;
        Ok(())
    }

    pub(crate) async fn recover(
        &self,
        id: &str,
        expected: i64,
    ) -> Result<RecoveryResult, ManagementError> {
        let manager = self.clone();
        let id = id.to_owned();
        self.operation(async move { manager.recover_inner(&id, expected).await })
            .await
    }

    async fn recover_inner(
        &self,
        id: &str,
        expected: i64,
    ) -> Result<RecoveryResult, ManagementError> {
        let _write = self.writes.lock().await;
        let lock = self.operation_lock(id).await?;
        self.current(id, expected).await?;
        let runtime = self.snapshot()?;
        let retired = if runtime.host.instance_manifest(id).is_some() {
            runtime.host.retire_instance(id).await
        } else {
            Ok(())
        };
        self.runtime
            .write()
            .map_err(|_| ManagementError::Store)?
            .restart_required
            .insert(id.to_owned());
        retired.map_err(|_| ManagementError::Lifecycle)?;
        let store = self.store.clone();
        let instance_id = id.to_owned();
        let recovered = tokio::task::spawn_blocking(move || {
            let result = store.recover_dead_letters_if_updated(
                &instance_id,
                expected,
                chrono::Utc::now().timestamp_millis(),
            );
            drop(lock);
            result
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        Ok(RecoveryResult {
            plugin: self.view(id).await?,
            recovered,
            restart_required: true,
        })
    }

    pub(crate) async fn failed_deliveries(
        &self,
        id: &str,
        before: Option<i64>,
        limit: usize,
    ) -> Result<FailedDeliveryPage, ManagementError> {
        if id.is_empty()
            || id.len() > 512
            || !(1..=100).contains(&limit)
            || before.is_some_and(|id| id <= 0)
        {
            return Err(ManagementError::InvalidOperation);
        }
        let registered = self.snapshot()?.host.instance_manifest(id).is_some();
        let store = self.store.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            if !registered && store.installation(&id)?.is_none() {
                return Err(ManagementError::NotManaged);
            }
            Ok(store.failed_delivery_page(&id, before, limit)?)
        })
        .await
        .map_err(|_| ManagementError::Store)?
    }
}
