//! Loopback-only runtime health, metrics, and configuration-check server.

use std::{net::SocketAddr, sync::Arc, thread, time::Duration};

use axum::{
    Json, Router,
    body::Bytes,
    error_handling::HandleErrorLayer,
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bot_core::{HealthStatus, RuntimeObserver, ShutdownSignal};
use serde_json::json;
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tower::{
    BoxError, ServiceBuilder, limit::ConcurrencyLimitLayer, load_shed::LoadShedLayer,
    timeout::TimeoutLayer,
};
use tracing::{info, warn};

use crate::config::{BotConfig, ManagementConfig};

const MAX_CONFIG_CHECK_BYTES: usize = 1024 * 1024;
const MAX_CONCURRENT_CONFIG_CHECKS: usize = 4;
const CONFIG_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub(crate) enum ManagementError {
    #[error("management listen address `{0}` is invalid")]
    InvalidListen(String),
    #[error("failed to bind management listener at {listen}: {source}")]
    Bind {
        listen: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("management server failed: {0}")]
    Serve(#[from] std::io::Error),
    #[error("management validation supervisor failed")]
    ValidationSupervisor(#[source] tokio::task::JoinError),
    #[error("management server graceful shutdown timed out")]
    ShutdownTimeout,
}

#[derive(Clone)]
struct ManagementState {
    observer: RuntimeObserver,
    validation_slots: Arc<Semaphore>,
    validation_jobs: mpsc::Sender<ValidationJob>,
}

struct ValidationJob {
    body: Bytes,
    response: oneshot::Sender<Result<Result<(), crate::config::ConfigError>, ()>>,
    permit: OwnedSemaphorePermit,
}

pub(crate) async fn serve(
    config: ManagementConfig,
    observer: RuntimeObserver,
    mut shutdown: ShutdownSignal,
    shutdown_timeout: Duration,
) -> Result<(), ManagementError> {
    if !config.enabled {
        shutdown.cancelled().await;
        return Ok(());
    }
    let listen = config
        .listen
        .parse::<SocketAddr>()
        .map_err(|_| ManagementError::InvalidListen(config.listen.clone()))?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|source| ManagementError::Bind { listen, source })?;
    info!(listen = %listen, "management server listening");
    let (app, validation_jobs) = router_parts(observer);
    let mut validation_supervisor = tokio::spawn(run_validation_workers(validation_jobs));
    let (serve_result, shutdown_deadline) = {
        let mut graceful_shutdown = shutdown.clone();
        let server = async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { graceful_shutdown.cancelled().await })
                .await
        };
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => (result.map_err(ManagementError::Serve), None),
            () = shutdown.cancelled() => {
                let deadline = tokio::time::Instant::now() + shutdown_timeout;
                let result = match tokio::time::timeout(
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                    &mut server,
                ).await {
                    Ok(result) => result.map_err(ManagementError::Serve),
                    Err(_) => Err(ManagementError::ShutdownTimeout),
                };
                (result, Some(deadline))
            }
        }
    };
    let supervisor_grace = shutdown_deadline.map_or(shutdown_timeout, |deadline| {
        deadline.saturating_duration_since(tokio::time::Instant::now())
    });
    let supervisor_result =
        match tokio::time::timeout(supervisor_grace, &mut validation_supervisor).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ManagementError::ValidationSupervisor(error)),
            Err(_) => {
                warn!("management validation jobs exceeded the shutdown grace period");
                // Validation runs on detached, concurrency-bounded system
                // threads rather than Tokio's blocking pool, so aborting this
                // supervisor cannot hold the async runtime open indefinitely.
                validation_supervisor.abort();
                let _ = validation_supervisor.await;
                Err(ManagementError::ShutdownTimeout)
            }
        };
    if let Err(error) = supervisor_result {
        if serve_result.is_ok() {
            return Err(error);
        }
        warn!(error = %error, "management validation supervisor also failed");
    }
    serve_result
}

#[cfg(test)]
fn router(observer: RuntimeObserver) -> Router {
    let (router, validation_jobs) = router_parts(observer);
    tokio::spawn(run_validation_workers(validation_jobs));
    router
}

fn router_parts(observer: RuntimeObserver) -> (Router, mpsc::Receiver<ValidationJob>) {
    let (validation_jobs, validation_receiver) = mpsc::channel(MAX_CONCURRENT_CONFIG_CHECKS);
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/metrics", get(metrics))
        .route(
            "/config/check",
            post(config_check).layer(
                ServiceBuilder::new()
                    .layer(HandleErrorLayer::new(config_check_service_error))
                    .layer(LoadShedLayer::new())
                    .layer(TimeoutLayer::new(CONFIG_CHECK_TIMEOUT))
                    .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_CONFIG_CHECKS)),
            ),
        )
        .layer(DefaultBodyLimit::max(MAX_CONFIG_CHECK_BYTES))
        .with_state(ManagementState {
            observer,
            validation_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CONFIG_CHECKS)),
            validation_jobs,
        });
    (router, validation_receiver)
}

