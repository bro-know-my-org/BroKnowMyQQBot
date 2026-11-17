//! Serialize isolated data management with live invocations, without lifecycle hooks.
use super::{PluginHostError, PluginInstanceSnapshot, PluginInstanceState, StaticPluginHost};
use futures_util::FutureExt;
use plugin_api::{
    HostQueries, ManagementError, ManagementErrorCode, ManagementRequest, ManagementResponse,
    StaticPlugin,
};
use std::{panic::AssertUnwindSafe, sync::Arc};
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit};

/// Retained across snapshot, isolated handler execution and atomic state commit.
/// Dropping this lease never changes state or schedules and never calls a guest.
#[derive(Debug)]
pub struct PluginManagementLease {
    pub snapshot: PluginInstanceSnapshot,
    _execution: OwnedSemaphorePermit,
    _lifecycle: OwnedMutexGuard<()>,
}

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;

impl StaticPluginHost {
    pub async fn lock_management_instance(
        &self,
        id: &str,
    ) -> Result<Option<PluginManagementLease>, PluginHostError> {
        let Some(plugin) = self.plugins.iter().find(|plugin| plugin.instance_id == id) else {
            return Ok(None);
        };
        let lifecycle = Arc::clone(&plugin.lifecycle).lock_owned().await;
        let execution = plugin.execution.acquire_exclusive().await?;
        let state = plugin.state()?;
        if !matches!(
            state,
            PluginInstanceState::Ready
                | PluginInstanceState::Stopped
                | PluginInstanceState::Disabled
        ) {
            return Err(PluginHostError::LifecycleChanged {
                plugin_id: plugin.manifest.id.to_string(),
            });
        }
        Ok(Some(PluginManagementLease {
            snapshot: PluginInstanceSnapshot {
                instance_id: plugin.instance_id.clone(),
                manifest: plugin.manifest.clone(),
                state,
                config: plugin.config_snapshot()?,
                schema: None,
                granted_capabilities: plugin.granted_capabilities.clone(),
            },
            _execution: execution,
            _lifecycle: lifecycle,
        }))
    }
}

/// Invoke only the optional management export. No init, migration, event or action path.
pub async fn invoke_management(
    plugin: &dyn StaticPlugin,
    request: &ManagementRequest,
    queries: &dyn HostQueries,
) -> Result<ManagementResponse, ManagementError> {
    request.validate()?;
    let internal = || {
        ManagementError::new(
            ManagementErrorCode::Internal,
            "plugin management invocation failed",
        )
    };
    let (descriptor, deadline) = std::panic::catch_unwind(AssertUnwindSafe(|| {
        (
            plugin.management_schema().cloned(),
            plugin.manifest().timeout(),
        )
    }))
    .map_err(|_| internal())?;
    let descriptor = descriptor.ok_or_else(|| {
        ManagementError::new(
            ManagementErrorCode::Unsupported,
            "plugin has no management resources",
        )
    })?;
    descriptor.validate().map_err(|_| internal())?;
    let resource = descriptor
        .resources
        .iter()
        .find(|r| r.id == request.resource)
        .ok_or_else(|| {
            ManagementError::new(ManagementErrorCode::NotFound, "unknown management resource")
        })?;
    if !resource.operations.contains(&request.operation) {
        return Err(ManagementError::new(
            ManagementErrorCode::Unsupported,
            "unsupported management operation",
        ));
    }
    let schema = jsonschema::validator_for(&resource.schema).map_err(|_| internal())?;
    if matches!(
        request.operation,
        plugin_api::ManagementOperation::Create | plugin_api::ManagementOperation::Update
    ) && !request
        .input
        .as_ref()
        .is_some_and(|input| schema.is_valid(input))
    {
        return Err(ManagementError::new(
            ManagementErrorCode::InvalidArgument,
            "resource input does not match schema",
        ));
    }
    let call = AssertUnwindSafe(async { plugin.manage(request, queries).await }).catch_unwind();
    let response = tokio::time::timeout(deadline, call)
        .await
        .map_err(|_| internal())?
        .map_err(|_| internal())??;
    response.validate_for(request).map_err(|_| internal())?;
    if response.data["records"]
        .as_array()
        .is_none_or(|records| records.len() > request.limit as usize)
    {
        return Err(internal());
    }
    Ok(response)
}
