//! Marketplace routes inherit management authentication, source checks and request slots.
use super::plugins::{ApiError, ApiState};
use crate::plugin_management::marketplace::Catalogue;
use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Browse {
    #[serde(default)]
    q: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    20
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Details {
    selector: String,
}

pub(super) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<Browse>,
) -> Result<Json<Catalogue>, ApiError> {
    Ok(Json(
        state
            .manager
            .catalogue(&query.q, query.offset, query.limit, None)
            .await?,
    ))
}
pub(super) async fn details(
    State(state): State<ApiState>,
    Query(query): Query<Details>,
) -> Result<Json<Catalogue>, ApiError> {
    Ok(Json(
        state
            .manager
            .catalogue("", 0, 1, Some(&query.selector))
            .await?,
    ))
}

#[cfg(test)]
#[path = "marketplace_tests.rs"]
mod tests;

pub(super) async fn install(
    State(state): State<ApiState>,
    Json(request): Json<crate::plugin_management::marketplace_install::InstallRequest>,
) -> Result<Json<crate::plugin_management::marketplace_install::InstallResult>, ApiError> {
    Ok(Json(
        state.manager.install_marketplace(request, false).await?,
    ))
}
pub(super) async fn update(
    State(state): State<ApiState>,
    Json(request): Json<crate::plugin_management::marketplace_install::InstallRequest>,
) -> Result<Json<crate::plugin_management::marketplace_install::InstallResult>, ApiError> {
    Ok(Json(
        state.manager.install_marketplace(request, true).await?,
    ))
}
