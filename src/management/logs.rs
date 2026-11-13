//! Cursor-based access to bounded logs from this process.

use super::plugins::ApiState;
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct LogQuery {
    after: Option<u64>,
    limit: Option<usize>,
    level: Option<String>,
    instance_id: Option<String>,
}

pub(super) async fn get(State(state): State<ApiState>, Query(query): Query<LogQuery>) -> Response {
    let limit = query.limit.unwrap_or(200);
    if !(1..=200).contains(&limit)
        || query
            .level
            .as_deref()
            .is_some_and(|level| !matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR"))
        || query
            .instance_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"日志筛选参数无效"})),
        )
            .into_response();
    }
    Json(state.logs.snapshot(
        query.after,
        limit,
        query.level.as_deref(),
        query.instance_id.as_deref(),
    ))
    .into_response()
}
