use std::sync::Arc;

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bot_core::{Action, Adapter, AdapterError};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    requests: Mutex<Vec<(String, Value)>>,
}

struct TestServerTask(Option<JoinHandle<()>>);

impl TestServerTask {
    async fn abort_and_wait(mut self) {
        let task = self.0.take().expect("test server task should be present");
        task.abort();
        match task.await {
            Err(error) if error.is_cancelled() => {}
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Ok(()) => panic!("test server exited unexpectedly"),
            Err(error) => panic!("test server task failed: {error}"),
        }
    }
}

impl Drop for TestServerTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push(("auth:token".to_owned(), Value::Null));
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn audio(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("audio:{channel_id}"), body));
    Json(json!({}))
}

async fn join_mic(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
) -> StatusCode {
    observations
        .requests
        .lock()
        .await
        .push((format!("mic:join:{channel_id}"), Value::Null));
    StatusCode::NO_CONTENT
}

async fn leave_mic(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
) -> impl IntoResponse {
    observations
        .requests
        .lock()
        .await
        .push((format!("mic:leave:{channel_id}"), Value::Null));
    Json(json!({}))
}

fn thread(thread_id: &str) -> Value {
    json!({
        "guild_id":"guild/id",
        "channel_id":"channel/id",
        "author_id":"author/id",
        "thread_info":{
            "thread_id":thread_id,
            "title":[{"type":1,"text":"标题"}],
            "content":"{\"paragraphs\":[]}",
            "date_time":"2026-08-22T10:00:00+08:00"
        }
    })
}

async fn list_threads(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("thread:list:{channel_id}"), Value::Null));
    Json(json!({"threads":[thread("thread/list")],"is_finish":1}))
}

async fn create_thread(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    observations
        .requests
        .lock()
        .await
        .push((format!("thread:create:{channel_id}"), body));
    match channel_id.as_str() {
        "unauthorized" => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":610_014,"message":"unauthorized"})),
        )
            .into_response(),
        "redirect" => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/channels/redirect-target/threads")],
        )
            .into_response(),
        _ => Json(json!({"task_id":"task/id","create_time":"1645503180"})).into_response(),
    }
}

async fn get_thread(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, thread_id)): Path<(String, String)>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("thread:get:{channel_id}:{thread_id}"), Value::Null));
    Json(json!({"thread":thread(&thread_id)}))
}

async fn delete_thread(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, thread_id)): Path<(String, String)>,
) -> StatusCode {
    observations.requests.lock().await.push((
        format!("thread:delete:{channel_id}:{thread_id}"),
        Value::Null,
    ));
    StatusCode::NO_CONTENT
}

async fn invalid_list_threads() -> Json<Value> {
    let mut invalid = thread("thread/list");
    invalid["thread_info"]["date_time"] = json!("not-time");
    Json(json!({"threads":[invalid],"is_finish":1}))
}

async fn invalid_get_thread() -> Json<Value> {
    Json(json!({"thread":thread("")}))
}

async fn invalid_create_thread(Json(body): Json<Value>) -> Json<Value> {
    if body["title"] == "oversized" {
        return Json(json!({
            "task_id":"task/id","create_time":"1645503180","padding":"x".repeat(1_048_577)
        }));
    }
    Json(json!({"task_id":"","create_time":"1645503180"}))
}

async fn spawn_adapter(
    app: Router,
    observations: Arc<Observations>,
) -> (QqWebSocketAdapter, Arc<Observations>, TestServerTask) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_task = TestServerTask(Some(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    })));
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

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, TestServerTask) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/channels/{channel_id}/audio", post(audio))
        .route(
            "/channels/{channel_id}/mic",
            axum::routing::put(join_mic).delete(leave_mic),
        )
        .route(
            "/channels/{channel_id}/threads",
            get(list_threads).put(create_thread),
        )
        .route(
            "/channels/{channel_id}/threads/{thread_id}",
            get(get_thread).delete(delete_thread),
        )
        .with_state(Arc::clone(&observations));
    spawn_adapter(app, observations).await
}

