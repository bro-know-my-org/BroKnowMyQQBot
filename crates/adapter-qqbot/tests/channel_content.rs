use std::sync::Arc;

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, RawQuery, State},
    http::StatusCode,
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

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push(("auth:token".to_owned(), Value::Null));
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn create_announcement(
    State(observations): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("announce:create:{guild_id}"), body.clone()));
    Json(json!({
        "guild_id":guild_id,
        "channel_id":body.get("channel_id").cloned().unwrap_or(json!("")),
        "message_id":body.get("message_id").cloned().unwrap_or(json!("")),
        "announces_type":body["announces_type"],
        "recommend_channels":body.get("recommend_channels").cloned().unwrap_or(json!([]))
    }))
}

async fn delete_announcement(
    State(observations): State<Arc<Observations>>,
    Path((guild_id, message_id)): Path<(String, String)>,
) -> StatusCode {
    observations.requests.lock().await.push((
        format!("announce:delete:{guild_id}:{message_id}"),
        Value::Null,
    ));
    StatusCode::NO_CONTENT
}

async fn list_pins(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("pin:list:{channel_id}"), Value::Null));
    Json(json!({
        "guild_id":"guild/id",
        "channel_id":channel_id,
        "message_ids":["message/id"]
    }))
}

async fn add_pin(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, message_id)): Path<(String, String)>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("pin:add:{channel_id}:{message_id}"), Value::Null));
    Json(json!({
        "guild_id":"guild/id",
        "channel_id":channel_id,
        "message_ids":[message_id]
    }))
}

async fn delete_pin(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, message_id)): Path<(String, String)>,
) -> StatusCode {
    observations
        .requests
        .lock()
        .await
        .push((format!("pin:delete:{channel_id}:{message_id}"), Value::Null));
    StatusCode::NO_CONTENT
}

fn schedule(schedule_id: &str, name: &str) -> Value {
    json!({
        "id":schedule_id,
        "name":name,
        "start_timestamp":"1784279600000",
        "end_timestamp":"1784283200000",
        "jump_channel_id":"0",
        "remind_type":"0"
    })
}

async fn list_schedules(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Json<Value> {
    observations.requests.lock().await.push((
        format!("schedule:list:{channel_id}"),
        json!(query.unwrap_or_default()),
    ));
    let mut listed = schedule("schedule/list", "列表日程");
    listed["remind_type"] = json!("256");
    Json(json!([listed]))
}

async fn create_schedule(
    State(observations): State<Arc<Observations>>,
    Path(channel_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("schedule:create:{channel_id}"), body.clone()));
    Json(schedule(
        "schedule/created",
        body["schedule"]["name"].as_str().unwrap(),
    ))
}

async fn get_schedule(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, schedule_id)): Path<(String, String)>,
) -> Json<Value> {
    observations.requests.lock().await.push((
        format!("schedule:get:{channel_id}:{schedule_id}"),
        Value::Null,
    ));
    Json(schedule(&schedule_id, "详情日程"))
}

async fn update_schedule(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, schedule_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations.requests.lock().await.push((
        format!("schedule:update:{channel_id}:{schedule_id}"),
        body.clone(),
    ));
    Json(schedule(
        &schedule_id,
        body["schedule"]["name"].as_str().unwrap_or("更新日程"),
    ))
}

