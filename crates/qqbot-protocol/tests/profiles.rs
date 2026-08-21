use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use qqbot_protocol::{ApiError, OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

async fn token(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn bot_profile() -> Json<Value> {
    Json(json!({
        "id":"bot-id",
        "username":"assistant",
        "avatar":"https://example.com/avatar.png",
        "bot":true,
        "union_openid":"union-id",
        "union_user_account":"",
        "share_url":"https://example.com/share",
        "welcome_msg":"welcome"
    }))
}

async fn group_info(Path(group): Path<String>) -> Json<Value> {
    let mut response = json!({
        "group_openid":group,
        "group_name":"reading group",
        "group_finger_memo":"read together",
        "group_class_text":"culture",
        "group_tags":["reading","growth"],
        "group_member_num":256
    });
    if response["group_openid"] == "missing-tags" {
        response.as_object_mut().unwrap().remove("group_tags");
    }
    Json(response)
}

async fn group_bot_state(Path(group): Path<String>) -> Json<Value> {
    let mut response = json!({
        "member_openid":"bot-member-id",
        "joined_at":"2025-06-15T14:30:00+08:00",
        "allow_proactive_msg":false,
        "recv_msg_setting":"only_mention",
        "member_role":"member"
    });
    if group == "missing-role" {
        response.as_object_mut().unwrap().remove("member_role");
    } else if group == "invalid-time" {
        response["joined_at"] = json!("not-a-timestamp");
    }
    Json(response)
}

async fn client() -> (OpenApiClient, JoinHandle<()>, Arc<AtomicUsize>) {
    let token_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/users/@me", get(bot_profile))
        .route("/v2/groups/{group}/info", get(group_info))
        .route("/v2/groups/{group}/bot_state", get(group_bot_state))
        .with_state(Arc::clone(&token_calls));
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
        OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        server_task,
        token_calls,
    )
}

#[tokio::test]
async fn decodes_bot_and_group_profiles_strictly() {
    let (client, server_task, _) = client().await;
    let bot = client.bot_profile().await.unwrap();
    assert_eq!(bot.id, "bot-id");
    assert_eq!(bot.union_user_account.as_deref(), Some(""));
    assert_eq!(bot.share_url.as_deref(), Some("https://example.com/share"));

    let group = client.group_info("group/id").await.unwrap();
    assert_eq!(group.group_openid, "group/id");
    assert_eq!(group.group_tags, ["reading", "growth"]);
    assert_eq!(group.group_member_num, 256);

    let state = client.group_bot_state("group/id").await.unwrap();
    assert_eq!(state.member_openid, "bot-member-id");
    assert_eq!(state.joined_at.to_rfc3339(), "2025-06-15T14:30:00+08:00");
    assert_eq!(state.recv_msg_setting, "only_mention");
    assert_eq!(state.member_role, "member");

    assert!(matches!(
        client.group_info("missing-tags").await,
        Err(ApiError::Decode(_))
    ));
    assert!(matches!(
        client.group_bot_state("missing-role").await,
        Err(ApiError::Decode(_))
    ));
    assert!(matches!(
        client.group_bot_state("invalid-time").await,
        Err(ApiError::Decode(_))
    ));
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_blank_group_profile_ids_before_authentication() {
    let (client, server_task, token_calls) = client().await;
    assert!(matches!(
        client.group_info(" ").await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        client.group_bot_state("").await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert_eq!(token_calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
