use super::super::plugins::{router, token_digest};
use crate::plugin_configuration::fixture::Fixture;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt as _;

const TOKEN: &str = "local-package-test-token-0123456789abcdef";

#[tokio::test]
async fn irregular_chunks_cannot_grow_retained_upload_allocation_past_budget() {
    let stream = futures_util::stream::iter(
        [17, 1].map(|n| Ok::<_, std::io::Error>(vec![0; n * 1024 * 1024])),
    );
    let bytes = super::read_upload(Body::from_stream(stream)).await.unwrap();
    assert_eq!(bytes.len(), 18 * 1024 * 1024);
    assert_eq!(bytes.capacity(), super::MAX_UPLOAD_BYTES);
}

#[tokio::test]
async fn inspect_uses_real_http_auth_source_limits_and_never_installs() {
    let fixture = Fixture::new(false);
    let manager = fixture.manager().await;
    let app = router(manager.clone(), token_digest(TOKEN).unwrap());
    let send = |body: Body, authorized: bool, origin: &'static str| {
        let app = app.clone();
        async move {
            let mut request = Request::post("/api/plugins/local/inspect")
                .header("Host", "localhost:52743")
                .header("Origin", origin)
                .header("Content-Type", "application/octet-stream");
            if authorized {
                request = request.header("Authorization", format!("Bearer {TOKEN}"));
            }
            app.oneshot(request.body(body).unwrap()).await.unwrap()
        }
    };
    assert_eq!(
        send(Body::from("invalid"), false, "http://localhost:52743")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(Body::from("invalid"), true, "https://evil.example")
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    // Missing Content-Length, streaming oversize: hard bound still applies.
    let failed = futures_util::stream::once(async {
        Err::<Vec<u8>, _>(std::io::Error::other("fixture read failure"))
    });
    assert_eq!(
        send(Body::from_stream(failed), true, "http://localhost:52743")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let stream =
        futures_util::stream::iter((0..33).map(|_| Ok::<_, std::io::Error>(vec![0; 1024 * 1024])));
    assert_eq!(
        send(Body::from_stream(stream), true, "http://localhost:52743")
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for invalid in [b"invalid".as_slice(), b"PK\x03\x04".as_slice()] {
        assert_eq!(
            send(Body::from(invalid.to_vec()), true, "http://localhost:52743")
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let bytes = std::fs::read(&fixture.installation.package_path).unwrap();
    let response = send(Body::from(bytes), true, "http://localhost:52743").await;
    assert_eq!(response.status(), StatusCode::OK);
    let data: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(data["sha256"], fixture.installation.package_sha256);
    assert_eq!(data["plugin_id"], fixture.installation.plugin_id);
    assert_eq!(data["signature_status"], "unsigned");
    assert!(uuid::Uuid::parse_str(data["selection"].as_str().unwrap()).is_ok());
    assert_eq!(
        fixture.store.installations().unwrap(),
        vec![fixture.installation.clone()]
    );
    assert!(!fixture.directory.join("plugins").exists());
    assert!(
        manager
            .list()
            .await
            .unwrap()
            .iter()
            .all(|p| serde_json::to_value(p).unwrap()["loaded_version"].is_null())
    );
}
