use std::{collections::BTreeMap, sync::Arc, time::Duration};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use bot_core::{RuntimeBuilder, ShutdownHandle, shutdown_channel};
use builtin_plugins::{ActiveSendProbePlugin, EchoPlugin, PingPlugin};
use futures_util::{SinkExt, StreamExt};
use plugin_host::{PluginStore, StaticPluginHost};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle, time::timeout};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use url::Url;

#[derive(Clone)]
struct HttpState {
    gateway_url: String,
    replies: mpsc::Sender<Value>,
    shutdown: ShutdownHandle,
}

async fn token(Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["appId"], "app-id");
    assert_eq!(body["clientSecret"], "app-secret");
    Json(json!({"access_token":"access-token","expires_in":7200}))
}

async fn gateway(State(state): State<HttpState>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(headers["authorization"], "QQBot access-token");
    Json(json!({"url":state.gateway_url}))
}

async fn group_reply(
    State(state): State<HttpState>,
    Path(group_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(group_id, "group-id");
    assert_eq!(headers["authorization"], "QQBot access-token");
    state.replies.send(body).await.unwrap();
    state.shutdown.shutdown();
    (StatusCode::OK, Json(json!({"id":"reply-message-id"})))
}

async fn c2c_reply(
    State(state): State<HttpState>,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(user_id, "user-id");
    assert_eq!(headers["authorization"], "QQBot access-token");
    state.replies.send(body).await.unwrap();
    state.shutdown.shutdown();
    (StatusCode::OK, Json(json!({"id":"reply-message-id"})))
}

async fn channel_reply(
    State(state): State<HttpState>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(channel_id, "channel-id");
    assert_eq!(headers["authorization"], "QQBot access-token");
    state.replies.send(body).await.unwrap();
    state.shutdown.shutdown();
    (StatusCode::OK, Json(json!({"id":"reply-message-id"})))
}

async fn direct_reply(
    State(state): State<HttpState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(guild_id, "direct-guild-id");
    assert_eq!(headers["authorization"], "QQBot access-token");
    state.replies.send(body).await.unwrap();
    state.shutdown.shutdown();
    (StatusCode::OK, Json(json!({"id":"reply-message-id"})))
}

async fn start_gateway(message: &str) -> (String, JoinHandle<()>) {
    start_gateway_dispatch(
        "GROUP_AT_MESSAGE_CREATE",
        json!({
            "id":"source-message-id",
            "content":message,
            "group_openid":"group-id",
            "author":{"member_openid":"member-id"}
        }),
    )
    .await
}

async fn start_gateway_dispatch(event_type: &str, data: Value) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let event_type = event_type.to_owned();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        send(
            &mut socket,
            json!({"op":10,"d":{"heartbeat_interval":10000}}),
        )
        .await;

        let identify = receive_json(&mut socket).await;
        assert_eq!(identify["op"], 2);
        assert_eq!(identify["d"]["token"], "QQBot access-token");
        assert_eq!(identify["d"]["shard"], json!([0, 1]));

        send(
            &mut socket,
            json!({"op":0,"s":1,"t":"READY","d":{"session_id":"session-id"}}),
        )
        .await;
        send(
            &mut socket,
            json!({
                "id":"event-id",
                "op":0,
                "s":2,
                "t":event_type,
                "d":data
            }),
        )
        .await;

        while let Some(message) = socket.next().await {
            match message.unwrap() {
                Message::Text(text) => {
                    let payload: Value = serde_json::from_str(text.as_str()).unwrap();
                    if payload["op"] == 1 {
                        send(&mut socket, json!({"op":11,"d":null})).await;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    (format!("ws://{address}"), task)
}

async fn start_http(
    gateway_url: String,
    shutdown: ShutdownHandle,
) -> (Url, mpsc::Receiver<Value>, JoinHandle<()>) {
    let (reply_sender, reply_receiver) = mpsc::channel(4);
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/gateway", get(gateway))
        .route("/v2/groups/{group_id}/messages", post(group_reply))
        .route("/v2/users/{user_id}/messages", post(c2c_reply))
        .route("/channels/{channel_id}/messages", post(channel_reply))
        .route("/dms/{guild_id}/messages", post(direct_reply))
        .with_state(HttpState {
            gateway_url,
            replies: reply_sender,
            shutdown,
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        Url::parse(&format!("http://{address}/")).unwrap(),
        reply_receiver,
        task,
    )
}

async fn run_ping_dispatch(event_type: &str, data: Value) -> Value {
    let (gateway_url, gateway_task) = start_gateway_dispatch(event_type, data).await;
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (base_url, mut replies, http_task) = start_http(gateway_url, shutdown_handle).await;
    let token_endpoint = base_url.join("app/getAppAccessToken").unwrap();
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        token_endpoint,
        "app-id",
        SecretString::from("app-secret".to_owned().into_boxed_str()),
    );
    let api = OpenApiClient::with_base_url(base_url, tokens).unwrap();
    let adapter = Arc::new(QqWebSocketAdapter::new(QqWebSocketConfig::default(), api));
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    plugins
        .register_trusted(
            Arc::new(PingPlugin::default()),
            "dev.bkm.ping/qq-websocket-test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new()
        .shutdown_timeout(Duration::from_secs(2))
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    timeout(Duration::from_secs(5), runtime.run(shutdown_signal))
        .await
        .expect("runtime should finish")
        .unwrap();
    let reply = timeout(Duration::from_secs(1), replies.recv())
        .await
        .unwrap()
        .unwrap();
    gateway_task.await.unwrap();
    http_task.abort();
    reply
}

async fn send<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>, value: Value)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn receive_json<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
        panic!("expected text payload");
    };
    serde_json::from_str(text.as_str()).unwrap()
}

#[tokio::test]
async fn qq_websocket_event_reaches_runtime_and_replies_through_openapi() {
    let reply = run_ping_dispatch(
        "GROUP_AT_MESSAGE_CREATE",
        json!({
            "id":"source-message-id",
            "content":"/ping",
            "group_openid":"group-id",
            "author":{"member_openid":"member-id"}
        }),
    )
    .await;
    assert_eq!(reply["content"], "pong");
    assert_eq!(reply["msg_id"], "source-message-id");
    assert_eq!(reply["msg_type"], 0);
}

#[tokio::test]
async fn c2c_websocket_event_replies_through_user_openapi() {
    let reply = run_ping_dispatch(
        "C2C_MESSAGE_CREATE",
        json!({
            "id":"source-message-id",
            "content":"/ping",
            "author":{"user_openid":"user-id"}
        }),
    )
    .await;
    assert_eq!(reply["content"], "pong");
    assert_eq!(reply["msg_id"], "source-message-id");
    assert_eq!(reply["msg_type"], 0);
}

#[tokio::test]
async fn channel_websocket_event_replies_through_channel_openapi() {
    let reply = run_ping_dispatch(
        "AT_MESSAGE_CREATE",
        json!({
            "id":"source-message-id",
            "content":"/ping",
            "channel_id":"channel-id",
            "author":{"id":"author-id"}
        }),
    )
    .await;
    assert_eq!(reply["content"], "pong");
    assert_eq!(reply["msg_id"], "source-message-id");
    assert!(reply.get("msg_type").is_none());
}

#[tokio::test]
async fn guild_direct_event_replies_through_dms_openapi() {
    let reply = run_ping_dispatch(
        "DIRECT_MESSAGE_CREATE",
        json!({
            "id":"source-message-id",
            "content":"/ping",
            "guild_id":"direct-guild-id",
            "channel_id":"direct-channel-id",
            "author":{"id":"author-id"}
        }),
    )
    .await;
    assert_eq!(reply["content"], "pong");
    assert_eq!(reply["msg_id"], "source-message-id");
    assert!(reply.get("msg_type").is_none());
}

#[tokio::test]
async fn echo_plugin_uses_passive_group_reply() {
    let (gateway_url, gateway_task) = start_gateway("/echo hello from plugin").await;
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (base_url, mut messages, http_task) = start_http(gateway_url, shutdown_handle).await;
    let token_endpoint = base_url.join("app/getAppAccessToken").unwrap();
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        token_endpoint,
        "app-id",
        SecretString::from("app-secret".to_owned().into_boxed_str()),
    );
    let api = OpenApiClient::with_base_url(base_url, tokens).unwrap();
    let adapter = Arc::new(QqWebSocketAdapter::new(QqWebSocketConfig::default(), api));
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    plugins
        .register_trusted(
            Arc::new(EchoPlugin::default()),
            "dev.bkm.echo/qq-websocket-test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new()
        .shutdown_timeout(Duration::from_secs(2))
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    timeout(Duration::from_secs(5), runtime.run(shutdown_signal))
        .await
        .expect("runtime should finish")
        .unwrap();
    let message = timeout(Duration::from_secs(1), messages.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message["content"], "hello from plugin");
    assert_eq!(message["msg_type"], 0);
    assert_eq!(message["msg_id"], "source-message-id");

    gateway_task.await.unwrap();
    http_task.abort();
}

#[tokio::test]
async fn active_send_probe_confirms_then_sends_without_reply_id() {
    let (gateway_url, gateway_task) = start_gateway("/active-send proactive").await;
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (base_url, mut messages, http_task) = start_http(gateway_url, shutdown_handle).await;
    let token_endpoint = base_url.join("app/getAppAccessToken").unwrap();
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        token_endpoint,
        "app-id",
        SecretString::from("app-secret".to_owned().into_boxed_str()),
    );
    let api = OpenApiClient::with_base_url(base_url, tokens).unwrap();
    let adapter = Arc::new(QqWebSocketAdapter::new(QqWebSocketConfig::default(), api));
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    plugins
        .register_trusted(
            Arc::new(ActiveSendProbePlugin::default()),
            "dev.bkm.active-send-probe/qq-websocket-test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new()
        .shutdown_timeout(Duration::from_secs(2))
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    timeout(Duration::from_secs(5), runtime.run(shutdown_signal))
        .await
        .expect("runtime should finish")
        .unwrap();
    let confirmation = timeout(Duration::from_secs(1), messages.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(confirmation["content"], "attempting proactive message");
    assert_eq!(confirmation["msg_id"], "source-message-id");
    let proactive = timeout(Duration::from_secs(1), messages.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(proactive["content"], "proactive");
    assert_eq!(proactive["msg_type"], 0);
    assert!(proactive.get("msg_id").is_none());

    gateway_task.await.unwrap();
    http_task.abort();
}
