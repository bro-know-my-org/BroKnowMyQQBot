use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    http::{Method, StatusCode},
    routing::{get, post, put},
};
use bot_core::{Action, Adapter, AdapterError};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

async fn token() -> Json<Value> {
    Json(json!({"access_token":"token","expires_in":7200}))
}

#[derive(Default)]
struct Observations {
    methods: Mutex<Vec<Method>>,
    queries: Mutex<Vec<String>>,
}

async fn mutate(
    State(observations): State<Arc<Observations>>,
    method: Method,
    Path((channel, message, emoji_type, emoji_id)): Path<(String, String, u32, String)>,
) -> StatusCode {
    assert_eq!(
        (
            channel.as_str(),
            message.as_str(),
            emoji_type,
            emoji_id.as_str()
        ),
        ("channel/id", "message/id", 1, "203")
    );
    observations.methods.lock().unwrap().push(method);
    StatusCode::NO_CONTENT
}

async fn users(
    State(observations): State<Arc<Observations>>,
    Path((channel, message, emoji_type, emoji_id)): Path<(String, String, u32, String)>,
    RawQuery(query): RawQuery,
) -> Json<Value> {
    assert_eq!(
        (
            channel.as_str(),
            message.as_str(),
            emoji_type,
            emoji_id.as_str()
        ),
        ("channel/id", "message/id", 1, "203")
    );
    observations
        .queries
        .lock()
        .unwrap()
        .push(query.unwrap_or_default());
    Json(json!({
        "users":[{"id":"user/id","username":"member","avatar":"avatar"}],
        "cookie":"next",
        "is_end":false
    }))
}

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route(
            "/channels/{channel}/messages/{message}/reactions/{emoji_type}/{emoji_id}",
            put(mutate).delete(mutate).get(users),
        )
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
async fn exposes_reaction_actions() {
    let (adapter, observations, server_task) = adapter().await;
    let base = json!({
        "channel_id":"channel/id",
        "message_id":"message/id",
        "emoji_type":1,
        "emoji_id":"203"
    });
    assert_eq!(
        platform(&adapter, "qq.channel.reaction.add", base.clone()).await,
        Value::Null
    );
    assert_eq!(
        platform(&adapter, "qq.channel.reaction.remove", base.clone()).await,
        Value::Null
    );
    let mut page = base;
    page["limit"] = json!(50);
    let users = platform(&adapter, "qq.channel.reaction.users", page).await;
    assert_eq!(users["users"][0]["id"], "user/id");
    assert_eq!(users["cookie"], "next");
    assert_eq!(users["is_end"], false);
    let next = platform(
        &adapter,
        "qq.channel.reaction.users",
        json!({
            "channel_id":"channel/id",
            "message_id":"message/id",
            "emoji_type":1,
            "emoji_id":"203",
            "cookie":"next"
        }),
    )
    .await;
    assert_eq!(next["cookie"], "next");
    assert_eq!(next["is_end"], false);
    assert_eq!(
        *observations.methods.lock().unwrap(),
        vec![Method::PUT, Method::DELETE]
    );
    assert_eq!(
        *observations.queries.lock().unwrap(),
        vec!["limit=50".to_owned(), "cookie=next".to_owned()]
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

async fn counted(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
    calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn invalid_adapter() -> (QqWebSocketAdapter, Arc<AtomicUsize>, JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/app/getAppAccessToken", post(counted))
        .route(
            "/channels/{channel}/messages/{message}/reactions/{emoji_type}/{emoji_id}",
            get(counted).put(counted).delete(counted),
        )
        .with_state(Arc::clone(&calls));
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
        calls,
        server_task,
    )
}

#[tokio::test]
async fn rejects_invalid_reaction_actions_before_io() {
    let (adapter, calls, server_task) = invalid_adapter().await;
    let cases = [
        (
            "qq.channel.reaction.add",
            json!({"channel_id":" ","message_id":"message/id","emoji_type":1,"emoji_id":"203"}),
            "`channel_id`",
        ),
        (
            "qq.channel.reaction.remove",
            json!({"channel_id":"channel/id","message_id":" ","emoji_type":1,"emoji_id":"203"}),
            "`message_id`",
        ),
        (
            "qq.channel.reaction.add",
            json!({"channel_id":"channel/id","message_id":"message/id","emoji_type":3,"emoji_id":"203"}),
            "emoji type",
        ),
        (
            "qq.channel.reaction.users",
            json!({"channel_id":"channel/id","message_id":"message/id","emoji_type":1,"emoji_id":"203","limit":51}),
            "between 1 and 50",
        ),
        (
            "qq.channel.reaction.users",
            json!({"channel_id":"channel/id","message_id":"message/id","emoji_type":1,"emoji_id":"203","cookie":"next","limit":20}),
            "first request",
        ),
    ];
    for (name, payload, expected) in cases {
        let error = adapter
            .execute(Action::Platform {
                name: name.to_owned(),
                payload,
            })
            .await
            .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected Action for {name}");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }
    for payload in [
        json!({
            "channel_id":"channel/id",
            "message_id":"message/id",
            "emoji_type":1
        }),
        json!({
            "channel_id":"channel/id",
            "message_id":"message/id",
            "emoji_type":1,
            "emoji_id":"203",
            "limit":"50"
        }),
    ] {
        let error = adapter
            .execute(Action::Platform {
                name: "qq.channel.reaction.users".to_owned(),
                payload,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AdapterError::Action(_)));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
