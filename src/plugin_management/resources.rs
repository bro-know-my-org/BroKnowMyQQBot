//! Authenticated data resources use detached guests and the live instance execution gate.
use super::{ManagementError, PluginManager};
use crate::plugin_configuration::{checked_package, checked_plugin};
use plugin_api::{
    HostQueries, ManagementDescriptor, ManagementErrorCode, ManagementOperation, ManagementRequest,
    StateValue,
};
use plugin_host::{PluginInstallation, invoke_management};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
pub(crate) struct ResourceDescriptor {
    instance_id: String,
    plugin_updated_at_ms: i64,
    revision: String,
    descriptor: Option<ManagementDescriptor>,
    editable: bool,
    unavailable_reason: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct ResourceResult {
    instance_id: String,
    plugin_updated_at_ms: i64,
    revision: String,
    data: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceWrite {
    pub instance_id: String,
    pub resource: String,
    pub operation: ManagementOperation,
    pub record_id: Option<String>,
    pub input: Option<Value>,
    pub expected_revision: String,
    pub expected_updated_at_ms: i64,
    #[serde(default)]
    pub confirm: bool,
}

fn valid_id(id: &str) -> Result<(), ManagementError> {
    if id.is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(ManagementError::InvalidOperation);
    }
    Ok(())
}

fn revision(value: &str) -> Result<u64, ManagementError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| ManagementError::InvalidOperation)?;
    if parsed.to_string() != value || parsed > i64::MAX as u64 {
        return Err(ManagementError::InvalidOperation);
    }
    Ok(parsed)
}

impl PluginManager {
    async fn resource_installation(&self, id: &str) -> Result<PluginInstallation, ManagementError> {
        valid_id(id)?;
        let store = self.store.clone();
        let name = id.to_owned();
        let saved = tokio::task::spawn_blocking(move || store.installation(&name))
            .await
            .map_err(|_| ManagementError::Store)??
            .ok_or(ManagementError::NotManaged)?;
        self.current(id, saved.updated_at_ms).await
    }

    fn resource_package_matches(&self, item: &PluginInstallation) -> Result<bool, ManagementError> {
        let runtime = self.snapshot()?;
        Ok(!runtime.restart_required.contains(&item.instance_id)
            && runtime
                .host
                .instance_manifest(&item.instance_id)
                .is_none_or(|_| {
                    runtime.packages.get(&item.instance_id) == Some(&item.package_sha256)
                }))
    }

    pub(crate) async fn resource_descriptor(
        &self,
        id: &str,
    ) -> Result<ResourceDescriptor, ManagementError> {
        valid_id(id)?;
        let view = self.view(id).await?;
        if !view.editable {
            return Ok(ResourceDescriptor {
                instance_id: id.to_owned(),
                plugin_updated_at_ms: view.updated_at_ms,
                revision: "0".to_owned(),
                descriptor: None,
                editable: false,
                unavailable_reason: None,
            });
        }
        let installation = self.resource_installation(id).await?;
        let package = checked_package(&installation).await?;
        let store = self.store.clone();
        let ids = vec![id.to_owned()];
        let revisions = tokio::task::spawn_blocking(move || store.management_revisions(&ids))
            .await
            .map_err(|_| ManagementError::Store)??;
        let available = self.resource_package_matches(&installation)?
            && (package.manifest().state_version == 1
                || self
                    .snapshot()?
                    .host
                    .instance_manifest(id)
                    .is_some_and(|manifest| {
                        manifest.state_version == package.manifest().state_version
                    }));
        Ok(ResourceDescriptor {
            instance_id: id.to_owned(),
            plugin_updated_at_ms: installation.updated_at_ms,
            revision: revisions[id].clone(),
            descriptor: package.management_schema().cloned(),
            editable: available,
            unavailable_reason: (!available).then(|| ManagementError::RestartRequired.to_string()),
        })
    }

    pub(crate) async fn resource_list(
        &self,
        id: String,
        request: ManagementRequest,
    ) -> Result<ResourceResult, ManagementError> {
        if request.operation != ManagementOperation::List {
            return Err(ManagementError::InvalidOperation);
        }
        let manager = self.clone();
        self.operation(async move { manager.resource_invoke(id, request, None).await })
            .await
    }

    pub(crate) async fn resource_write(
        &self,
        write: ResourceWrite,
    ) -> Result<ResourceResult, ManagementError> {
        if write.operation == ManagementOperation::List
            || (write.operation == ManagementOperation::Delete && !write.confirm)
        {
            return Err(ManagementError::InvalidOperation);
        }
        let expected = revision(&write.expected_revision)?;
        let request = ManagementRequest {
            resource: write.resource,
            operation: write.operation,
            search: String::new(),
            cursor: None,
            limit: 20,
            record_id: write.record_id,
            input: write.input,
        };
        let manager = self.clone();
        self.operation(async move {
            manager
                .resource_invoke(
                    write.instance_id,
                    request,
                    Some((expected, write.expected_updated_at_ms)),
                )
                .await
        })
        .await
    }