async fn invalid_response_adapter() -> (QqWebSocketAdapter, TestServerTask) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route(
            "/channels/{channel_id}/threads",
            get(invalid_list_threads).put(invalid_create_thread),
        )
        .route(
            "/channels/{channel_id}/threads/{thread_id}",
            get(invalid_get_thread),
        )
        .with_state(Arc::clone(&observations));
    let (adapter, _, server_task) = spawn_adapter(app, observations).await;
    (adapter, server_task)
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
async fn exposes_all_audio_and_forum_actions() {
    let (adapter, observations, server_task) = adapter().await;

    assert_eq!(
        platform(
            &adapter,
            "qq.channel.audio.control",
            json!({
                "channel_id":"channel/id","audio_url":"https://example.com/audio.mp3",
                "text":"播放中","status":0
            }),
        )
        .await,
        Value::Null
    );
    for status in 1..=3 {
        assert_eq!(
            platform(
                &adapter,
                "qq.channel.audio.control",
                json!({"channel_id":"channel/id","status":status}),
            )
            .await,
            Value::Null
        );
    }
    for name in ["qq.channel.mic.join", "qq.channel.mic.leave"] {
        assert_eq!(
            platform(&adapter, name, json!({"channel_id":"channel/id"})).await,
            Value::Null
        );
    }
    let listed = platform(
        &adapter,
        "qq.channel.thread.list",
        json!({"channel_id":"channel/id"}),
    )
    .await;
    assert_eq!(
        listed["threads"][0]["thread_info"]["thread_id"],
        "thread/list"
    );
    assert!(listed["threads"][0]["thread_info"]["title"].is_array());
    assert_eq!(listed["is_finish"], 1);

    let detail = platform(
        &adapter,
        "qq.channel.thread.get",
        json!({"channel_id":"channel/id","thread_id":"thread/id"}),
    )
    .await;
    assert_eq!(detail["thread"]["thread_info"]["thread_id"], "thread/id");

    let task = platform(
        &adapter,
        "qq.channel.thread.create",
        json!({
            "channel_id":"channel/id","title":"标题",
            "content":"{\"paragraphs\":[]}","format":4
        }),
    )
    .await;
    assert_eq!(
        task,
        json!({"task_id":"task/id","create_time":"1645503180"})
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.thread.delete",
            json!({"channel_id":"channel/id","thread_id":"thread/id"}),
        )
        .await,
        Value::Null
    );

    assert_eq!(
        *observations.requests.lock().await,
        vec![
            ("auth:token".to_owned(), Value::Null),
            (
                "audio:channel/id".to_owned(),
                json!({
                    "audio_url":"https://example.com/audio.mp3",
                    "text":"播放中","status":0
                }),
            ),
            ("audio:channel/id".to_owned(), json!({"status":1})),
            ("audio:channel/id".to_owned(), json!({"status":2})),
            ("audio:channel/id".to_owned(), json!({"status":3})),
            ("mic:join:channel/id".to_owned(), Value::Null),
            ("mic:leave:channel/id".to_owned(), Value::Null),
            ("thread:list:channel/id".to_owned(), Value::Null),
            ("thread:get:channel/id:thread/id".to_owned(), Value::Null,),
            (
                "thread:create:channel/id".to_owned(),
                json!({"title":"标题","content":"{\"paragraphs\":[]}","format":4}),
            ),
            ("thread:delete:channel/id:thread/id".to_owned(), Value::Null,),
        ]
    );
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn rejects_invalid_audio_and_forum_actions_before_io() {
    let (adapter, observations, server_task) = adapter().await;
    let cases = [
        (
            "qq.channel.audio.control",
            json!({"channel_id":"channel","status":0}),
            "requires audio_url",
        ),
        (
            "qq.channel.audio.control",
            json!({"channel_id":"channel","audio_url":"url","status":1}),
            "only allowed when starting",
        ),
        (
            "qq.channel.audio.control",
            json!({
                "channel_id":"channel","audio_url":"file:///tmp/audio.mp3","status":0
            }),
            "HTTP(S) URL",
        ),
        (
            "qq.channel.audio.control",
            json!({"channel_id":"channel","status":4}),
            "between 0 and 3",
        ),
        (
            "qq.channel.thread.create",
            json!({"channel_id":"channel","title":"title","content":"bad","format":4}),
            "valid JSON text",
        ),
        (
            "qq.channel.thread.create",
            json!({"channel_id":"channel","title":"title","content":"content","format":5}),
            "between 1 and 4",
        ),
        (
            "qq.channel.thread.get",
            json!({"channel_id":"channel","thread_id":" "}),
            "thread_id",
        ),
        (
            "qq.channel.thread.list",
            json!({"channel_id":"channel","cursor":"invented"}),
            "unknown field",
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
            panic!("expected Action error for {name}");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }
    assert!(observations.requests.lock().await.is_empty());
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn rejects_invalid_forum_responses_at_the_adapter_boundary() {
    let (adapter, server_task) = invalid_response_adapter().await;
    for (name, payload) in [
        ("qq.channel.thread.list", json!({"channel_id":"channel/id"})),
        (
            "qq.channel.thread.get",
            json!({"channel_id":"channel/id","thread_id":"thread/id"}),
        ),
        (
            "qq.channel.thread.create",
            json!({
                "channel_id":"channel/id","title":"title","content":"content","format":1
            }),
        ),
        (
            "qq.channel.thread.create",
            json!({
                "channel_id":"channel/id","title":"oversized",
                "content":"content","format":1
            }),
        ),
    ] {
        let error = adapter
            .execute(Action::Platform {
                name: name.to_owned(),
                payload,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, AdapterError::ActionUnknown(_)),
            "invalid response for {name} should be result-unknown: {error}"
        );
    }
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn forum_publish_is_never_replayed_or_redirected() {
    let (adapter, observations, server_task) = adapter().await;
    for channel_id in ["unauthorized", "redirect"] {
        let error = adapter
            .execute(Action::Platform {
                name: "qq.channel.thread.create".to_owned(),
                payload: json!({
                    "channel_id":channel_id,"title":"title","content":"content","format":1
                }),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AdapterError::Action(_)));
    }

    let requests = observations.requests.lock().await;
    for channel_id in ["unauthorized", "redirect"] {
        assert_eq!(
            requests
                .iter()
                .filter(|(name, _)| name == &format!("thread:create:{channel_id}"))
                .count(),
            1
        );
    }
    assert!(
        requests
            .iter()
            .all(|(name, _)| name != "thread:create:redirect-target")
    );
    drop(requests);
    server_task.abort_and_wait().await;
}
