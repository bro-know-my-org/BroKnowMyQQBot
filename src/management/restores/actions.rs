use super::*;
use crate::restore::offline;
use axum::http::HeaderMap;
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Confirmation {
    expected_boot_id: String,
    confirm_sensitive_plugin_data: bool,
    confirm_unsigned_disabled: bool,
}
pub(super) async fn commit(
    Extract((state, auth)): Extract<(State, AuthState)>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<Confirmation>,
) -> Result<Response, ApiError> {
    if !request.confirm_sensitive_plugin_data || !request.confirm_unsigned_disabled {
        return Err(ManagementError::InvalidOperation.into());
    }
    if request.expected_boot_id != crate::restart::boot_id() {
        return Err(ManagementError::Conflict.into());
    }
    if state
        .shutdown
        .as_ref()
        .is_none_or(bot_core::ShutdownSignal::is_shutdown)
    {
        return Err(ManagementError::Busy.into());
    }
    if !state.controller.available() {
        return Err(ManagementError::InvalidOperation.into());
    }
    let permit = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ManagementError::Busy)?;
    let selected = id.clone();
    let tickets = state.tickets.clone();
    let selection = move || {
        let mut tickets = tickets.lock().map_err(crate::backup::invalid)?;
        if !tickets.as_ref().is_some_and(|t| {
            t.expires > std::time::Instant::now()
                && t.prepared.inspection.operation_id == selected
                && t.prepared.inspection.boot_id == request.expected_boot_id
        }) {
            return Err(ManagementError::ExpiredSelection);
        }
        Ok(tickets
            .take()
            .ok_or(ManagementError::ExpiredSelection)?
            .prepared)
    };
    let repository = state.repository.clone().ok_or(ManagementError::Backup)?;
    state
        .manager
        .accept_restore(
            repository,
            selection,
            state.controller,
            auth.restart_session(&headers),
            permit,
        )
        .await?;
    Ok((StatusCode::ACCEPTED,Json(serde_json::json!({"operation_id":id,"action":"restart","boot_id":crate::restart::boot_id()}))).into_response())
}
pub(super) async fn outcome(
    Extract((state, _)): Extract<(State, AuthState)>,
    Path(id): Path<String>,
) -> Result<Json<offline::Outcome>, ApiError> {
    if uuid::Uuid::parse_str(&id).is_err() {
        return Err(ManagementError::InvalidOperation.into());
    }
    let permit = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ManagementError::Busy)?;
    let repository = state.repository.clone().ok_or(ManagementError::Backup)?;
    state
        .manager
        .restore_outcome(repository, id, permit)
        .await
        .map(Json)
        .map_err(Into::into)
}
