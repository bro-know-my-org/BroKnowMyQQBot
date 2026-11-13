//! Browser sessions and native Bearer access share the same management boundary.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, Uri, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

const COOKIE: &str = "bkmqb_management_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 128;

#[derive(Clone)]
pub(super) struct AuthState {
    expected: Option<[u8; 32]>,
    secure_cookie: bool,
    sessions: Arc<Mutex<HashMap<[u8; 32], Instant>>>,
}

impl From<[u8; 32]> for AuthState {
    fn from(expected: [u8; 32]) -> Self {
        Self {
            expected: Some(expected),
            secure_cookie: false,
            sessions: Arc::default(),
        }
    }
}

impl AuthState {
    pub(super) fn from_config(
        config: &crate::config::ManagementConfig,
    ) -> Result<Self, super::ManagementError> {
        if config.auth == crate::config::ManagementAuth::None {
            return Ok(Self {
                expected: None,
                secure_cookie: config.session_cookie_secure,
                sessions: Arc::default(),
            });
        }
        let token = std::env::var("BKMQB_MANAGEMENT_TOKEN")
            .map_err(|_| super::ManagementError::InvalidToken)?;
        let mut auth: Self = super::plugins::token_digest(&token)?.into();
        auth.secure_cookie = config.session_cookie_secure;
        Ok(auth)
    }

    fn token_valid(&self, token: &str) -> bool {
        self.expected.is_some_and(|expected| {
            super::plugins::token_digest(token)
                .is_ok_and(|actual| bool::from(expected.ct_eq(&actual)))
        })
    }

    pub(super) fn authenticated(&self, headers: &HeaderMap) -> bool {
        if self.expected.is_none() {
            return true;
        }
        if single_header(headers, header::AUTHORIZATION.as_str())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| self.token_valid(token))
        {
            return true;
        }
        let Some(digest) = session_digest(headers) else {
            return false;
        };
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        sessions.retain(|_, expires| *expires > now);
        sessions.contains_key(&digest)
    }

    fn status(&self, authenticated: bool) -> Json<Value> {
        Json(
            json!({"mode": if self.expected.is_some() { "token" } else { "none" }, "authenticated": authenticated}),
        )
    }
}

pub(super) fn router(auth: AuthState) -> Router {
    Router::new()
        .route("/api/session", get(session))
        .route("/api/session/login", post(login))
        .route("/api/session/logout", post(logout))
        .layer(DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn(super::plugins::read_body))
        .route_layer(middleware::from_fn_with_state(auth.clone(), check_source))
        .with_state(auth)
}

pub(super) async fn authenticate(
    State(auth): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    if !source_allowed(&auth, request.headers(), request.uri()) {
        return forbidden();
    }
    if !auth.authenticated(request.headers()) {
        return unauthorized();
    }
    next.run(request).await
}

async fn check_source(State(auth): State<AuthState>, request: Request, next: Next) -> Response {
    if !source_allowed(&auth, request.headers(), request.uri()) {
        return forbidden();
    }
    next.run(request).await
}

async fn session(State(auth): State<AuthState>, headers: HeaderMap) -> Json<Value> {
    auth.status(auth.authenticated(&headers))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    token: String,
}

async fn login(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    uri: Uri,
    Json(login): Json<Login>,
) -> Response {
    if auth.expected.is_none() {
        return auth.status(true).into_response();
    }
    if !auth.token_valid(&login.token) {
        return unauthorized();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let digest = Sha256::digest(id.as_bytes()).into();
    let now = Instant::now();
    let mut sessions = auth
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sessions.retain(|_, expires| *expires > now);
    let previous = session_digest(&headers);
    let replacing = previous.is_some_and(|id| sessions.contains_key(&id));
    if sessions.len() >= MAX_SESSIONS && !replacing {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"管理会话已达上限，请退出旧会话或稍后重试"})),
        )
            .into_response();
    }
    if let Some(previous) = previous {
        sessions.remove(&previous);
    }
    sessions.insert(digest, now + SESSION_TTL);
    (
        [(
            header::SET_COOKIE,
            cookie(
                &id,
                false,
                auth.secure_cookie || uri.scheme_str() == Some("https"),
            ),
        )],
        auth.status(true),
    )
        .into_response()
}

