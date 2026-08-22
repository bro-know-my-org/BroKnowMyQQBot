use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use bot_core::{Action, Adapter, AdapterError, MessageTarget};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    guild_queries: Mutex<Vec<HashMap<String, String>>>,
    create_bodies: Mutex<Vec<Value>>,
    update_bodies: Mutex<Vec<Value>>,
    recall_queries: Mutex<Vec<HashMap<String, String>>>,
    direct_recall_queries: Mutex<Vec<HashMap<String, String>>>,
}

async fn token(State(state): State<Arc<Observations>>) -> Json<Value> {
    state.token_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token":"token","expires_in":7200}))
}

fn guild(id: &str) -> Value {
    json!({
        "id":id,"name":"guild","icon":"https://example.com/guild.png",
        "owner_id":"owner/id","owner":false,
        "joined_at":"2026-08-22T10:00:00Z",
        "member_count":10,"max_members":100,"description":"description",
        "future_field":{"preserved":true}
    })
}

fn channel(id: &str, name: &str) -> Value {
    json!({
        "id":id,"guild_id":"guild/id","name":name,"type":0,"sub_type":0,
        "position":1,"parent_id":"0","owner_id":"owner/id",
        "private_type":0,"speak_permission":1,"future_field":"preserved"
    })
}

async fn guilds(
    State(state): State<Arc<Observations>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    state.guild_queries.lock().unwrap().push(query);
    Json(json!([guild("guild/id")]))
}

async fn guild_detail(Path(guild_id): Path<String>) -> Json<Value> {
    Json(guild(&guild_id))
}

async fn channels(Path(guild_id): Path<String>) -> Json<Value> {
    assert_eq!(guild_id, "guild/id");
    Json(json!([channel("channel/id", "general")]))
}

async fn channel_detail(Path(channel_id): Path<String>) -> Json<Value> {
    Json(channel(&channel_id, "general"))
}

async fn create_channel(
    State(state): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(guild_id, "guild/id");
    state.create_bodies.lock().unwrap().push(body.clone());
    Json(channel(
        "channel/id",
        body["name"].as_str().unwrap_or("general"),
    ))
}

async fn update_channel(
    State(state): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.update_bodies.lock().unwrap().push(body.clone());
    Json(channel(
        &channel_id,
        body["name"].as_str().unwrap_or("general"),
    ))
}

async fn delete_channel(Path(channel_id): Path<String>) -> Json<Value> {
    assert_eq!(channel_id, "channel/id");
    Json(json!({}))
}

async fn recall(
    State(state): State<Arc<Observations>>,
    Path((channel_id, message_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> StatusCode {
    assert_eq!(channel_id, "channel/id");
    assert_eq!(message_id, "message/id");
    state.recall_queries.lock().unwrap().push(query);
    StatusCode::OK
}

async fn direct_recall(
    State(state): State<Arc<Observations>>,
    Path((guild_id, message_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> StatusCode {
    assert_eq!(guild_id, "direct/guild");
    assert_eq!(message_id, "message/id");
    state.direct_recall_queries.lock().unwrap().push(query);
    StatusCode::OK
}

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/users/@me/guilds", get(guilds))
        .route("/guilds/{guild_id}", get(guild_detail))
        .route(
            "/guilds/{guild_id}/channels",
            get(channels).post(create_channel),
        )
        .route(
            "/channels/{channel_id}",
            get(channel_detail)
                .patch(update_channel)
                .delete(delete_channel),
        )
        .route(
            "/channels/{channel_id}/messages/{message_id}",
            delete(recall),
        )
        .route(
            "/dms/{guild_id}/messages/{message_id}",
            delete(direct_recall),
        )
        .with_state(Arc::clone(&observations));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = Url::parse(&format!("http://{address}/")).unwrap();
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        base.join("app/getAppAccessToken").unwrap(),
        "app-id",
        SecretString::from("secret".to_owned().into_boxed_str()),
    );
    (
        QqWebSocketAdapter::new(
            QqWebSocketConfig::default(),
            OpenApiClient::with_base_url(base, tokens).unwrap(),
        ),
        observations,
        task,
    )
}

async fn stop_server(task: JoinHandle<()>) {
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

async fn platform(
    adapter: &QqWebSocketAdapter,
    name: &str,
    payload: Value,
) -> Result<Value, AdapterError> {
    tokio::time::timeout(
        Duration::from_secs(2),
        adapter.execute(Action::Platform {
            name: name.to_owned(),
            payload,
        }),
    )
    .await
    .expect("guild resource Action timed out")
    .map(|result| result.raw)
}

#[tokio::test]
async fn exposes_typed_guild_and_channel_read_actions() {
    let (adapter, observations, task) = adapter().await;

    let guilds = platform(
        &adapter,
        "qq.guild.list",
        json!({"after":"cursor/id","limit":20}),
    )
    .await
    .unwrap();
    assert_eq!(guilds[0]["id"], "guild/id");
    assert_eq!(guilds[0]["future_field"]["preserved"], true);
    assert_eq!(
        platform(&adapter, "qq.guild.get", json!({"guild_id":"guild/id"}))
            .await
            .unwrap()["id"],
        "guild/id"
    );
    let channels = platform(&adapter, "qq.channel.list", json!({"guild_id":"guild/id"}))
        .await
        .unwrap();
    assert_eq!(channels[0]["id"], "channel/id");
    assert_eq!(channels[0]["future_field"], "preserved");
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.get",
            json!({"channel_id":"channel/id"})
        )
        .await
        .unwrap()["name"],
        "general"
    );

    {
        let queries = observations.guild_queries.lock().unwrap();
        assert_eq!(
            queries[0].get("after").map(String::as_str),
            Some("cursor/id")
        );
        assert_eq!(queries[0].get("limit").map(String::as_str), Some("20"));
    }
    stop_server(task).await;
}

#[tokio::test]
async fn exposes_typed_channel_mutation_actions() {
    let (adapter, observations, task) = adapter().await;

    assert_eq!(
        platform(
            &adapter,
            "qq.channel.create",
            json!({
                "guild_id":"guild/id",
                "body":{
                    "name":"voice","type":2,"sub_type":3,"position":2,
                    "parent_id":"0","private_type":2,
                    "private_user_ids":["user/id"],"speak_permission":2
                }
            }),
        )
        .await
        .unwrap()["name"],
        "voice"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.create",
            json!({"guild_id":"guild/id","body":{"type":2}}),
        )
        .await
        .unwrap()["name"],
        "general"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.update",
            json!({
                "channel_id":"channel/id",
                "body":{
                    "name":"renamed","position":3,"parent_id":"group/id",
                    "private_type":1,"speak_permission":2
                }
            }),
        )
        .await
        .unwrap()["name"],
        "renamed"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.delete",
            json!({"channel_id":"channel/id"}),
        )
        .await
        .unwrap(),
        Value::Null
    );

    assert_eq!(
        observations.create_bodies.lock().unwrap()[0],
        json!({
            "name":"voice","type":2,"sub_type":3,"position":2,
            "parent_id":"0","private_type":2,
            "private_user_ids":["user/id"],"speak_permission":2
        })
    );
    assert_eq!(
        observations.create_bodies.lock().unwrap()[1],
        json!({"type":2})
    );
    assert_eq!(
        observations.update_bodies.lock().unwrap()[0],
        json!({
            "name":"renamed","position":3,"parent_id":"group/id",
            "private_type":1,"speak_permission":2
        })
    );
    stop_server(task).await;
}

