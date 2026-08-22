use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, atomic::AtomicUsize},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use qqbot_protocol::{
    ApiError, Channel, ChannelPrivateType, ChannelSubType, ChannelType, CreateChannelRequest,
    Guild, GuildListQuery, GuildResourceValidationError, OpenApiClient, SpeakPermission,
    TokenManager, UpdateChannelRequest,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    guild_queries: Mutex<Vec<HashMap<String, String>>>,
    writes: Mutex<Vec<(String, Value)>>,
    recalls: Mutex<Vec<HashMap<String, String>>>,
    attempts: Mutex<HashMap<String, usize>>,
    read_attempts: AtomicUsize,
}

fn record_attempt(state: &Observations, operation: &str) {
    let mut attempts = state.attempts.lock().unwrap();
    *attempts.entry(operation.to_owned()).or_default() += 1;
}

async fn token(State(state): State<Arc<Observations>>) -> Json<Value> {
    let call = state
        .token_calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Json(json!({"access_token":format!("token-{call}"),"expires_in":7200}))
}

fn guild(id: &str) -> Value {
    json!({
        "id":id,
        "name":"guild",
        "icon":"https://example.com/guild.png",
        "owner_id":"owner/id",
        "owner":true,
        "joined_at":"2026-08-22T10:00:00+08:00",
        "member_count":12,
        "max_members":100,
        "description":"guild description",
        "future_field":{"kept_by_raw_gateway_only":true}
    })
}

fn channel(id: &str, name: &str, channel_type: i64) -> Value {
    json!({
        "id":id,
        "guild_id":"guild/id",
        "name":name,
        "type":channel_type,
        "sub_type":0,
        "position":1,
        "parent_id":"0",
        "owner_id":"owner/id",
        "private_type":0,
        "speak_permission":1,
        "future_field":"ignored"
    })
}

async fn guilds(
    State(state): State<Arc<Observations>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    state.guild_queries.lock().unwrap().push(query);
    Json(json!([guild("guild/id")]))
}

async fn guild_detail(
    State(state): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if guild_id == "retry" {
        let attempt = state
            .read_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap();
        if attempt == 0 {
            assert_eq!(authorization, "QQBot token-0");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"code":11241,"message":"expired token"})),
            )
                .into_response();
        }
        assert_eq!(authorization, "QQBot token-1");
    }
    Json(guild(&guild_id)).into_response()
}

async fn channels(Path(guild_id): Path<String>) -> Json<Value> {
    assert_eq!(guild_id, "guild/id");
    Json(json!([channel("channel/id", "general", 99_999)]))
}

async fn channel_detail(Path(channel_id): Path<String>) -> Json<Value> {
    Json(channel(&channel_id, "general", 0))
}

async fn create_channel(
    State(state): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    record_attempt(&state, "create");
    if guild_id == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":11241,"message":"expired token"})),
        )
            .into_response();
    }
    assert_eq!(guild_id, "guild/id");
    state
        .writes
        .lock()
        .unwrap()
        .push(("create".to_owned(), body.clone()));
    Json(channel(
        "channel/id",
        body["name"].as_str().unwrap_or("general"),
        body["type"].as_i64().unwrap(),
    ))
    .into_response()
}

async fn update_channel(
    State(state): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    record_attempt(&state, "update");
    if channel_id == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":11241,"message":"expired token"})),
        )
            .into_response();
    }
    state
        .writes
        .lock()
        .unwrap()
        .push(("update".to_owned(), body.clone()));
    Json(channel(
        &channel_id,
        body["name"].as_str().unwrap_or("general"),
        0,
    ))
    .into_response()
}

async fn delete_channel(
    State(state): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
) -> Response {
    record_attempt(&state, "delete");
    if channel_id == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":11241,"message":"expired token"})),
        )
            .into_response();
    }
    assert_eq!(channel_id, "channel/id");
    (StatusCode::OK, Json(json!({}))).into_response()
}

async fn recall(
    State(state): State<Arc<Observations>>,
    Path((channel_id, message_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    record_attempt(&state, "recall");
    if channel_id == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":11241,"message":"expired token"})),
        )
            .into_response();
    }
    assert_eq!(channel_id, "channel/id");
    assert_eq!(message_id, "message/id");
    state.recalls.lock().unwrap().push(query);
    StatusCode::OK.into_response()
}

async fn client() -> (OpenApiClient, Arc<Observations>, JoinHandle<()>) {
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
        OpenApiClient::with_base_url(base, tokens).unwrap(),
        observations,
        task,
    )
}

async fn stop_server(task: JoinHandle<()>) {
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("guild resource operation timed out")
}