async fn delete_schedule(
    State(observations): State<Arc<Observations>>,
    Path((channel_id, schedule_id)): Path<(String, String)>,
) -> StatusCode {
    observations.requests.lock().await.push((
        format!("schedule:delete:{channel_id}:{schedule_id}"),
        Value::Null,
    ));
    StatusCode::NO_CONTENT
}

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/guilds/{guild_id}/announces", post(create_announcement))
        .route(
            "/guilds/{guild_id}/announces/{message_id}",
            axum::routing::delete(delete_announcement),
        )
        .route("/channels/{channel_id}/pins", get(list_pins))
        .route(
            "/channels/{channel_id}/pins/{message_id}",
            axum::routing::put(add_pin).delete(delete_pin),
        )
        .route(
            "/channels/{channel_id}/schedules",
            get(list_schedules).post(create_schedule),
        )
        .route(
            "/channels/{channel_id}/schedules/{schedule_id}",
            get(get_schedule)
                .patch(update_schedule)
                .delete(delete_schedule),
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
#[allow(clippy::too_many_lines)]
async fn exposes_all_announcement_pin_and_schedule_actions() {
    let (adapter, observations, server_task) = adapter().await;

    let announcement = platform(
        &adapter,
        "qq.guild.announce.create",
        json!({
            "guild_id":"guild/id",
            "message_id":"message/id",
            "channel_id":"channel/id",
            "announces_type":0
        }),
    )
    .await;
    assert_eq!(announcement["message_id"], "message/id");
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.announce.delete",
            json!({"guild_id":"guild/id","message_id":"message/id"}),
        )
        .await,
        Value::Null
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.announce.clear",
            json!({"guild_id":"guild/id"}),
        )
        .await,
        Value::Null
    );

    let pins = platform(
        &adapter,
        "qq.channel.pin.add",
        json!({"channel_id":"channel/id","message_id":"message/id"}),
    )
    .await;
    assert_eq!(pins["message_ids"], json!(["message/id"]));
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.pin.delete",
            json!({"channel_id":"channel/id","message_id":"message/id"}),
        )
        .await,
        Value::Null
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.pin.clear",
            json!({"channel_id":"channel/id"}),
        )
        .await,
        Value::Null
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.pin.list",
            json!({"channel_id":"channel/id"}),
        )
        .await["channel_id"],
        "channel/id"
    );

    let listed = platform(
        &adapter,
        "qq.channel.schedule.list",
        json!({"channel_id":"channel/id","since":1_784_279_600_000_u64}),
    )
    .await;
    assert_eq!(listed[0]["id"], "schedule/list");
    assert_eq!(listed[0]["start_timestamp"], "1784279600000");
    assert_eq!(listed[0]["end_timestamp"], "1784283200000");
    assert_eq!(listed[0]["remind_type"], "256");
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.schedule.get",
            json!({"channel_id":"channel/id","schedule_id":"schedule/id"}),
        )
        .await["id"],
        "schedule/id"
    );
    let created = platform(
        &adapter,
        "qq.channel.schedule.create",
        json!({
            "channel_id":"channel/id",
            "schedule":{
                "name":"创建日程",
                "start_timestamp":"1784279600000",
                "end_timestamp":"1784283200000",
                "jump_channel_id":"0",
                "remind_type":"0"
            }
        }),
    )
    .await;
    assert_eq!(created["id"], "schedule/created");
    let updated = platform(
        &adapter,
        "qq.channel.schedule.update",
        json!({
            "channel_id":"channel/id",
            "schedule_id":"schedule/id",
            "schedule":{"name":"更新日程"}
        }),
    )
    .await;
    assert_eq!(updated["name"], "更新日程");
    assert_eq!(
        platform(
            &adapter,
            "qq.channel.schedule.delete",
            json!({"channel_id":"channel/id","schedule_id":"schedule/id"}),
        )
        .await,
        Value::Null
    );

    let requests = observations.requests.lock().await.clone();
    assert_eq!(
        requests,
        vec![
            ("auth:token".to_owned(), Value::Null),
            (
                "announce:create:guild/id".to_owned(),
                json!({
                    "message_id":"message/id",
                    "channel_id":"channel/id",
                    "announces_type":0
                }),
            ),
            (
                "announce:delete:guild/id:message/id".to_owned(),
                Value::Null,
            ),
            ("announce:delete:guild/id:all".to_owned(), Value::Null),
            ("pin:add:channel/id:message/id".to_owned(), Value::Null),
            ("pin:delete:channel/id:message/id".to_owned(), Value::Null,),
            ("pin:delete:channel/id:all".to_owned(), Value::Null),
            ("pin:list:channel/id".to_owned(), Value::Null),
            (
                "schedule:list:channel/id".to_owned(),
                json!("since=1784279600000"),
            ),
            (
                "schedule:get:channel/id:schedule/id".to_owned(),
                Value::Null,
            ),
            (
                "schedule:create:channel/id".to_owned(),
                json!({
                    "schedule":{
                        "name":"创建日程",
                        "start_timestamp":"1784279600000",
                        "end_timestamp":"1784283200000",
                        "jump_channel_id":"0",
                        "remind_type":"0"
                    }
                }),
            ),
            (
                "schedule:update:channel/id:schedule/id".to_owned(),
                json!({"schedule":{"name":"更新日程"}}),
            ),
            (
                "schedule:delete:channel/id:schedule/id".to_owned(),
                Value::Null,
            ),
        ]
    );

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn preserves_official_empty_announcement_compatibility_fields() {
    let (adapter, observations, server_task) = adapter().await;
    let member = platform(
        &adapter,
        "qq.guild.announce.create",
        json!({
            "guild_id":"guild/id",
            "message_id":"message/id",
            "channel_id":"channel/id",
            "announces_type":0,
            "recommend_channels":[]
        }),
    )
    .await;
    let welcome = platform(
        &adapter,
        "qq.guild.announce.create",
        json!({
            "guild_id":"guild/id",
            "message_id":"",
            "channel_id":"",
            "announces_type":1,
            "recommend_channels":[{
                "channel_id":"recommended/id",
                "introduce":"推荐子频道"
            }]
        }),
    )
    .await;

    assert_eq!(
        member,
        json!({
            "guild_id":"guild/id",
            "channel_id":"channel/id",
            "message_id":"message/id",
            "announces_type":0,
            "recommend_channels":[]
        })
    );
    assert_eq!(
        welcome,
        json!({
            "guild_id":"guild/id",
            "channel_id":"",
            "message_id":"",
            "announces_type":1,
            "recommend_channels":[{
                "channel_id":"recommended/id",
                "introduce":"推荐子频道"
            }]
        })
    );

    assert_eq!(
        *observations.requests.lock().await,
        vec![
            ("auth:token".to_owned(), Value::Null),
            (
                "announce:create:guild/id".to_owned(),
                json!({
                    "message_id":"message/id",
                    "channel_id":"channel/id",
                    "announces_type":0,
                    "recommend_channels":[]
                }),
            ),
            (
                "announce:create:guild/id".to_owned(),
                json!({
                    "message_id":"",
                    "channel_id":"",
                    "announces_type":1,
                    "recommend_channels":[{
                        "channel_id":"recommended/id",
                        "introduce":"推荐子频道"
                    }]
                }),
            ),
        ]
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_content_actions_before_openapi_io() {
    let (adapter, observations, server_task) = adapter().await;
    let cases = [
        (
            "qq.guild.announce.create",
            json!({"guild_id":"guild","announces_type":0}),
            "message_id",
        ),
        (
            "qq.guild.announce.delete",
            json!({"guild_id":"guild","message_id":"all"}),
            "reserved value",
        ),
        (
            "qq.channel.pin.add",
            json!({"channel_id":"channel","message_id":"all"}),
            "reserved value",
        ),
        (
            "qq.channel.schedule.create",
            json!({
                "channel_id":"channel",
                "schedule":{
                    "name":"activity",
                    "start_timestamp":"2000",
                    "end_timestamp":"1000",
                    "remind_type":"0"
                }
            }),
            "end before",
        ),
        (
            "qq.channel.schedule.update",
            json!({"channel_id":"channel","schedule_id":"schedule","schedule":{}}),
            "at least one field",
        ),
        (
            "qq.channel.schedule.update",
            json!({
                "channel_id":"channel",
                "schedule_id":"schedule",
                "schedule":{"remind_type":"6"}
            }),
            "remind type must be between 0 and 5",
        ),
        (
            "qq.channel.schedule.create",
            json!({
                "channel_id":"channel",
                "schedule":{
                    "name":"activity",
                    "start_timestamp":"１２３",
                    "end_timestamp":"1784283200000",
                    "remind_type":"0"
                }
            }),
            "unsigned decimal Unix epoch millisecond",
        ),
        (
            "qq.channel.schedule.list",
            json!({"channel_id":"channel","since":1,"future":true}),
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
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
