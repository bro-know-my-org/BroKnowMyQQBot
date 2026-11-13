use super::*;
use axum::body::{Body, to_bytes};
use tower::ServiceExt as _;

const TOKEN: &str = "local-test-token-0123456789abcdef0123456789abcdef";

fn state() -> AuthState {
    super::super::plugins::token_digest(TOKEN).unwrap().into()
}
fn app(auth: AuthState) -> Router {
    router(auth.clone()).merge(
        Router::new()
            .route("/protected", get(async || "ok"))
            .route_layer(middleware::from_fn_with_state(auth, authenticate)),
    )
}
async fn call(
    app: Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    for &(name, value) in headers {
        request = request.header(name, value);
    }
    app.oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}
async fn json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap()
}
async fn login(app: Router) -> Response {
    call(
        app,
        "POST",
        "/api/session/login",
        &[],
        &json!({"token":TOKEN}).to_string(),
    )
    .await
}
fn session_cookie(response: &Response) -> String {
    response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn cookie_survives_refresh_and_logout_revokes_replay() {
    let auth = state();
    let app = app(auth);
    assert_eq!(
        json(call(app.clone(), "GET", "/api/session", &[], "").await).await["authenticated"],
        false
    );
    let response = login(app.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/api"] {
        assert!(set_cookie.contains(attribute));
    }
    assert!(!set_cookie.contains(TOKEN));
    assert!(!set_cookie.contains("Max-Age"));
    let cookie = session_cookie(&response);
    let headers = [("cookie", cookie.as_str())];
    assert_eq!(
        json(response).await,
        json!({"mode":"token","authenticated":true})
    );
    assert_eq!(
        json(call(app.clone(), "GET", "/api/session", &headers, "").await).await["authenticated"],
        true
    );
    assert_eq!(
        call(app.clone(), "GET", "/protected", &headers, "")
            .await
            .status(),
        StatusCode::OK
    );
    let logout = call(app.clone(), "POST", "/api/session/logout", &headers, "").await;
    assert!(
        logout.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        call(app, "GET", "/protected", &headers, "").await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn none_mode_requires_loopback_host_but_no_token() {
    let app = app(AuthState {
        expected: None,
        secure_cookie: false,
        sessions: Arc::default(),
    });
    for value in [
        "localhost:9090",
        "LOCALHOST",
        "127.0.0.1:9090",
        "127.1.2.3",
        "[::1]:9090",
    ] {
        let headers = [("host", value)];
        assert_eq!(
            call(app.clone(), "GET", "/protected", &headers, "")
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            json(call(app.clone(), "GET", "/api/session", &headers, "").await).await,
            json!({"mode":"none","authenticated":true})
        );
    }
    for headers in [
        vec![],
        vec![("host", "attacker.example")],
        vec![("host", "localhost.attacker.example")],
        vec![("host", "localhost:bad")],
        vec![("host", "localhost:")],
        vec![("host", "localhost:65536")],
        vec![("host", "localhost:+80")],
    ] {
        assert_eq!(
            call(app.clone(), "GET", "/protected", &headers, "")
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
}