    async fn resource_invoke(
        &self,
        id: String,
        request: ManagementRequest,
        expected: Option<(u64, i64)>,
    ) -> Result<ResourceResult, ManagementError> {
        valid_id(&id)?;
        request
            .validate()
            .map_err(|_| ManagementError::InvalidOperation)?;
        let _write = self.writes.lock().await;
        let _package_lock = self.operation_lock(&id).await?;
        let installation = self.resource_installation(&id).await?;
        if expected.is_some_and(|(_, timestamp)| timestamp != installation.updated_at_ms) {
            return Err(ManagementError::Conflict);
        }
        if !self.resource_package_matches(&installation)? {
            return Err(ManagementError::RestartRequired);
        }
        let runtime = self.snapshot()?;
        let lease = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            runtime.host.lock_management_instance(&id),
        )
        .await
        .map_err(|_| ManagementError::Busy)?
        .map_err(|_| ManagementError::Lifecycle)?;
        // Constructing this fresh guest never calls init, health, recovery or migration.
        let plugin = checked_plugin(&installation).await?;
        let store = self.store.clone();
        let name = id.clone();
        let (state_revision, state) =
            tokio::task::spawn_blocking(move || store.management_snapshot(&name))
                .await
                .map_err(|_| ManagementError::Store)??;
        if expected.is_some_and(|(expected, _)| expected != state_revision) {
            return Err(ManagementError::Conflict);
        }
        // Legacy state has no persisted schema version. Version 1 is the original KV
        // layout; higher versions require an actually loaded, matching runtime.
        if lease
            .as_ref()
            .map_or(plugin.manifest().state_version != 1, |lease| {
                lease.snapshot.manifest.state_version != plugin.manifest().state_version
            })
        {
            return Err(ManagementError::RestartRequired);
        }
        let config = lease
            .as_ref()
            .map_or(&installation.config, |lease| &lease.snapshot.config);
        let grants = BTreeSet::new(); // Management has only config/private-state queries.
        let queries = ResourceQueries {
            config,
            state: &state,
            grants: &grants,
            now_ms: chrono::Utc::now().timestamp_millis(),
        };
        let result = invoke_management(plugin.as_ref(), &request, &queries).await;
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                tracing::warn!(instance_id=id, resource=request.resource, operation=?request.operation, status="rejected", "plugin resource operation rejected");
                return Err(match error.code {
                    ManagementErrorCode::Conflict => ManagementError::Conflict,
                    ManagementErrorCode::Unsupported => ManagementError::ResourcesUnsupported,
                    ManagementErrorCode::InvalidArgument | ManagementErrorCode::NotFound => {
                        ManagementError::InvalidResourceData
                    }
                    ManagementErrorCode::Internal => ManagementError::ResourceFailed,
                });
            }
        };
        // Even reads verify that the installation and snapshot did not change while
        // the guest ran. The write transaction atomically enforces both revisions.
        let store = self.store.clone();
        let name = id.clone();
        let updated = installation.updated_at_ms;
        let quota = plugin.manifest().permissions.storage_quota_bytes;
        let operations = output.state_ops;
        let next_revision = tokio::task::spawn_blocking(move || {
            store.commit_management(&name, state_revision, Some(updated), &operations, quota)
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        tracing::info!(instance_id=id, resource=request.resource, operation=?request.operation, status="succeeded", source="webui", "plugin resource operation completed");
        Ok(ResourceResult {
            instance_id: id,
            plugin_updated_at_ms: updated,
            revision: next_revision.to_string(),
            data: output.data,
        })
    }
}

struct ResourceQueries<'a> {
    config: &'a BTreeMap<String, Value>,
    state: &'a BTreeMap<String, StateValue>,
    grants: &'a BTreeSet<String>,
    now_ms: i64,
}
impl HostQueries for ResourceQueries<'_> {
    fn config_snapshot(&self) -> BTreeMap<String, Value> {
        self.config.clone()
    }
    fn config_get(&self, key: &str) -> Option<&Value> {
        self.config.get(key)
    }
    fn state_get(&self, key: &str) -> Option<&StateValue> {
        self.state.get(key)
    }
    fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)> {
        self.state
            .range(prefix.to_owned()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .take(limit.min(4096))
            .map(|(key, value)| (key.as_str(), value))
            .collect()
    }
    fn granted_capabilities(&self) -> &BTreeSet<String> {
        self.grants
    }
    fn invocation_time_ms(&self) -> i64 {
        self.now_ms
    }
}

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;