async fn run_validation_workers(mut jobs: mpsc::Receiver<ValidationJob>) {
    let mut tasks = JoinSet::new();
    loop {
        if tasks.len() >= MAX_CONCURRENT_CONFIG_CHECKS {
            if let Some(joined) = tasks.join_next().await
                && let Err(error) = joined
            {
                warn!(error = %error, "configuration validation worker failed");
            }
            continue;
        }
        tokio::select! {
            job = jobs.recv() => {
                let Some(job) = job else {
                    break;
                };
                tasks.spawn(async move {
                    let ValidationJob { body, response, permit } = job;
                    let (finished, checked) = oneshot::channel();
                    let spawned = thread::Builder::new()
                        .name("bkm-config-check".to_owned())
                        .spawn(move || {
                        let _permit = permit;
                        let result = std::str::from_utf8(&body).map_or_else(
                            |_| {
                                Err(crate::config::ConfigError::InvalidValue(
                                    "configuration request body must be UTF-8 TOML".to_owned(),
                                ))
                            },
                            |source| BotConfig::check_source(source).map(|_| ()),
                        );
                        let _ = finished.send(result);
                    });
                    if let Err(error) = spawned {
                        warn!(error = %error, "failed to start configuration validation worker");
                        let _ = response.send(Err(()));
                        return;
                    }
                    let checked = checked.await.map_err(|_| ());
                    let _ = response.send(checked);
                });
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    warn!(error = %error, "configuration validation worker failed");
                }
            }
        }
    }
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined {
            warn!(error = %error, "configuration validation worker failed");
        }
    }
}

async fn config_check_service_error(error: BoxError) -> Response {
    let (status, message) = if error.is::<tower::load_shed::error::Overloaded>() {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "configuration validation is busy",
        )
    } else if error.is::<tower::timeout::error::Elapsed>() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration validation timed out",
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "configuration validation service failed",
        )
    };
    (status, Json(json!({ "valid": false, "error": message }))).into_response()
}

async fn health(State(state): State<ManagementState>) -> impl IntoResponse {
    let health = state.observer.health();
    let status = if health.status == HealthStatus::Healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(health))
}

async fn metrics(State(state): State<ManagementState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.observer.render_prometheus(),
    )
}

async fn config_check(State(state): State<ManagementState>, body: Bytes) -> Response {
    if body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "valid": false,
                "error": "configuration request body must contain UTF-8 TOML"
            })),
        )
            .into_response();
    }
    let Ok(validation_slot) = state.validation_slots.clone().try_acquire_owned() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "valid": false,
                "error": "configuration validation is busy"
            })),
        )
            .into_response();
    };
    let (response, checked) = oneshot::channel();
    if state
        .validation_jobs
        .try_send(ValidationJob {
            body,
            response,
            permit: validation_slot,
        })
        .is_err()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "valid": false,
                "error": "configuration validation service unavailable"
            })),
        )
            .into_response();
    }
    match checked.await {
        Ok(Ok(Ok(()))) => (
            StatusCode::OK,
            Json(json!({
                "valid": true,
                "restart_required": true
            })),
        )
            .into_response(),
        Ok(Ok(Err(error))) => {
            warn!(error = %error.redacted_message(), "configuration validation failed");
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "valid": false,
                    "error": "configuration validation failed"
                })),
            )
                .into_response()
        }
        Ok(Err(())) | Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "valid": false,
                "error": "configuration validation task failed"
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use bot_core::RuntimeObserver;
    use serde_json::Value;
    use tower::ServiceExt as _;

    use super::{MAX_CONFIG_CHECK_BYTES, config_check_service_error, router};

    #[tokio::test]
    async fn management_router_exposes_health_and_metrics() {
        let observer = RuntimeObserver::new();
        let response = router(observer.clone())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let response = router(observer)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; version=0.0.4; charset=utf-8"
        );
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("bkm_runtime_queue_depth")
        );
    }

    #[tokio::test]
    async fn config_check_validates_proposed_toml_without_applying_it() {
        let valid = "[qq]\nenabled = false\n[onebot11]\nenabled = true\n";
        let response = router(RuntimeObserver::new())
            .oneshot(
                Request::post("/config/check")
                    .body(Body::from(valid))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["valid"], true);
        assert_eq!(body["restart_required"], true);

        let invalid = "[qq]\nenabled = false\n[onebot11]\nenabled = false\n";
        let response = router(RuntimeObserver::new())
            .oneshot(
                Request::post("/config/check")
                    .body(Body::from(invalid))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "configuration validation failed");
        assert!(!body.to_string().contains("at least one Adapter"));
    }

    #[tokio::test]
    async fn config_check_rejects_oversized_request_body() {
        let response = router(RuntimeObserver::new())
            .oneshot(
                Request::post("/config/check")
                    .body(Body::from(vec![b'x'; MAX_CONFIG_CHECK_BYTES + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn config_check_requires_a_proposed_configuration_body() {
        let response = router(RuntimeObserver::new())
            .oneshot(Request::post("/config/check").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn config_check_overload_and_timeout_errors_are_safe_json() {
        for (error, expected_status, expected_message) in [
            (
                Box::new(tower::load_shed::error::Overloaded::new()) as tower::BoxError,
                StatusCode::TOO_MANY_REQUESTS,
                "configuration validation is busy",
            ),
            (
                Box::new(tower::timeout::error::Elapsed::new()) as tower::BoxError,
                StatusCode::SERVICE_UNAVAILABLE,
                "configuration validation timed out",
            ),
        ] {
            let response = config_check_service_error(error).await;
            assert_eq!(response.status(), expected_status);
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["valid"], false);
            assert_eq!(body["error"], expected_message);
        }
    }
}
