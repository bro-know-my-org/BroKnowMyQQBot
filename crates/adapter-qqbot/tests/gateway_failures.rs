use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use bot_core::{Adapter, AdapterError, EventSender, RuntimeObserver, shutdown_channel};
use futures_util::{SinkExt as _, StreamExt as _};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc, time::timeout};
use tokio_tungstenite::{
    accept_async,
    tungstenite::{
        Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};
use url::Url;

async fn token_ok(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({"access_token":"access-token","expires_in":7200}))
}

async fn token_unauthorized() -> StatusCode {
    StatusCode::UNAUTHORIZED
}

async fn gateway(State(url): State<String>) -> Json<Value> {
    Json(json!({"url":url}))
}

async fn start_http(router: Router) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (Url::parse(&format!("http://{address}/")).unwrap(), task)
}

fn api(base_url: Url, token_endpoint: Url) -> OpenApiClient {
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        token_endpoint,
        "app-id",
        SecretString::from("app-secret".to_owned().into_boxed_str()),
    );
    OpenApiClient::with_base_url(base_url, tokens).unwrap()
}

async fn run_until_error(adapter: QqWebSocketAdapter) -> AdapterError {
    let (_shutdown_handle, shutdown_signal) = shutdown_channel();
    let (events, _receiver) = mpsc::channel(1);
    let events = EventSender::new(events, adapter.id().clone(), RuntimeObserver::new()).unwrap();
    timeout(
        std::time::Duration::from_secs(2),
        adapter.run(events, shutdown_signal),
    )
    .await
    .expect("fatal Gateway configuration error must not enter reconnect loop")
    .unwrap_err()
}

#[tokio::test]
async fn invalid_credentials_fail_fast_instead_of_reconnecting_forever() {
    let router = Router::new().route("/app/getAppAccessToken", post(token_unauthorized));
    let (base_url, http_task) = start_http(router).await;
    let token_endpoint = base_url.join("app/getAppAccessToken").unwrap();
    let error = run_until_error(QqWebSocketAdapter::new(
        QqWebSocketConfig::default(),
        api(base_url, token_endpoint),
    ))
    .await;
    assert!(matches!(error, AdapterError::Configuration(message) if message.contains("401")));
    http_task.abort();
}

#[tokio::test]
async fn fatal_intent_close_code_fails_fast_with_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("ws://{}", listener.local_addr().unwrap());
    let gateway_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"op":10,"d":{"heartbeat_interval":10000}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let identify = socket.next().await.unwrap().unwrap();
        assert!(matches!(identify, Message::Text(_)));
        socket
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(4014),
                reason: "intent not permitted".into(),
            })))
            .await
            .unwrap();
    });
    let router = Router::new()
        .route("/app/getAppAccessToken", post(token_ok))
        .route("/gateway", get(gateway))
        .with_state(gateway_url);
    let (base_url, http_task) = start_http(router).await;
    let token_endpoint = base_url.join("app/getAppAccessToken").unwrap();
    let error = run_until_error(QqWebSocketAdapter::new(
        QqWebSocketConfig::default(),
        api(base_url, token_endpoint),
    ))
    .await;
    assert!(
        matches!(error, AdapterError::Configuration(message) if message.contains("4014") && message.contains("intent not permitted"))
    );
    timeout(std::time::Duration::from_secs(2), gateway_task)
        .await
        .expect("mock Gateway should complete")
        .unwrap();
    http_task.abort();
}
