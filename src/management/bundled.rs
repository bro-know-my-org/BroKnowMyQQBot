use super::plugins::{ApiError, ApiState};
use crate::plugin_management::bundled::{Catalogue, Change};
use axum::{Json, extract::State};

pub(super) async fn get(State(state): State<ApiState>) -> Result<Json<Catalogue>, ApiError> {
    Ok(Json(state.manager.bundled_catalogue().await?))
}

pub(super) async fn save(
    State(state): State<ApiState>,
    Json(change): Json<Change>,
) -> Result<Json<Catalogue>, ApiError> {
    Ok(Json(state.manager.save_bundled(change).await?))
}
