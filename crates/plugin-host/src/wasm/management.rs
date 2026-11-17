//! Detached management calls never run lifecycle hooks or inherit chat identity.
use super::{HostQueries, StoreData, WasmInstance, WasmPlugin, exports, state_op};
use plugin_api::{ManagementError, ManagementErrorCode, ManagementRequest, ManagementResponse};

fn internal(_: impl std::fmt::Display) -> ManagementError {
    ManagementError::new(
        ManagementErrorCode::Internal,
        "plugin management invocation failed",
    )
}

impl WasmPlugin {
    pub(super) async fn invoke_management(
        &self,
        request: &ManagementRequest,
        queries: &dyn HostQueries,
    ) -> Result<ManagementResponse, ManagementError> {
        request.validate()?;
        // Every store on this engine shares its epoch. Keep its deadline timer
        // exclusive with business calls and other detached management calls.
        let _execution = self.lock_instance().await;
        let invocation = async {
            let mut instance =
                Self::instantiate(&self.engine, &self.component, &self.manifest, &self.assets)
                    .await
                    .map_err(internal)?;
            let epoch = self
                .prepare_invocation(&mut instance.store)
                .map_err(internal)?;
            let data = instance.store.data_mut();
            data.config = queries.config_snapshot();
            data.state = queries
                .state_scan("", usize::MAX)
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.clone()))
                .collect();
            data.granted_capabilities
                .clone_from(queries.granted_capabilities());
            data.invocation_time_ms = queries.invocation_time_ms();
            // Caller authority and command context deliberately retain fresh defaults.
            let WasmInstance { store, bindings } = &mut instance;
            let hook = bindings.management.as_ref().ok_or_else(|| {
                ManagementError::new(
                    ManagementErrorCode::Unsupported,
                    "plugin management is unsupported",
                )
            })?;
            let request =
                StoreData::encoded_json(&serde_json::to_value(request).map_err(internal)?);
            let output = hook.call_invoke(store, &request).await.map_err(internal)?;
            epoch.cancel().await;
            let output = output.map_err(|error| {
                use exports::bkm::plugin::management::ErrorCode;
                ManagementError::new(
                    match error.code {
                        ErrorCode::InvalidArgument => ManagementErrorCode::InvalidArgument,
                        ErrorCode::NotFound => ManagementErrorCode::NotFound,
                        ErrorCode::Conflict => ManagementErrorCode::Conflict,
                        ErrorCode::Unsupported => ManagementErrorCode::Unsupported,
                        ErrorCode::Internal => ManagementErrorCode::Internal,
                    },
                    error.message,
                )
            })?;
            if output.data.data.len() > plugin_api::MAX_MANAGEMENT_RESPONSE_BYTES
                || output.data.schema_version != "1.0"
                || output.data.content_type != "application/json"
            {
                return Err(internal("invalid encoded management data"));
            }
            Ok(ManagementResponse {
                data: serde_json::from_slice(&output.data.data).map_err(internal)?,
                state_ops: output.state_ops.into_iter().map(state_op).collect(),
            })
        };
        let response = tokio::time::timeout(self.timeout, invocation)
            .await
            .map_err(internal)??;
        response.validate_for(request).map_err(internal)?;
        Ok(response)
    }
}
