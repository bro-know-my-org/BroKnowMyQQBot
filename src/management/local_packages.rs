//! Separate raw upload route: authentication precedes its dedicated hard body bound.
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::json;

use super::{
    auth::{self, AuthState},
    plugins::ApiError,
};
use crate::plugin_management::{ManagementError, PluginManager, local_packages::MAX_UPLOAD_BYTES};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectionRequest {
    selection: String,
}

pub(super) async fn discard(
    State(state): State<super::plugins::ApiState>,
    Json(request): Json<SelectionRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if uuid::Uuid::parse_str(&request.selection).is_err() {
        return Err(ManagementError::InvalidOperation.into());
    }
    state.manager.discard_local_selection(&request.selection)?;
    Ok(Json(json!({"discarded":true})))
}

pub(super) fn router(manager: PluginManager, auth: AuthState) -> Router {
    Router::new()
        .route("/api/plugins/local/inspect", post(inspect))
        .with_state(manager)
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate))
}

async fn inspect(State(manager): State<PluginManager>, request: Request) -> Response {
    let permit = match manager.reserve_upload() {
        Ok(permit) => permit,
        Err(error) => return ApiError::from(error).into_response(),
    };
    if request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        != Some("application/octet-stream")
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error":"请上传二进制 .bkm-plugin 文件"})),
        )
            .into_response();
    }
    // Do not trust Content-Length and do not pass this through generic 1 MiB JSON middleware.
    let bytes =
        match tokio::time::timeout(Duration::from_secs(30), read_upload(request.into_body())).await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err((status, error))) => {
                return (status, Json(json!({"error":error}))).into_response();
            }
            Err(_) => {
                return (
                    StatusCode::REQUEST_TIMEOUT,
                    Json(json!({"error":"上传超时，请重新选择文件"})),
                )
                    .into_response();
            }
        };
    match manager.inspect_local_package(bytes, permit).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => ApiError::from(error).into_response(),
    }
}

async fn read_upload(body: axum::body::Body) -> Result<Vec<u8>, (StatusCode, &'static str)> {
    let mut stream = body.into_data_stream();
    // Fixed allocation after acquiring a permit: irregular chunks cannot make
    // Vec's geometric growth retain more than the per-transfer memory budget.
    let mut bytes = Vec::with_capacity(MAX_UPLOAD_BYTES);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "上传中断或无法读取，请重新选择文件",
            )
        })?;
        if chunk.len() > MAX_UPLOAD_BYTES - bytes.len() {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "插件包不得超过 32 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "local_packages_tests.rs"]
mod tests;
