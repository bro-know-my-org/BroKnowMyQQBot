//! Authenticated, boot-conditional, deduplicated process control.
use super::auth::{self, AuthState};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::json;

pub(super) fn router(controller: crate::restart::Controller, auth: AuthState) -> Router {
    Router::new()
        .route("/api/restart", post(restart))
        .route("/api/stop", post(stop))
        .with_state(controller)
        .route_layer(middleware::from_fn(super::plugins::read_body))
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate))
        .layer(
            tower::ServiceBuilder::new()
                .layer(axum::error_handling::HandleErrorLayer::new(
                    super::config_check_service_error,
                ))
                .layer(tower::load_shed::LoadShedLayer::new())
                .layer(tower::limit::GlobalConcurrencyLimitLayer::new(1)),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartRequest {
    expected_boot_id: String,
}

async fn restart(
    State(controller): State<crate::restart::Controller>,
    Json(request): Json<RestartRequest>,
) -> Response {
    request_shutdown(&controller, &request, crate::restart::Action::Restart)
}
async fn stop(
    State(controller): State<crate::restart::Controller>,
    Json(request): Json<RestartRequest>,
) -> Response {
    request_shutdown(&controller, &request, crate::restart::Action::Stop)
}
fn request_shutdown(
    controller: &crate::restart::Controller,
    request: &RestartRequest,
    action: crate::restart::Action,
) -> Response {
    match controller.request(&request.expected_boot_id, action) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({"boot_id":crate::restart::boot_id(), "action":action})),
        )
            .into_response(),
        Err(error) => (
            match error {
                crate::restart::Rejection::Unavailable | crate::restart::Rejection::Signal => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                crate::restart::Rejection::Stale | crate::restart::Rejection::Conflict => {
                    StatusCode::CONFLICT
                }
            },
            Json(json!({"error":error.message()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use tower::ServiceExt as _;
    const TOKEN: &str = "restart-test-token-0123456789abcdef0123456789";

    #[tokio::test]
    async fn restart_requires_auth_and_same_origin_and_matching_boot() {
        for path in ["/api/restart", "/api/stop"] {
            let controller = crate::restart::Controller::new();
            let app = router(
                controller.clone(),
                super::super::plugins::token_digest(TOKEN).unwrap().into(),
            );
            for (token, origin, boot, status) in [
                (
                    "wrong",
                    "http://localhost",
                    crate::restart::boot_id(),
                    StatusCode::UNAUTHORIZED,
                ),
                (
                    TOKEN,
                    "http://evil.example",
                    crate::restart::boot_id(),
                    StatusCode::FORBIDDEN,
                ),
                (TOKEN, "http://localhost", "stale", StatusCode::CONFLICT),
                (
                    TOKEN,
                    "http://localhost",
                    crate::restart::boot_id(),
                    StatusCode::ACCEPTED,
                ),
                (
                    TOKEN,
                    "http://localhost",
                    crate::restart::boot_id(),
                    StatusCode::ACCEPTED,
                ),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::post(path)
                            .header(header::AUTHORIZATION, format!("Bearer {token}"))
                            .header(header::HOST, "localhost")
                            .header(header::ORIGIN, origin)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(json!({"expected_boot_id":boot}).to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = if status == StatusCode::ACCEPTED
                    && path == "/api/restart"
                    && !controller.available()
                {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    status
                };
                assert_eq!(response.status(), status);
                if status == StatusCode::ACCEPTED {
                    let data: serde_json::Value = serde_json::from_slice(
                        &to_bytes(response.into_body(), 4096).await.unwrap(),
                    )
                    .unwrap();
                    assert_eq!(data["boot_id"], crate::restart::boot_id());
                }
            }
            let response = app
                .oneshot(
                    Request::post(if path == "/api/restart" {
                        "/api/stop"
                    } else {
                        "/api/restart"
                    })
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"expected_boot_id":crate::restart::boot_id()}).to_string(),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if path == "/api/restart" && !controller.available() {
                    StatusCode::ACCEPTED
                } else {
                    StatusCode::CONFLICT
                }
            );
        }
    }
}
