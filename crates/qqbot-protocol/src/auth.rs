//! Access Token acquisition, caching, and refresh coordination.

use std::{fmt, sync::Arc, time::Duration};

use reqwest::{Client, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::Mutex, time::Instant};
use url::Url;

const DEFAULT_TOKEN_ENDPOINT: &str = "https://api.bot.qq.com/app/getAppAccessToken";
const DEFAULT_REFRESH_BEFORE: Duration = Duration::from_secs(60);

/// A redacted QQ Access Token.
#[derive(Clone)]
pub struct AccessToken(Arc<SecretString>);

impl AccessToken {
    fn new(value: String) -> Self {
        Self(Arc::new(SecretString::from(value.into_boxed_str())))
    }

    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken([REDACTED])")
    }
}

/// Errors produced while acquiring or refreshing a QQ Access Token.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("failed to call QQ Access Token endpoint")]
    Request(#[source] reqwest::Error),
    #[error("QQ Access Token endpoint returned HTTP {status}")]
    HttpStatus { status: StatusCode },
    #[error("QQ Access Token response was invalid: {0}")]
    InvalidResponse(String),
}

/// Concurrency-safe in-memory Access Token manager.
#[derive(Clone)]
pub struct TokenManager {
    inner: Arc<TokenManagerInner>,
}

struct TokenManagerInner {
    client: Client,
    endpoint: Url,
    app_id: String,
    client_secret: SecretString,
    refresh_before: Duration,
    cached: Mutex<Option<CachedToken>>,
    refresh: Mutex<()>,
}

struct CachedToken {
    token: AccessToken,
    expires_at: Instant,
}

impl fmt::Debug for TokenManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenManager")
            .field("endpoint", &self.inner.endpoint)
            .field("app_id", &self.inner.app_id)
            .field("client_secret", &"[REDACTED]")
            .field("refresh_before", &self.inner.refresh_before)
            .finish_non_exhaustive()
    }
}

impl TokenManager {
    pub fn new(app_id: impl Into<String>, client_secret: SecretString) -> Result<Self, AuthError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(AuthError::Request)?;
        let endpoint = Url::parse(DEFAULT_TOKEN_ENDPOINT)
            .map_err(|error| AuthError::InvalidResponse(error.to_string()))?;
        Ok(Self::with_client_and_endpoint(
            client,
            endpoint,
            app_id,
            client_secret,
        ))
    }

    pub fn with_client_and_endpoint(
        client: Client,
        endpoint: Url,
        app_id: impl Into<String>,
        client_secret: SecretString,
    ) -> Self {
        Self::with_refresh_window(
            client,
            endpoint,
            app_id,
            client_secret,
            DEFAULT_REFRESH_BEFORE,
        )
    }

    pub fn with_refresh_window(
        client: Client,
        endpoint: Url,
        app_id: impl Into<String>,
        client_secret: SecretString,
        refresh_before: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(TokenManagerInner {
                client,
                endpoint,
                app_id: app_id.into(),
                client_secret,
                refresh_before,
                cached: Mutex::new(None),
                refresh: Mutex::new(()),
            }),
        }
    }

    pub async fn access_token(&self) -> Result<AccessToken, AuthError> {
        if let Some(token) = self.cached_token(true).await {
            return Ok(token);
        }

        let _refresh_guard = self.inner.refresh.lock().await;
        if let Some(token) = self.cached_token(true).await {
            return Ok(token);
        }

        match self.fetch().await {
            Ok(token) => Ok(token),
            Err(error) => self.cached_token(false).await.ok_or(error),
        }
    }

    pub async fn force_refresh(&self) -> Result<AccessToken, AuthError> {
        let _refresh_guard = self.inner.refresh.lock().await;
        self.fetch().await
    }

    pub async fn refresh_if_current(
        &self,
        rejected: &AccessToken,
    ) -> Result<AccessToken, AuthError> {
        let _refresh_guard = self.inner.refresh.lock().await;
        let replacement = {
            let cached = self.inner.cached.lock().await;
            cached.as_ref().and_then(|cached| {
                (cached.token.expose() != rejected.expose() && cached.expires_at > Instant::now())
                    .then(|| cached.token.clone())
            })
        };
        if let Some(replacement) = replacement {
            return Ok(replacement);
        }
        self.fetch().await
    }

    pub async fn invalidate(&self) {
        let _refresh_guard = self.inner.refresh.lock().await;
        *self.inner.cached.lock().await = None;
    }

    async fn cached_token(&self, apply_refresh_window: bool) -> Option<AccessToken> {
        let cached = self.inner.cached.lock().await;
        let cached = cached.as_ref()?;
        let required = if apply_refresh_window {
            self.inner.refresh_before
        } else {
            Duration::ZERO
        };

        (cached.expires_at.saturating_duration_since(Instant::now()) > required)
            .then(|| cached.token.clone())
    }

    async fn fetch(&self) -> Result<AccessToken, AuthError> {
        let request = TokenRequest {
            app_id: &self.inner.app_id,
            client_secret: self.inner.client_secret.expose_secret(),
        };
        let response = self
            .inner
            .client
            .post(self.inner.endpoint.clone())
            .json(&request)
            .send()
            .await
            .map_err(AuthError::Request)?;

        if !response.status().is_success() {
            return Err(AuthError::HttpStatus {
                status: response.status(),
            });
        }

        let response: TokenResponse = response.json().await.map_err(AuthError::Request)?;
        let expires_in = response.expires_in.as_u64().ok_or_else(|| {
            AuthError::InvalidResponse("expires_in must be a positive integer".to_owned())
        })?;
        if response.access_token.is_empty() {
            return Err(AuthError::InvalidResponse(
                "access_token must not be empty".to_owned(),
            ));
        }

        let token = AccessToken::new(response.access_token);
        let expires_at = Instant::now()
            .checked_add(Duration::from_secs(expires_in))
            .ok_or_else(|| AuthError::InvalidResponse("expires_in is too large".to_owned()))?;
        *self.inner.cached.lock().await = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });
        Ok(token)
    }
}

