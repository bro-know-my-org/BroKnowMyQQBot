use super::marketplace::selection;
use super::{ManagementError, PluginManager};
use crate::plugin_installer::{
    PendingInstallation, finish_installation, validate_install_candidate,
};
use plugin_host::{PluginInstallation, ValidatedPluginPackage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallRequest {
    selector: String,
    selection: String,
    version: String,
    sha256: String,
    expected_updated_at_ms: Option<i64>,
    grants: Vec<String>,
    confirm_unsigned: bool,
}

#[derive(Serialize)]
pub(crate) struct InstallResult {
    instance_id: String,
    version: String,
    enabled: bool,
    restart_required: bool,
    missing_permissions: Vec<String>,
    config_incomplete: bool,
    updated_at_ms: i64,
}

impl PluginManager {
    pub(crate) async fn install_marketplace(
        &self,
        request: InstallRequest,
        update: bool,
    ) -> Result<InstallResult, ManagementError> {
        if !request.confirm_unsigned
            || request.selector.len() > 128
            || request.version.len() > 128
            || request.sha256.len() != 64
            || request.selection.len() != 64
            || request.grants.len() > 128
            || request.grants.iter().any(|s| s.len() > 256)
        {
            return Err(ManagementError::InvalidMarketRequest);
        }
        crate::plugin_marketplace::validate_selector(&request.selector)
            .map_err(|_| ManagementError::InvalidMarketRequest)?;
        let manager = self.clone();
        self.operation(async move { manager.install_marketplace_inner(request, update).await })
            .await
    }

    async fn install_marketplace_inner(
        &self,
        request: InstallRequest,
        update: bool,
    ) -> Result<InstallResult, ManagementError> {
        // Keep the slot until blocking validation and persistence finish. Network
        // requests have individual deadlines; HTTP cancellation drops only the receiver.
        let (pending, existing) = self.prepare_marketplace(request, update).await?;
        let _write = self.writes.lock().await;
        let context = self.marketplace_context()?.clone();
        let store = self.store.clone();
        let id = pending.instance_id.clone();
        let missing_permissions = pending
            .requested
            .difference(&pending.granted)
            .cloned()
            .collect();
        let config_incomplete = pending.config_error.is_some();
        let installation = tokio::task::spawn_blocking(move || {
            if crate::plugins::is_declared_instance_id(
                context.declarations.as_deref(),
                &pending.instance_id,
            )
            .map_err(|_| ManagementError::NotManaged)?
            {
                return Err(ManagementError::NotManaged);
            }
            finish_installation(&store, &context.database, existing.as_ref(), &pending).map_err(
                |error| {
                    if let Some(error) = error.downcast_ref::<plugin_host::StoreError>() {
                        if matches!(error, plugin_host::StoreError::InstallationConflict) {
                            ManagementError::Conflict
                        } else {
                            ManagementError::Store
                        }
                    } else if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
                    {
                        ManagementError::Busy
                    } else {
                        ManagementError::Store
                    }
                },
            )
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        self.runtime
            .write()
            .map_err(|_| ManagementError::Store)?
            .restart_required
            .insert(id.clone());
        Ok(InstallResult {
            instance_id: id,
            version: installation.version,
            enabled: installation.enabled,
            restart_required: true,
            missing_permissions,
            config_incomplete,
            updated_at_ms: installation.updated_at_ms,
        })
    }

    async fn prepare_marketplace(
        &self,
        request: InstallRequest,
        update: bool,
    ) -> Result<(PendingInstallation, Option<PluginInstallation>), ManagementError> {
        let context = self.marketplace_context()?;
        let index = context
            .client
            .fetch_index()
            .await
            .map_err(|_| ManagementError::MarketplaceUnavailable)?;
        let plugin = index
            .find(&request.selector)
            .map_err(|_| ManagementError::StaleMarketplace)?
            .clone();
        if selection(&plugin)? != request.selection
            || plugin.latest.version != request.version
            || plugin.latest.sha256 != request.sha256
        {
            return Err(ManagementError::StaleMarketplace);
        }
        let (instance_id, existing) = self.marketplace_existing(&plugin, &request, update).await?;
        let bytes = context
            .client
            .download(&plugin)
            .await
            .map_err(|_| ManagementError::InvalidPackage)?;
        let (bytes, package) = tokio::task::spawn_blocking(move || {
            let package = ValidatedPluginPackage::from_bytes(&bytes)
                .map_err(|_| ManagementError::InvalidPackage)?;
            Ok::<_, ManagementError>((bytes, package))
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        plugin
            .validate_package(&package)
            .map_err(|_| ManagementError::InvalidPackage)?;
        let manifest = package.manifest().clone();
        let requested = manifest.requested_capabilities();
        let mut granted: BTreeSet<_> = existing
            .as_ref()
            .map(|i| {
                i.granted_permissions
                    .iter()
                    .filter(|p| requested.contains(*p))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if request
            .grants
            .iter()
            .any(|grant| !requested.contains(grant))
        {
            return Err(ManagementError::InvalidMarketRequest);
        }
        granted.extend(request.grants);
        let config = existing
            .as_ref()
            .map(|i| i.config.clone())
            .unwrap_or_default();
        let config_error = validate_install_candidate(package, &config)
            .await
            .map_err(|_| ManagementError::InvalidPackage)?;
        let latest = context
            .client
            .fetch_index()
            .await
            .map_err(|_| ManagementError::MarketplaceUnavailable)?;
        if latest
            .find(&request.selector)
            .ok()
            .and_then(|p| selection(p).ok())
            .as_deref()
            != Some(request.selection.as_str())
        {
            return Err(ManagementError::StaleMarketplace);
        }
        Ok((
            PendingInstallation {
                manifest,
                instance_id,
                package_sha256: plugin.latest.sha256,
                package_bytes: bytes,
                source: plugin.latest.download,
                permissions_complete: requested.is_subset(&granted),
                requested,
                granted,
                config,
                config_error,
                update,
            },
            existing,
        ))
    }
    async fn marketplace_existing(
        &self,
        plugin: &crate::plugin_marketplace::MarketplacePlugin,
        request: &InstallRequest,
        update: bool,
    ) -> Result<(String, Option<PluginInstallation>), ManagementError> {
        let instance_id = format!("{}/default", plugin.plugin_id);
        let runtime = self.snapshot()?;
        if crate::plugins::is_reserved_bundled_instance_id(&instance_id)
            || self.file_instances.contains(&instance_id)
            || (runtime.host.instance_manifest(&instance_id).is_some()
                && !runtime.packages.contains_key(&instance_id))
        {
            return Err(ManagementError::NotManaged);
        }
        let store = self.store.clone();
        let id = instance_id.clone();
        let existing = tokio::task::spawn_blocking(move || store.installation(&id))
            .await
            .map_err(|_| ManagementError::Store)??;
        if existing.as_ref().map(|i| i.updated_at_ms) != request.expected_updated_at_ms
            || update != existing.is_some()
        {
            return Err(ManagementError::Conflict);
        }
        if existing
            .as_ref()
            .is_some_and(|i| i.plugin_id != plugin.plugin_id.as_str())
        {
            return Err(ManagementError::NotManaged);
        }
        Ok((instance_id, existing))
    }
}
