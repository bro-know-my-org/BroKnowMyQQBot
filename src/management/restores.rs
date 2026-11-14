//! Raw bounded upload has its own route; generic JSON limits remain unchanged.
use super::{
    auth::{self, AuthState},
    plugins::ApiError,
};
use crate::{plugin_management::ManagementError, restore::State};
use axum::{
    Json, Router,
    extract::{Path, Request, State as Extract},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt as _;

pub(super) fn router(state: State, auth: AuthState) -> Router {
    Router::new()
        .route("/api/restores/inspect", post(inspect))
        .route("/api/restores/{id}", get(inspection).delete(discard))
        .with_state(state)
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate))
}
async fn inspection(
    Extract(state): Extract<State>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let permit = std::sync::Arc::new(
        state
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Busy)?,
    );
    Ok(json(state.inspection(&id)?, permit))
}
async fn discard(
    Extract(state): Extract<State>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let mut ticket = state.tickets.lock().map_err(crate::backup::invalid)?;
    if ticket
        .as_ref()
        .is_some_and(|t| t.prepared.inspection.operation_id == id)
    {
        *ticket = None;
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn inspect(Extract(state): Extract<State>, request: Request) -> Response {
    let result = async {
        let permit = std::sync::Arc::new(state
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Busy)?);
        let repository = state.repository.clone().ok_or(ManagementError::Backup)?;
        if request
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            != Some("application/octet-stream")
        {
            return Err(ManagementError::InvalidOperation);
        }
        let upload = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut stream = request.into_body().into_data_stream();
            let mut bytes = Vec::with_capacity(crate::backup::LIMIT);
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| ManagementError::InvalidOperation)?;
                if chunk.len() > crate::backup::LIMIT - bytes.len() {
                    return Err(ManagementError::RestoreTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        });
        let cancelled=async {match state.shutdown.clone() {Some(mut signal)=>signal.cancelled().await,None=>std::future::pending().await}};
        let bytes=tokio::select! {biased; ()=cancelled=>return Err(ManagementError::Busy), bytes=upload=>bytes.map_err(|_|ManagementError::InvalidOperation)??};
        let inspection=state
                .manager
                .inspect_restore(repository, bytes, permit.clone());
        let cancelled=async {match state.shutdown.clone() {Some(mut signal)=>signal.cancelled().await,None=>std::future::pending().await}};
        let prepared=tokio::select! {biased; ()=cancelled=>return Err(ManagementError::Busy), prepared=inspection=>prepared?};
        let plan=state.retain(prepared)?;
        Ok((plan,permit))
    }
    .await;
    match result {
        Ok((plan, permit)) => json(plan, permit),
        Err(error) => ApiError::from(error).into_response(),
    }
}
fn json(
    value: impl serde::Serialize,
    permit: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Response {
    let (parts, body) = Json(value).into_response().into_parts();
    Response::from_parts(
        parts,
        axum::body::Body::from_stream(PermitBody {
            body: Box::pin(body.into_data_stream()),
            _permit: permit,
        }),
    )
}
struct PermitBody {
    body: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>,
    >,
    _permit: std::sync::Arc<tokio::sync::OwnedSemaphorePermit>,
}
impl futures_util::Stream for PermitBody {
    type Item = Result<axum::body::Bytes, axum::Error>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.body.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests;
