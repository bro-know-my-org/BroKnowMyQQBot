use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, Query},
    http::StatusCode,
    routing::{delete, post},
};
use bot_core::{
    Action, Adapter, AdapterError, MediaAttachment, MessageTarget, ReplyMediaAction,
    SendMediaAction, SendMessageAction,
};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

async fn token() -> Json<Value> {
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn create_session(Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(
        body,
        json!({"recipient_id":"recipient/id","source_guild_id":"source/guild"})
    );
    Json(json!({
        "guild_id":"direct/guild",
        "channel_id":"direct/channel",
        "create_time":"2099-08-10T10:00:00Z"
    }))
}

async fn send_direct_message(Path(guild): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(guild, "direct/guild");
    if body.get("markdown").is_some() {
        assert_eq!(body["markdown"]["content"], "hello");
        assert_eq!(body["msg_id"], "source-message");
    } else if body.get("ark").is_some() {
        assert_eq!(body["ark"]["template_id"], 23);
        assert!(body.get("msg_id").is_none());
    } else {
        assert_eq!(body["content"], "plain hello");
        assert!(body.get("msg_id").is_none());
    }
    Json(json!({"id":"direct-message"}))
}

async fn recall_direct_message(
    Path((guild, message)): Path<(String, String)>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> StatusCode {
    assert_eq!(guild, "direct/guild");
    assert_eq!(message, "direct-message");
    assert_eq!(query.get("hidetip").map(String::as_str), Some("false"));
    StatusCode::NO_CONTENT
}

async fn adapter() -> (QqWebSocketAdapter, JoinHandle<()>) {
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/users/@me/dms", post(create_session))
        .route("/dms/{guild}/messages", post(send_direct_message))
        .route(
            "/dms/{guild}/messages/{message}",
            delete(recall_direct_message),
        );
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
    )
}

#[tokio::test]
async fn creates_sends_and_recalls_guild_direct_messages() {
    let (adapter, server_task) = adapter().await;
    let created = adapter
        .execute(Action::Platform {
            name: "qq.dms.create".to_owned(),
            payload: json!({
                "recipient_id":"recipient/id",
                "source_guild_id":"source/guild"
            }),
        })
        .await
        .unwrap();
    assert_eq!(created.raw["guild_id"], "direct/guild");

    let sent = adapter
        .execute(Action::Platform {
            name: "qq.message.markdown".to_owned(),
            payload: json!({
                "target":{"scope":"guild_direct","guild_id":"direct/guild"},
                "body":{"content":"hello"},
                "reply_to":"source-message"
            }),
        })
        .await
        .unwrap();
    assert_eq!(sent.message_id.as_deref(), Some("direct-message"));

    let plain = adapter
        .execute(Action::SendMessage(SendMessageAction {
            target: MessageTarget::GuildDirect {
                guild_id: "direct/guild".to_owned(),
            },
            content: "plain hello".to_owned(),
        }))
        .await
        .unwrap();
    assert_eq!(plain.message_id.as_deref(), Some("direct-message"));

    let ark = adapter
        .execute(Action::Platform {
            name: "qq.message.ark".to_owned(),
            payload: json!({
                "target":{"scope":"guild_direct","guild_id":"direct/guild"},
                "body":{"template_id":23}
            }),
        })
        .await
        .unwrap();
    assert_eq!(ark.message_id.as_deref(), Some("direct-message"));

    let attachment =
        MediaAttachment::image("image/png", None, b"\x89PNG\r\n\x1a\n".to_vec()).unwrap();
    let send_media_error = adapter
        .execute(Action::SendMedia(SendMediaAction {
            target: MessageTarget::GuildDirect {
                guild_id: "direct/guild".to_owned(),
            },
            attachment: attachment.clone(),
            caption: None,
        }))
        .await
        .unwrap_err();
    assert!(
        matches!(send_media_error, AdapterError::Action(message) if message.contains("guild inline media"))
    );

    let reply_media_error = adapter
        .execute(Action::ReplyMedia(ReplyMediaAction {
            target: MessageTarget::GuildDirect {
                guild_id: "direct/guild".to_owned(),
            },
            source_message_id: "source-message".to_owned(),
            attachment,
            caption: None,
        }))
        .await
        .unwrap_err();
    assert!(
        matches!(reply_media_error, AdapterError::Action(message) if message.contains("guild inline media"))
    );

    adapter
        .execute(Action::Recall {
            target: MessageTarget::GuildDirect {
                guild_id: "direct/guild".to_owned(),
            },
            message_id: "direct-message".to_owned(),
        })
        .await
        .unwrap();

    let error = adapter
        .execute(Action::Platform {
            name: "qq.dms.create".to_owned(),
            payload: json!({"recipient_id":"","source_guild_id":"source/guild"}),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, AdapterError::Action(message) if message.contains("recipient_id")));
    server_task.abort();
    let result = server_task.await;
    assert!(result.is_err_and(|error| error.is_cancelled()));
}