#[tokio::test]
async fn exposes_typed_guild_and_channel_resources() {
    let (client, observations, task) = client().await;

    let query = GuildListQuery {
        before: Some("guild/before".to_owned()),
        after: None,
        limit: Some(100),
    };
    let guilds = bounded(client.guilds(&query)).await.unwrap();
    assert_eq!(guilds[0].id, "guild/id");
    assert_eq!(
        guilds[0].extra()["future_field"]["kept_by_raw_gateway_only"],
        true
    );
    assert_eq!(
        bounded(client.guild("guild/id"))
            .await
            .unwrap()
            .member_count,
        12
    );

    let channels = bounded(client.guild_channels("guild/id")).await.unwrap();
    assert_eq!(channels[0].channel_type, ChannelType(99_999));
    assert!(!channels[0].channel_type.is_known());
    assert_eq!(channels[0].extra()["future_field"], "ignored");
    assert_eq!(
        bounded(client.channel("channel/id")).await.unwrap().name,
        "general"
    );

    let create = CreateChannelRequest {
        name: Some("voice".to_owned()),
        channel_type: Some(ChannelType::VOICE),
        sub_type: Some(ChannelSubType::TEAM_UP),
        position: Some(2),
        parent_id: Some("0".to_owned()),
        private_type: Some(ChannelPrivateType::SELECTED_MEMBERS),
        private_user_ids: Some(vec!["user/id".to_owned()]),
        speak_permission: Some(SpeakPermission::ADMIN_AND_SELECTED),
        application_id: None,
    };
    let created = bounded(client.create_channel("guild/id", &create))
        .await
        .unwrap();
    assert_eq!(created.channel_type, ChannelType::VOICE);

    let created_without_name = bounded(client.create_channel(
        "guild/id",
        &CreateChannelRequest {
            channel_type: Some(ChannelType::VOICE),
            ..CreateChannelRequest::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(created_without_name.name, "general");

    let update = UpdateChannelRequest {
        name: Some("renamed".to_owned()),
        position: None,
        parent_id: None,
        private_type: Some(ChannelPrivateType::PUBLIC),
        speak_permission: Some(SpeakPermission::EVERYONE),
    };
    let updated = bounded(client.update_channel("channel/id", &update))
        .await
        .unwrap();
    assert_eq!(updated.name, "renamed");

    bounded(client.delete_channel("channel/id")).await.unwrap();
    bounded(client.recall_channel_message("channel/id", "message/id", true))
        .await
        .unwrap();
    bounded(client.recall_channel_message("channel/id", "message/id", false))
        .await
        .unwrap();

    {
        let queries = observations.guild_queries.lock().unwrap();
        assert_eq!(
            queries[0].get("before").map(String::as_str),
            Some("guild/before")
        );
        assert_eq!(queries[0].get("limit").map(String::as_str), Some("100"));
        assert!(!queries[0].contains_key("after"));
    }

    {
        let writes = observations.writes.lock().unwrap();
        assert_eq!(writes[0].1["type"], 2);
        assert_eq!(writes[0].1["sub_type"], 3);
        assert_eq!(writes[0].1["private_user_ids"], json!(["user/id"]));
        assert_eq!(writes[1].1, json!({"type": 2}));
        assert!(writes[2].1.get("type").is_none());
        assert_eq!(writes[2].1["private_type"], 0);
    }

    {
        let recalls = observations.recalls.lock().unwrap();
        assert_eq!(recalls[0].get("hidetip").map(String::as_str), Some("true"));
        assert_eq!(recalls[1].get("hidetip").map(String::as_str), Some("false"));
    }
    stop_server(task).await;
}

#[tokio::test]
async fn rejects_invalid_requests_before_authentication() {
    let (client, observations, task) = client().await;

    assert!(matches!(
        client
            .guilds(&GuildListQuery {
                before: Some("before".to_owned()),
                after: Some("after".to_owned()),
                limit: None,
            })
            .await,
        Err(ApiError::InvalidGuildResourceRequest(
            GuildResourceValidationError::ConflictingGuildCursors
        ))
    ));
    assert!(matches!(
        client
            .guilds(&GuildListQuery {
                before: None,
                after: None,
                limit: Some(101),
            })
            .await,
        Err(ApiError::InvalidGuildResourceRequest(
            GuildResourceValidationError::GuildPageLimitOutOfRange { limit: 101 }
        ))
    ));
    assert!(matches!(
        client
            .guilds(&GuildListQuery {
                before: None,
                after: None,
                limit: Some(0),
            })
            .await,
        Err(ApiError::InvalidGuildResourceRequest(
            GuildResourceValidationError::GuildPageLimitOutOfRange { limit: 0 }
        ))
    ));
    assert!(matches!(
        client
            .create_channel(
                "guild/id",
                &CreateChannelRequest {
                    channel_type: Some(ChannelType(12345)),
                    ..CreateChannelRequest::default()
                },
            )
            .await,
        Err(ApiError::InvalidGuildResourceRequest(
            GuildResourceValidationError::InvalidChannelType { value: 12345 }
        ))
    ));
    assert!(matches!(
        client
            .create_channel("guild/id", &CreateChannelRequest::default())
            .await,
        Err(ApiError::InvalidGuildResourceRequest(
            GuildResourceValidationError::EmptyChannelMutation
        ))
    ));
    assert!(matches!(
        client
            .update_channel("channel/id", &UpdateChannelRequest::default())
            .await,
        Err(ApiError::InvalidGuildResourceRequest(
            GuildResourceValidationError::EmptyChannelMutation
        ))
    ));
    assert!(matches!(
        client.channel("bad\nchannel").await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert_eq!(
        observations
            .token_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    stop_server(task).await;
}

#[test]
fn request_models_enforce_conditional_fields_and_preserve_zero_values() {
    let group = CreateChannelRequest {
        channel_type: Some(ChannelType::GROUP),
        position: Some(1),
        ..CreateChannelRequest::default()
    };
    assert!(matches!(
        group.validate(),
        Err(GuildResourceValidationError::GroupPositionTooSmall { position: 1 })
    ));

    let application = CreateChannelRequest {
        channel_type: Some(ChannelType::TEXT),
        application_id: Some("app/id".to_owned()),
        ..CreateChannelRequest::default()
    };
    assert!(matches!(
        application.validate(),
        Err(GuildResourceValidationError::ApplicationIdForNonApplicationChannel)
    ));

    let public_members = CreateChannelRequest {
        private_type: Some(ChannelPrivateType::PUBLIC),
        private_user_ids: Some(vec!["user/id".to_owned()]),
        ..CreateChannelRequest::default()
    };
    assert!(matches!(
        public_members.validate(),
        Err(GuildResourceValidationError::PrivateUsersForIncompatibleChannel)
    ));

    let admin_only_members = CreateChannelRequest {
        private_type: Some(ChannelPrivateType::ADMIN_ONLY),
        private_user_ids: Some(vec!["user/id".to_owned()]),
        ..CreateChannelRequest::default()
    };
    assert!(matches!(
        admin_only_members.validate(),
        Err(GuildResourceValidationError::PrivateUsersForIncompatibleChannel)
    ));

    for private_user_ids in [None, Some(Vec::new())] {
        let selected_members = CreateChannelRequest {
            private_type: Some(ChannelPrivateType::SELECTED_MEMBERS),
            private_user_ids,
            ..CreateChannelRequest::default()
        };
        assert_eq!(selected_members.validate(), Ok(()));
    }
    assert_eq!(
        CreateChannelRequest {
            private_user_ids: Some(Vec::new()),
            ..CreateChannelRequest::default()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        CreateChannelRequest {
            application_id: Some("app/id".to_owned()),
            ..CreateChannelRequest::default()
        }
        .validate(),
        Ok(())
    );
    assert_eq!(
        CreateChannelRequest {
            private_user_ids: Some(vec!["user/id".to_owned()]),
            ..CreateChannelRequest::default()
        }
        .validate(),
        Ok(())
    );

    let zero_values = CreateChannelRequest {
        channel_type: Some(ChannelType::TEXT),
        sub_type: Some(ChannelSubType::CHAT),
        private_type: Some(ChannelPrivateType::PUBLIC),
        ..CreateChannelRequest::default()
    };
    assert_eq!(
        serde_json::to_value(zero_values).unwrap(),
        json!({"type":0,"sub_type":0,"private_type":0})
    );
}

#[test]
fn resource_validation_checks_group_positions_and_nested_guild_ownership() {
    let group: Channel = serde_json::from_value(channel("group/id", "group", 4)).unwrap();
    assert!(matches!(
        group.validate(),
        Err(GuildResourceValidationError::GroupPositionTooSmall { position: 1 })
    ));

    let mut nested = channel("channel/id", "general", 0);
    nested["guild_id"] = json!("other/guild");
    let mut value = guild("guild/id");
    value["channels"] = json!([nested]);
    let guild: Guild = serde_json::from_value(value).unwrap();
    assert!(matches!(
        guild.validate(),
        Err(GuildResourceValidationError::ChannelGuildMismatch)
    ));
}

#[tokio::test]
async fn channel_writes_deletes_and_recalls_are_not_replayed_after_unauthorized() {
    let (client, observations, task) = client().await;
    let create = CreateChannelRequest {
        name: Some("channel".to_owned()),
        channel_type: Some(ChannelType::TEXT),
        ..CreateChannelRequest::default()
    };
    let update = UpdateChannelRequest {
        name: Some("renamed".to_owned()),
        ..UpdateChannelRequest::default()
    };

    assert!(matches!(
        bounded(client.create_channel("unauthorized", &create)).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            ..
        })
    ));
    assert!(matches!(
        bounded(client.update_channel("unauthorized", &update)).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            ..
        })
    ));
    assert!(matches!(
        bounded(client.delete_channel("unauthorized")).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            ..
        })
    ));
    assert!(matches!(
        bounded(client.recall_channel_message("unauthorized", "message/id", true)).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            ..
        })
    ));

    {
        let attempts = observations.attempts.lock().unwrap();
        assert_eq!(attempts.get("create"), Some(&1));
        assert_eq!(attempts.get("update"), Some(&1));
        assert_eq!(attempts.get("delete"), Some(&1));
        assert_eq!(attempts.get("recall"), Some(&1));
    }
    stop_server(task).await;
}

#[tokio::test]
async fn guild_reads_retry_once_after_refreshing_an_unauthorized_token() {
    let (client, observations, task) = client().await;

    let guild = bounded(client.guild("retry")).await.unwrap();
    assert_eq!(guild.id, "retry");
    assert_eq!(
        observations
            .read_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(
        observations
            .token_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    stop_server(task).await;
}
