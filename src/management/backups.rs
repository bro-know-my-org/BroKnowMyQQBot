//! Authenticated artifact export. Download holds a bounded stream slot until drop.
use super::{
    auth::{self, AuthState},
    plugins::{ApiError, read_body},
};
use crate::{
    backup::Repository,
    plugin_management::{ManagementError, PluginManager},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::{io::AsyncReadExt as _, sync::Semaphore};

#[derive(Clone)]
struct Backups {
    manager: PluginManager,
    slots: Arc<Semaphore>,
    repository: Option<Repository>,
    shutdown: Option<bot_core::ShutdownSignal>,
}
pub(super) fn router(
    manager: PluginManager,
    auth: AuthState,
    shutdown: Option<bot_core::ShutdownSignal>,
) -> Router {
    let repository = manager.backup_repository().ok();
    routes(
        Backups {
            manager,
            slots: Arc::new(Semaphore::new(1)),
            repository,
            shutdown,
        },
        auth,
    )
}
fn routes(state: Backups, auth: AuthState) -> Router {
    Router::new()
        .route("/api/backups", get(list).post(create))
        .route("/api/backups/{id}", get(download))
        .with_state(state)
        .route_layer(middleware::from_fn(read_body))
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Export {
    confirm_sensitive_plugin_data: bool,
}
async fn create(
    State(state): State<Backups>,
    Json(request): Json<Export>,
) -> Result<Json<crate::backup::Artifact>, ApiError> {
    if !request.confirm_sensitive_plugin_data {
        return Err(ManagementError::InvalidOperation.into());
    }
    let permit = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ManagementError::Busy)?;
    let repository = state.repository.clone().ok_or(ManagementError::Backup)?;
    // Permit moves into the retained operation; disconnect cannot release it early.
    let artifact = state.manager.create_backup(repository, permit).await?;
    Ok(Json(artifact))
}
async fn list(
    State(state): State<Backups>,
) -> Result<Json<Vec<crate::backup::Artifact>>, ApiError> {
    let permit = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ManagementError::Busy)?;
    let repository = state.repository.clone().ok_or(ManagementError::Backup)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        repository.list()
    })
    .await
    .map_err(|_| ManagementError::Backup)?
    .map(Json)
    .map_err(Into::into)
}
async fn download(
    State(state): State<Backups>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let permit = state
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ManagementError::Busy)?;
    let repository = state.repository.clone().ok_or(ManagementError::Backup)?;
    let selected = id.clone();
    let (file, permit) =
        tokio::task::spawn_blocking(move || repository.open(&selected).map(|file| (file, permit)))
            .await
            .map_err(|_| ManagementError::Backup)??;
    let length = file.metadata().map_err(|_| ManagementError::Backup)?.len();
    let body = download_body(&state, file, permit, std::time::Duration::from_secs(300))?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zip".into()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"bkmqb-{id}.bkm-backup\""),
            ),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::CONTENT_LENGTH, length.to_string()),
        ],
        body,
    )
        .into_response())
}

fn download_body(
    state: &Backups,
    file: std::fs::File,
    permit: tokio::sync::OwnedSemaphorePermit,
    lifetime: std::time::Duration,
) -> Result<axum::body::Body, ApiError> {
    let (send, receive) = tokio::sync::mpsc::channel(1);
    let shutdown = state.shutdown.clone();
    state.manager.supervise_backup(async move {
        let _permit = permit;
        let work = async move {
            let mut file = tokio::fs::File::from_std(file).take(crate::backup::LIMIT as u64);
            loop {
                let mut bytes = vec![0; 64 * 1024];
                match file.read(&mut bytes).await {
                    Ok(0) => break,
                    Ok(count) => {
                        bytes.truncate(count);
                        if send.send(Ok(Some(bytes))).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = send.send(Err(error)).await;
                        return;
                    }
                }
            }
            drop(file);
            // Explicit EOF lets the body finish while we retain the slot until its
            // receiver drops. A stalled receiver is still bounded by the outer timer.
            if send.send(Ok(None)).await.is_ok() {
                send.closed().await;
            }
        };
        let cancelled = async move {
            match shutdown {
                Some(mut signal) => signal.cancelled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {biased; ()=cancelled=>(), _=tokio::time::timeout(lifetime,work)=>()}
    })?;
    let stream = futures_util::stream::try_unfold(receive, |mut receive| async move {
        match receive.recv().await {
            Some(Ok(Some(bytes))) => Ok(Some((bytes, receive))),
            Some(Err(error)) => Err(error),
            _ => Ok(None),
        }
    });
    Ok(axum::body::Body::from_stream(stream))
}
