use std::sync::{Arc, Mutex};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};
use bot_core::{Action, Adapter};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations(Mutex<Vec<(String, Value)>>);

async fn token() -> Json<Value> {
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn group_message(
    State(observations): State<Arc<Observations>>,
    Path(group_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations.0.lock().unwrap().push((group_id, body));
    Json(json!({"id":"group-message"}))
}

async fn c2c_message(
    State(observations): State<Arc<Observations>>,
    Path(user_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations.0.lock().unwrap().push((user_id, body));
    Json(json!({"id":"c2c-message"}))
}

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/v2/groups/{group_id}/messages", post(group_message))
        .route("/v2/users/{user_id}/messages", post(c2c_message))
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

#[tokio::test]
async fn forwards_markdown_image_verification_for_group_and_c2c() {
    let (adapter, observations, server_task) = adapter().await;
    for (scope, id, expected_message_id) in [
        ("group", "group/id", "group-message"),
        ("private", "user/id", "c2c-message"),
    ] {
        let target = if scope == "group" {
            json!({"scope":"group","group_id":id})
        } else {
            json!({"scope":"private","user_id":id})
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            adapter.execute(Action::Platform {
                name: "qq.message.markdown".to_owned(),
                payload: json!({
                    "target":target,
                    "body":{
                        "content":"hello",
                        "force_verify_image_resource":true
                    }
                }),
            }),
        )
        .await
        .expect("Markdown action timed out")
        .unwrap();
        assert_eq!(result.message_id.as_deref(), Some(expected_message_id));
    }

    {
        let requests = observations.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for ((actual_id, body), expected_id) in requests.iter().zip(["group/id", "user/id"]) {
            assert_eq!(actual_id, expected_id);
            assert_eq!(
                body,
                &json!({
                    "msg_type":2,
                    "markdown":{
                        "content":"hello",
                        "force_verify_image_resource":true
                    }
                })
            );
        }
    }

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