#[tokio::test]
async fn recall_hide_tip_reaches_channel_and_guild_direct_endpoints() {
    let (adapter, observations, task) = adapter().await;

    tokio::time::timeout(
        Duration::from_secs(2),
        adapter.execute(Action::Recall {
            target: MessageTarget::Channel {
                channel_id: "channel/id".to_owned(),
            },
            message_id: "message/id".to_owned(),
            hide_tip: true,
        }),
    )
    .await
    .expect("channel Recall timed out")
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        adapter.execute(Action::Recall {
            target: MessageTarget::Channel {
                channel_id: "channel/id".to_owned(),
            },
            message_id: "message/id".to_owned(),
            hide_tip: false,
        }),
    )
    .await
    .expect("channel Recall timed out")
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        adapter.execute(Action::Recall {
            target: MessageTarget::GuildDirect {
                guild_id: "direct/guild".to_owned(),
            },
            message_id: "message/id".to_owned(),
            hide_tip: true,
        }),
    )
    .await
    .expect("GuildDirect Recall timed out")
    .unwrap();

    assert_eq!(
        observations.recall_queries.lock().unwrap()[0]
            .get("hidetip")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        observations.recall_queries.lock().unwrap()[1]
            .get("hidetip")
            .map(String::as_str),
        Some("false")
    );
    assert_eq!(
        observations.direct_recall_queries.lock().unwrap()[0]
            .get("hidetip")
            .map(String::as_str),
        Some("true")
    );
    stop_server(task).await;
}

#[tokio::test]
async fn rejects_invalid_action_payloads_before_authentication() {
    let (adapter, observations, task) = adapter().await;

    assert!(matches!(
        platform(
            &adapter,
            "qq.guild.list",
            json!({"before":"before","after":"after"}),
        )
        .await,
        Err(AdapterError::Action(message)) if message.contains("mutually exclusive")
    ));
    assert!(matches!(
        platform(
            &adapter,
            "qq.channel.update",
            json!({"channel_id":"channel/id","body":{"type":2}}),
        )
        .await,
        Err(AdapterError::Action(message))
            if message.contains("invalid qq.channel.update payload")
                && message.contains("unknown field `type`")
    ));
    assert!(matches!(
        platform(
            &adapter,
            "qq.channel.create",
            json!({"guild_id":"guild/id","body":{}}),
        )
        .await,
        Err(AdapterError::Action(message)) if message.contains("at least one field")
    ));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    stop_server(task).await;
}

#[tokio::test]
async fn guild_list_preserves_null_no_argument_compatibility() {
    let (adapter, observations, task) = adapter().await;

    let guilds = platform(&adapter, "qq.guild.list", Value::Null)
        .await
        .unwrap();
    assert_eq!(guilds[0]["id"], "guild/id");
    {
        let queries = observations.guild_queries.lock().unwrap();
        assert!(queries[0].is_empty());
    }
    stop_server(task).await;
}
