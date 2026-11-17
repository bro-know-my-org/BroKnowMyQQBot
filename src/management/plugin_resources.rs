//! Resource APIs share the plugin router's authentication, origin and body limits.
use super::plugins::{ApiError, ApiState};
use crate::plugin_management::{
    ManagementError,
    resources::{ResourceDescriptor, ResourceResult, ResourceWrite},
};
use axum::{
    Json,
    extract::{Query, State},
};
use plugin_api::{ManagementOperation, ManagementRequest};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Instance {
    instance_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResourceQuery {
    instance_id: String,
    resource: String,
    #[serde(default)]
    search: String,
    cursor: Option<String>,
    limit: Option<u32>,
}
pub(super) async fn descriptor(
    State(state): State<ApiState>,
    Query(query): Query<Instance>,
) -> Result<Json<ResourceDescriptor>, ApiError> {
    Ok(Json(
        state
            .manager
            .resource_descriptor(&query.instance_id)
            .await?,
    ))
}
pub(super) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<ResourceQuery>,
) -> Result<Json<ResourceResult>, ApiError> {
    let request = ManagementRequest {
        resource: query.resource,
        operation: ManagementOperation::List,
        search: query.search,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(20),
        record_id: None,
        input: None,
    };
    request
        .validate()
        .map_err(|_| ManagementError::InvalidOperation)?;
    Ok(Json(
        state
            .manager
            .resource_list(query.instance_id, request)
            .await?,
    ))
}
pub(super) async fn write(
    State(state): State<ApiState>,
    Json(request): Json<ResourceWrite>,
) -> Result<Json<ResourceResult>, ApiError> {
    Ok(Json(state.manager.resource_write(request).await?))
}
