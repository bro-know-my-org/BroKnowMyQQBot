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

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn http_bundled_routes_require_auth_origin_confirmation_and_revision() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt as _;
        let directory =
            std::env::temp_dir().join(format!("bkm-bundled-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("plugins.toml");
        std::fs::write(&path, "bundled = []\n").unwrap();
        let store = plugin_host::PluginStore::in_memory().unwrap();
        let manager = crate::plugin_management::PluginManager::new(
            plugin_host::StaticPluginHost::new(store.clone()),
            store,
            std::collections::BTreeMap::default(),
            std::collections::BTreeSet::default(),
        )
        .with_marketplace(
            directory.join("db"),
            Some(path.clone()),
            "https://example.test/index.json".into(),
        )
        .unwrap();
        let token = "bundled-test-token-0123456789abcdef0123456789";
        let app = super::super::plugins::router(
            manager.clone(),
            super::super::plugins::token_digest(token).unwrap(),
        );
        for method in ["GET", "POST"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/api/plugins/bundled")
                        .body(Body::from("invalid"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let before = std::fs::read(&path).unwrap();
        let revision = crate::config::config_revision(&before);
        for (body, origin, expected) in [
            (
                serde_json::json!({"name":"echo","enabled":true,"confirm":true,"expected_revision":revision}),
                "https://other.test",
                StatusCode::FORBIDDEN,
            ),
            (
                serde_json::json!({"name":"echo","enabled":true,"confirm":false,"expected_revision":revision}),
                "http://localhost",
                StatusCode::BAD_REQUEST,
            ),
            (
                serde_json::json!({"name":"admin","enabled":true,"confirm":true,"expected_revision":revision}),
                "http://localhost",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                serde_json::json!({"name":"echo","enabled":true,"confirm":true,"expected_revision":"0".repeat(64)}),
                "http://localhost",
                StatusCode::CONFLICT,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/plugins/bundled")
                        .header("authorization", format!("Bearer {token}"))
                        .header("host", "localhost")
                        .header("origin", origin)
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
