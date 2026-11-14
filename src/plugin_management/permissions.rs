//! Manifest constrained grants. Revocation drains and irreversibly retires live authority.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{ManagementError, PluginManager, PluginView};
use crate::plugin_configuration::checked_package;

#[derive(Serialize)]
pub(crate) struct PermissionView {
    instance_id: String,
    expected_updated_at_ms: i64,
    version: String,
    sha256: String,
    requested_permissions: BTreeSet<String>,
    granted_permissions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PermissionRequest {
    pub(crate) instance_id: String,
    pub(crate) expected_updated_at_ms: i64,
    pub(crate) grants: BTreeSet<String>,
    pub(crate) confirm: bool,
}

impl PluginManager {
    pub(crate) async fn permissions(&self, id: &str) -> Result<PermissionView, ManagementError> {
        let _write = self.writes.lock().await;
        let _lock = self.operation_lock(id).await?;
        let view = self.view(id).await?;
        let item = self.current(id, view.updated_at_ms).await?;
        let package = checked_package(&item).await?;
        Ok(PermissionView {
            instance_id: id.to_owned(),
            expected_updated_at_ms: item.updated_at_ms,
            version: item.version,
            sha256: item.package_sha256,
            requested_permissions: package.manifest().requested_capabilities(),
            granted_permissions: item.granted_permissions,
        })
    }

    pub(crate) async fn save_permissions(
        &self,
        request: PermissionRequest,
    ) -> Result<PluginView, ManagementError> {
        if !request.confirm
            || request.instance_id.is_empty()
            || request.instance_id.len() > 512
            || request.expected_updated_at_ms < 0
        {
            return Err(ManagementError::InvalidOperation);
        }
        let manager = self.clone();
        self.operation(async move { manager.save_permissions_inner(request).await })
            .await
    }

    async fn save_permissions_inner(
        &self,
        request: PermissionRequest,
    ) -> Result<PluginView, ManagementError> {
        let _write = self.writes.lock().await;
        let id = &request.instance_id;
        let lock = self.operation_lock(id).await?;
        let mut item = self.current(id, request.expected_updated_at_ms).await?;
        let package = checked_package(&item).await?;
        let requested = package.manifest().requested_capabilities();
        if !request.grants.is_subset(&requested) {
            return Err(ManagementError::InvalidOperation);
        }
        let runtime = self.snapshot()?;
        let live = runtime
            .host
            .instance_snapshots()
            .map_err(|_| ManagementError::Store)?;
        let revoke_live = live
            .iter()
            .any(|s| s.instance_id == *id && !s.granted_capabilities.is_subset(&request.grants));
        // Set the boundary before awaiting retirement: failure and later CAS conflict
        // must still truthfully expose a retired/disabled guest requiring restart.
        self.runtime
            .write()
            .map_err(|_| ManagementError::Store)?
            .restart_required
            .insert(id.clone());
        if revoke_live {
            runtime
                .host
                .retire_instance(id)
                .await
                .map_err(|_| ManagementError::Lifecycle)?;
        }
        item.enabled &= requested.is_subset(&request.grants);
        item.requested_permissions = requested.into_iter().collect();
        item.granted_permissions = request.grants.into_iter().collect();
        item.updated_at_ms = chrono::Utc::now().timestamp_millis().max(
            request
                .expected_updated_at_ms
                .checked_add(1)
                .ok_or(ManagementError::Conflict)?,
        );
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            store.write_installation(&item, Some(request.expected_updated_at_ms))
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        self.view(id).await
    }
}

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod tests;
