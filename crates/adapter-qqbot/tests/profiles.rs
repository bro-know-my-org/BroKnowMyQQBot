use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
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
    profile_calls: AtomicUsize,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations.token_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token":"token","expires_in":7200}))
}

fn assert_authorization(headers: &HeaderMap) {
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("QQBot token")
    );
}

async fn bot_profile(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
) -> Json<Value> {
    assert_authorization(&headers);
    observations.profile_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "id":"bot-id",
        "username":"assistant",
        "avatar":"https://example.com/avatar.png",
        "bot":true
    }))
}

async fn group_info(
    State(observations): State<Arc<Observations>>,
    Path(group): Path<String>,
    headers: HeaderMap,
) -> Json<Value> {
    assert_authorization(&headers);
    observations.profile_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "group_openid":group,
        "group_name":"reading group",
        "group_finger_memo":"read together",
        "group_class_text":"culture",
        "group_tags":["reading"],
        "group_member_num":256
    }))
}

async fn group_bot_state(
    State(observations): State<Arc<Observations>>,
    Path(_group): Path<String>,
    headers: HeaderMap,
) -> Json<Value> {
    assert_authorization(&headers);
    observations.profile_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "member_openid":"bot-member-id",
        "joined_at":"2025-06-15T14:30:00+08:00",
        "allow_proactive_msg":false,
        "recv_msg_setting":"only_mention",
        "member_role":"member"
    }))
}

async fn adapter() -> (QqWebSocketAdapter, JoinHandle<()>, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/users/@me", get(bot_profile))
        .route("/v2/groups/{group}/info", get(group_info))
        .route("/v2/groups/{group}/bot_state", get(group_bot_state))
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
        server_task,
        observations,
    )
}

async fn platform(adapter: &QqWebSocketAdapter, name: &str, payload: Value) -> Value {
    adapter
        .execute(Action::Platform {
            name: name.to_owned(),
            payload,
        })
        .await
        .unwrap()
        .raw
}

#[tokio::test]
async fn exposes_typed_bot_and_group_profile_actions() {
    let (adapter, server_task, observations) = adapter().await;
    assert_eq!(
        platform(&adapter, "qq.bot.profile.get", json!({})).await["id"],
        "bot-id"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.group.info.get",
            json!({"group_openid":"group/id"}),
        )
        .await["group_member_num"],
        256
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.group.bot-state.get",
            json!({"group_openid":"group/id"}),
        )
        .await["recv_msg_setting"],
        "only_mention"
    );
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.profile_calls.load(Ordering::SeqCst), 3);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_blank_group_profile_action_ids_before_io() {
    let (adapter, server_task, observations) = adapter().await;
    for name in ["qq.group.info.get", "qq.group.bot-state.get"] {
        let error = adapter
            .execute(Action::Platform {
                name: name.to_owned(),
                payload: json!({"group_openid":" "}),
            })
            .await
            .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic Action error");
        };
        assert!(message.contains("`group_openid`"));
    }
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(observations.profile_calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