#[derive(Serialize)]
struct TokenRequest<'a> {
    #[serde(rename = "appId")]
    app_id: &'a str,
    #[serde(rename = "clientSecret")]
    client_secret: &'a str,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: ExpiresIn,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExpiresIn {
    Number(u64),
    String(String),
}

impl ExpiresIn {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => (*value > 0).then_some(*value),
            Self::String(value) => value.parse().ok().filter(|value| *value > 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use reqwest::Client;
    use secrecy::SecretString;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, sync::Barrier};
    use url::Url;

    use super::TokenManager;

    #[derive(Clone)]
    struct ServerState {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        fail_after_first: bool,
    }

    async fn token_handler(
        State(state): State<ServerState>,
        Json(body): Json<Value>,
    ) -> Result<Json<Value>, StatusCode> {
        assert_eq!(body["appId"], "app-id");
        assert_eq!(body["clientSecret"], "app-secret");
        let call = state
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if state.fail_after_first && call > 0 {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok(Json(
            json!({"access_token":"access-token","expires_in":7200}),
        ))
    }

    async fn manager(
        fail_after_first: bool,
        refresh_before: Duration,
    ) -> (TokenManager, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = Router::new()
            .route("/app/getAppAccessToken", post(token_handler))
            .with_state(ServerState {
                calls: Arc::clone(&calls),
                fail_after_first,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let endpoint = Url::parse(&format!("http://{address}/app/getAppAccessToken")).unwrap();
        let manager = TokenManager::with_refresh_window(
            Client::new(),
            endpoint,
            "app-id",
            SecretString::from("app-secret".to_owned().into_boxed_str()),
            refresh_before,
        );
        (manager, calls)
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let (manager, calls) = manager(false, Duration::from_secs(60)).await;
        let barrier = Arc::new(Barrier::new(17));
        let mut tasks = Vec::new();

        for _ in 0..16 {
            let manager = manager.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                manager.access_token().await.unwrap().expose().to_owned()
            }));
        }
        barrier.wait().await;

        for task in tasks {
            assert_eq!(task.await.unwrap(), "access-token");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_failure_keeps_still_valid_token() {
        let (manager, calls) = manager(true, Duration::from_secs(7200)).await;
        assert_eq!(
            manager.force_refresh().await.unwrap().expose(),
            "access-token"
        );

        let token = manager.access_token().await.unwrap();
        assert_eq!(token.expose(), "access-token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
