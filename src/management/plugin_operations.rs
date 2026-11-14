//! These routes share authentication, same-origin checks and request limits.
use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::plugins::{ApiError, ApiState};
use crate::plugin_management::ManagementError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OperationRequest {
    instance_id: String,
    expected_updated_at_ms: i64,
    confirm: bool,
}

impl OperationRequest {
    fn check(&self) -> Result<(), ApiError> {
        if !self.confirm
            || self.instance_id.is_empty()
            || self.instance_id.len() > 512
            || self.expected_updated_at_ms < 0
        {
            return Err(ManagementError::InvalidOperation.into());
        }
        Ok(())
    }
}

pub(super) async fn uninstall(
    State(state): State<ApiState>,
    Json(request): Json<OperationRequest>,
) -> Result<Json<Value>, ApiError> {
    request.check()?;
    state
        .manager
        .uninstall(&request.instance_id, request.expected_updated_at_ms)
        .await?;
    Ok(Json(
        json!({"removed": true, "instance_id": request.instance_id, "private_state_retained": true}),
    ))
}

pub(super) async fn recover(
    State(state): State<ApiState>,
    Json(request): Json<OperationRequest>,
) -> Result<Json<Value>, ApiError> {
    request.check()?;
    Ok(Json(json!(
        state
            .manager
            .recover(&request.instance_id, request.expected_updated_at_ms)
            .await?
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FailureQuery {
    instance_id: String,
    before: Option<i64>,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    20
}

pub(super) async fn failures(
    State(state): State<ApiState>,
    query: Result<Query<FailureQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<plugin_host::FailedDeliveryPage>, ApiError> {
    let Query(query) = query.map_err(|_| ManagementError::InvalidOperation)?;
    Ok(Json(
        state
            .manager
            .failed_deliveries(&query.instance_id, query.before, query.limit)
            .await?,
    ))
}
