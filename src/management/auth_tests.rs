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
async fn bearer_and_login_validate_credentials_without_echoing_them() {
    let app = app(state());
    for (token, status) in [(TOKEN, StatusCode::OK), ("wrong", StatusCode::UNAUTHORIZED)] {
        assert_eq!(
            call(
                app.clone(),
                "GET",
                "/protected",
                &[("authorization", &format!("Bearer {token}"))],
                ""
            )
            .await
            .status(),
            status
        );
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/session/login",
                &[],
                &json!({"token":token}).to_string()
            )
            .await
            .status(),
            status
        );
    }
    for body in [r#"{"token":"wrong","extra":true}"#, r#"{"token":12}"#] {
        assert_eq!(
            call(app.clone(), "POST", "/api/session/login", &[], body)
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        call(app, "POST", "/api/session/login", &[], &" ".repeat(4097))
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
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

#[tokio::test]
async fn source_policy_rejects_cross_site_and_normalizes_origin_authority() {
    let app = app(state());
    for path in ["/api/session", "/protected"] {
        for source in ["cross-site", "same-site"] {
            assert_eq!(
                call(app.clone(), "GET", path, &[("sec-fetch-site", source)], "")
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        for origin in [
            "null",
            "http://evil.example",
            "http://localhost:1234",
            "http://localhost/path",
            "http://localhost:bad",
            "http://user@localhost",
            "https://localhost",
        ] {
            assert_eq!(
                call(
                    app.clone(),
                    "GET",
                    path,
                    &[("host", "localhost:80"), ("origin", origin)],
                    ""
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    for (host, origin) in [
        ("LOCALHOST:80", "http://localhost"),
        ("[::1]:80", "http://[0:0:0:0:0:0:0:1]"),
        ("localhost:443", "https://LOCALHOST"),
    ] {
        assert_eq!(
            call(
                app.clone(),
                "GET",
                "/api/session",
                &[
                    ("host", host),
                    ("origin", origin),
                    ("sec-fetch-site", "same-origin")
                ],
                ""
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            app,
            "POST",
            "/api/session/login",
            &[("host", "localhost"), ("origin", "http://evil.example")],
            "invalid"
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn expiration_and_capacity_preserve_existing_sessions() {
    let auth = state();
    let app = app(auth.clone());
    let cookie = session_cookie(&login(app.clone()).await);
    {
        let mut sessions = auth.sessions.lock().unwrap();
        *sessions.values_mut().next().unwrap() =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    }
    assert_eq!(
        call(app.clone(), "GET", "/protected", &[("cookie", &cookie)], "")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let valid_cookie = session_cookie(&login(app.clone()).await);
    {
        let mut sessions = auth.sessions.lock().unwrap();
        for n in 0u8..127 {
            sessions.insert([n; 32], Instant::now() + SESSION_TTL);
        }
    }
    assert_eq!(
        login(app.clone()).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(auth.sessions.lock().unwrap().len(), MAX_SESSIONS);
    assert_eq!(
        call(app, "GET", "/protected", &[("cookie", &valid_cookie)], "")
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn untrusted_source_is_rejected_before_reading_a_stalled_body() {
    let body = Body::from_stream(futures_util::stream::pending::<
        Result<axum::body::Bytes, std::io::Error>,
    >());
    let request = Request::post("/api/session/login")
        .header("sec-fetch-site", "cross-site")
        .body(body)
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(1), app(state()).oneshot(request))
        .await
        .expect("source rejection must not wait for the request body")
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn secure_cookie_only_uses_request_scheme_and_duplicate_cookie_is_rejected() {
    let app = app(state());
    let body = json!({"token":TOKEN}).to_string();
    let response = call(
        app.clone(),
        "POST",
        "/api/session/login",
        &[("x-forwarded-proto", "https"), ("forwarded", "proto=https")],
        &body,
    )
    .await;
    assert!(
        !response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Secure")
    );
    let duplicate = format!(
        "{}; {}",
        session_cookie(&response),
        session_cookie(&response)
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/protected",
            &[("cookie", &duplicate)],
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(
        app,
        "POST",
        "https://localhost/api/session/login",
        &[],
        &body,
    )
    .await;
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
}

#[tokio::test]
async fn relogin_rotates_cookie_at_capacity_and_secure_proxy_setting_is_explicit() {
    let mut auth = state();
    auth.secure_cookie = true;
    let app = app(auth.clone());
    let first = login(app.clone()).await;
    assert!(
        first.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
    let old_cookie = session_cookie(&first);
    {
        let mut sessions = auth.sessions.lock().unwrap();
        for _ in sessions.len()..MAX_SESSIONS {
            sessions.insert(
                Sha256::digest(uuid::Uuid::new_v4().as_bytes()).into(),
                Instant::now() + SESSION_TTL,
            );
        }
    }
    let response = call(
        app.clone(),
        "POST",
        "/api/session/login",
        &[("cookie", &old_cookie)],
        &json!({"token":TOKEN}).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(auth.sessions.lock().unwrap().len(), MAX_SESSIONS);
    let new_cookie = session_cookie(&response);
    assert_ne!(old_cookie, new_cookie);
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/protected",
            &[("cookie", &old_cookie)],
            ""
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/protected",
            &[("cookie", &new_cookie)],
            ""
        )
        .await
        .status(),
        StatusCode::OK
    );
    let response = call(
        app,
        "POST",
        "/api/session/logout",
        &[("cookie", &new_cookie)],
        "",
    )
    .await;
    assert!(
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("; Secure")
    );
}
