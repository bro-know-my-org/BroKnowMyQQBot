use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{post, put},
};
use bot_core::{Action, Adapter, AdapterError};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    api_calls: AtomicUsize,
    requests: Mutex<Vec<(String, Value)>>,
    authorizations: Mutex<Vec<(String, String)>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    let call = observations.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({"access_token":format!("token-{call}"),"expires_in":7200}))
}

async fn respond(
    State(observations): State<Arc<Observations>>,
    Path(interaction_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("interaction request must be authorized")
        .to_owned();
    observations.api_calls.fetch_add(1, Ordering::SeqCst);
    observations
        .authorizations
        .lock()
        .unwrap()
        .push((interaction_id.clone(), authorization));
    observations
        .requests
        .lock()
        .unwrap()
        .push((interaction_id.clone(), body));
    match interaction_id.as_str() {
        "interaction-empty" => StatusCode::NO_CONTENT.into_response(),
        "platform-error" => Json(json!({"code":630_003,"message":"appid invalid"})).into_response(),
        "rate-limited" => (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "2")],
            Json(json!({"code":630_008,"message":"preprocess failed"})),
        )
            .into_response(),
        "unauthorized" => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":630_006,"message":"header appid failed"})),
        )
            .into_response(),
        "redirect" => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/interactions/redirect-target")],
        )
            .into_response(),
        "permanent-redirect" => (
            StatusCode::PERMANENT_REDIRECT,
            [("location", "/interactions/permanent-redirect-target")],
        )
            .into_response(),
        _ => Json(json!({})).into_response(),
    }
}

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/interactions/{interaction_id}", put(respond))
        .with_state(Arc::clone(&observations));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base_url = Url::parse(&format!("http://{address}/")).unwrap();
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        base_url.join("app/getAppAccessToken").unwrap(),
        "app-id",
        SecretString::from("secret".to_owned().into_boxed_str()),
    );
    (
        QqWebSocketAdapter::new(
            QqWebSocketConfig::default(),
            OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        ),
        observations,
        server_task,
    )
}

async fn platform(adapter: &QqWebSocketAdapter, payload: Value) -> Result<Value, AdapterError> {
    tokio::time::timeout(
        Duration::from_secs(2),
        adapter.execute(Action::Platform {
            name: "qq.interaction.respond".to_owned(),
            payload,
        }),
    )
    .await
    .expect("interaction action timed out")
    .map(|result| result.raw)
}

#[tokio::test]
async fn exposes_interaction_response_action() {
    let (adapter, observations, server_task) = adapter().await;
    assert_eq!(
        platform(
            &adapter,
            json!({"interaction_id":"interaction/id","code":0}),
        )
        .await
        .unwrap(),
        Value::Null
    );
    assert_eq!(
        platform(&adapter, json!({"interaction_id":"interaction-empty"}),)
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observations.requests.lock().unwrap(),
        vec![
            ("interaction/id".to_owned(), json!({"code":0})),
            ("interaction-empty".to_owned(), json!({})),
        ]
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_interaction_actions_before_io() {
    let (adapter, observations, server_task) = adapter().await;
    for (payload, expected) in [
        (json!({"interaction_id":" "}), "must not be empty"),
        (
            json!({"interaction_id":"INTERACTION_CREATE:interaction-id"}),
            "without an INTERACTION_CREATE prefix",
        ),
        (
            json!({"interaction_id":"bad id"}),
            "whitespace or control characters",
        ),
        (
            json!({"interaction_id":"interaction-id "}),
            "whitespace or control characters",
        ),
        (
            json!({"interaction_id":"interaction-id\n"}),
            "whitespace or control characters",
        ),
        (
            json!({"interaction_id":"interaction-id\t"}),
            "whitespace or control characters",
        ),
        (
            json!({"interaction_id":"interaction-id\0"}),
            "whitespace or control characters",
        ),
        (
            json!({"interaction_id":"interaction-id","code":6}),
            "between 0 and 5",
        ),
    ] {
        let error = platform(&adapter, payload).await.unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic interaction Action error");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }
    for payload in [
        json!({}),
        json!({"interaction_id":"interaction-id","code":"0"}),
        json!({"interaction_id":"interaction-id","codes":0}),
    ] {
        assert!(matches!(
            platform(&adapter, payload).await,
            Err(AdapterError::Action(_))
        ));
    }
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 0);
    assert!(observations.requests.lock().unwrap().is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn returns_platform_and_rate_limit_errors() {
    let (adapter, observations, server_task) = adapter().await;
    for (interaction_id, expected) in [
        ("platform-error", "630003"),
        ("rate-limited", "630008"),
        ("redirect", "307"),
        ("permanent-redirect", "308"),
    ] {
        let error = platform(&adapter, json!({"interaction_id":interaction_id}))
            .await
            .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected interaction Action error");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 4);
    {
        let requests = observations.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|(interaction_id, _)| !matches!(
            interaction_id.as_str(),
            "redirect-target" | "permanent-redirect-target"
        )));
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn refreshes_after_unauthorized_without_replaying_interaction() {
    let (adapter, observations, server_task) = adapter().await;
    let error = platform(&adapter, json!({"interaction_id":"unauthorized"}))
        .await
        .unwrap_err();
    let AdapterError::Action(message) = error else {
        panic!("expected interaction Action error");
    };
    assert!(message.contains("401"), "unexpected error: {message}");
    assert!(message.contains("630006"), "unexpected error: {message}");
    assert!(
        message.contains("header appid failed"),
        "unexpected error: {message}"
    );

    assert_eq!(
        platform(&adapter, json!({"interaction_id":"after-unauthorized"}))
            .await
            .unwrap(),
        Value::Null
    );
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 2);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observations.authorizations.lock().unwrap(),
        vec![
            ("unauthorized".to_owned(), "QQBot token-1".to_owned()),
            ("after-unauthorized".to_owned(), "QQBot token-2".to_owned()),
        ]
    );
    {
        let requests = observations.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(interaction_id, _)| interaction_id == "unauthorized")
                .count(),
            1
        );
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