async fn logout(State(auth): State<AuthState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(digest) = session_digest(&headers) {
        auth.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&digest);
    }
    (
        [(
            header::SET_COOKIE,
            cookie(
                "",
                true,
                auth.secure_cookie || uri.scheme_str() == Some("https"),
            ),
        )],
        auth.status(auth.expected.is_none()),
    )
        .into_response()
}

fn cookie(id: &str, expired: bool, secure: bool) -> String {
    format!(
        "{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/api{}{}",
        if expired { "; Max-Age=0" } else { "" },
        if secure { "; Secure" } else { "" }
    )
}

fn session_digest(headers: &HeaderMap) -> Option<[u8; 32]> {
    let mut result = None;
    for header in headers.get_all(header::COOKIE) {
        for pair in header.to_str().ok()?.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name != COOKIE {
                continue;
            }
            if result.is_some() || value.len() != 36 || uuid::Uuid::parse_str(value).is_err() {
                return None;
            }
            result = Some(Sha256::digest(value.as_bytes()).into());
        }
    }
    result
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

fn authority(value: &str) -> Option<Authority> {
    if value.contains('@') {
        return None;
    }
    let authority = value.parse::<Authority>().ok()?;
    let suffix = &authority.as_str()[authority.host().len()..];
    if authority.host().is_empty()
        || (!suffix.is_empty()
            && !suffix.strip_prefix(':').is_some_and(|port| {
                port.parse::<u16>().is_ok() && port.bytes().all(|byte| byte.is_ascii_digit())
            }))
    {
        return None;
    }
    Some(authority)
}

fn host(authority: &Authority) -> String {
    let value = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    value
        .parse::<IpAddr>()
        .map_or_else(|_| value.to_ascii_lowercase(), |ip| ip.to_string())
}

fn source_allowed(auth: &AuthState, headers: &HeaderMap, uri: &Uri) -> bool {
    if headers.contains_key("sec-fetch-site")
        && !matches!(
            single_header(headers, "sec-fetch-site"),
            Some("same-origin" | "none")
        )
    {
        return false;
    }
    let request_host = single_header(headers, "host").and_then(authority);
    if headers.contains_key(header::HOST) && request_host.is_none() {
        return false;
    }
    if auth.expected.is_none()
        && !request_host.as_ref().is_some_and(|value| {
            let host = host(value);
            host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
        })
    {
        return false;
    }
    if !headers.contains_key(header::ORIGIN) {
        return true;
    }
    let Some(origin) = single_header(headers, "origin").and_then(|value| value.parse::<Uri>().ok())
    else {
        return false;
    };
    let Some(scheme @ ("http" | "https")) = origin.scheme_str() else {
        return false;
    };
    if origin.query().is_some()
        || !matches!(origin.path(), "" | "/")
        || uri.scheme_str().is_some_and(|value| value != scheme)
    {
        return false;
    }
    let Some(origin_host) = origin
        .authority()
        .and_then(|value| authority(value.as_str()))
    else {
        return false;
    };
    let Some(request_host) = request_host else {
        return false;
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    host(&origin_host) == host(&request_host)
        && origin_host.port_u16().unwrap_or(default_port)
            == request_host.port_u16().unwrap_or(default_port)
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error":"管理请求来源不受信任"})),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({"error":"管理令牌无效或未提供"})),
    )
        .into_response()
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;

#[cfg(test)]
mod stream_expiration_test {
    use super::*;
    use axum::body::Body;
    use futures_util::StreamExt as _;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn expiration_terminates_an_established_log_stream() {
        let auth: AuthState = [0; 32].into();
        let id = uuid::Uuid::new_v4().to_string();
        let digest = Sha256::digest(id.as_bytes()).into();
        auth.sessions
            .lock()
            .unwrap()
            .insert(digest, Instant::now() + SESSION_TTL);
        let app = super::super::logs_stream::router(
            crate::logging::live::LogBuffer::default(),
            auth.clone(),
            None,
        );
        let response = app
            .oneshot(
                Request::get("/api/logs/stream")
                    .header(header::COOKIE, format!("{COOKIE}={id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        assert!(
            std::str::from_utf8(&body.next().await.unwrap().unwrap())
                .unwrap()
                .contains("event: logs")
        );
        auth.sessions.lock().unwrap().insert(
            digest,
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        );
        let event = body.next().await.unwrap().unwrap();
        assert!(
            std::str::from_utf8(&event)
                .unwrap()
                .contains("event: auth-required")
        );
        assert!(body.next().await.is_none());
    }
}
