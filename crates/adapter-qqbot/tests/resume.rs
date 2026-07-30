use std::{sync::Arc, time::Duration};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use bot_core::{Adapter, shutdown_channel};
use futures_util::{SinkExt, StreamExt};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use url::Url;

async fn token() -> Json<Value> {
    Json(json!({"access_token":"resume-token","expires_in":7200}))
}

async fn gateway(State(url): State<String>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(headers["authorization"], "QQBot resume-token");
    Json(json!({"url":url}))
}

async fn start_http(gateway_url: String) -> (Url, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/gateway", get(gateway))
        .with_state(gateway_url);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (Url::parse(&format!("http://{address}/")).unwrap(), task)
}

async fn send(socket: &mut WebSocketStream<TcpStream>, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn receive(socket: &mut WebSocketStream<TcpStream>) -> Value {
    let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
        panic!("expected text payload");
    };
    serde_json::from_str(text.as_str()).unwrap()
}

async fn first_connection(listener: &TcpListener, continue_reconnect: oneshot::Receiver<()>) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    send(
        &mut socket,
        json!({"op":10,"d":{"heartbeat_interval":10000}}),
    )
    .await;
    assert_eq!(receive(&mut socket).await["op"], 2);
    send(
        &mut socket,
        json!({"op":0,"s":1,"t":"READY","d":{"session_id":"resume-session"}}),
    )
    .await;
    send(
        &mut socket,
        json!({
            "id":"resume-event",
            "op":0,
            "s":2,
            "t":"GROUP_AT_MESSAGE_CREATE",
            "d":{
                "id":"resume-message",
                "content":"hello",
                "group_openid":"group-id",
                "author":{"member_openid":"member-id"}
            }
        }),
    )
    .await;
    continue_reconnect.await.unwrap();
    send(&mut socket, json!({"op":7,"d":null})).await;
}

async fn second_connection(listener: &TcpListener) -> Value {
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = accept_async(stream).await.unwrap();
    send(
        &mut socket,
        json!({"op":10,"d":{"heartbeat_interval":10000}}),
    )
    .await;
    receive(&mut socket).await
}

#[tokio::test]
async fn reconnect_resumes_from_last_runtime_committed_sequence() {
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_address = gateway_listener.local_addr().unwrap();
    let (continue_sender, continue_receiver) = oneshot::channel();
    let (resume_sender, resume_receiver) = oneshot::channel();
    let gateway_task = tokio::spawn(async move {
        first_connection(&gateway_listener, continue_receiver).await;
        let resume = second_connection(&gateway_listener).await;
        resume_sender.send(resume).unwrap();
    });
    let (base_url, http_task) = start_http(format!("ws://{gateway_address}")).await;
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        base_url.join("app/getAppAccessToken").unwrap(),
        "app-id",
        SecretString::from("app-secret".to_owned().into_boxed_str()),
    );
    let api = OpenApiClient::with_base_url(base_url, tokens).unwrap();
    let adapter = Arc::new(QqWebSocketAdapter::new(
        QqWebSocketConfig {
            reconnect_min_delay: Duration::from_millis(1),
            reconnect_max_delay: Duration::from_millis(5),
            ..QqWebSocketConfig::default()
        },
        api,
    ));
    let (events_sender, mut events) = mpsc::channel(4);
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let running_adapter = Arc::clone(&adapter);
    let adapter_task =
        tokio::spawn(async move { running_adapter.run(events_sender, shutdown_signal).await });

    let event = timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    adapter.event_handled(&event).await.unwrap();
    continue_sender.send(()).unwrap();
    let resume = timeout(Duration::from_secs(2), resume_receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["session_id"], "resume-session");
    assert_eq!(resume["d"]["seq"], 2);
    assert_eq!(resume["d"]["token"], "QQBot resume-token");

    shutdown_handle.shutdown();
    timeout(Duration::from_secs(2), adapter_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    gateway_task.await.unwrap();
    http_task.abort();
}
