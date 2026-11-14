//! Uses the same authentication, source checks and limits as other plugin writes.
use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::plugins::{ApiError, ApiState};
use crate::plugin_management::{
    ManagementError,
    permissions::{PermissionRequest, PermissionView},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PermissionQuery {
    instance_id: String,
}

pub(super) async fn get(
    State(state): State<ApiState>,
    query: Result<Query<PermissionQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<PermissionView>, ApiError> {
    let Query(query) = query.map_err(|_| ManagementError::InvalidOperation)?;
    if query.instance_id.is_empty() || query.instance_id.len() > 512 {
        return Err(ManagementError::InvalidOperation.into());
    }
    Ok(Json(state.manager.permissions(&query.instance_id).await?))
}

pub(super) async fn save(
    State(state): State<ApiState>,
    Json(request): Json<PermissionRequest>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"plugin": state.manager.save_permissions(request).await?}),
    ))
}

#[cfg(test)]
#[path = "permissions_tests.rs"]
mod tests;
