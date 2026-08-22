use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
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
    requests: Mutex<Vec<(String, Value)>>,
    demand_attempts: Mutex<Vec<(String, String)>>,
    setting_authorizations: Mutex<Vec<String>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    let call = observations.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({"access_token":format!("token-{call}"),"expires_in":7200}))
}

async fn message_setting(
    State(observations): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
) -> Json<Value> {
    assert_eq!(guild_id, "guild/id");
    observations
        .setting_authorizations
        .lock()
        .unwrap()
        .push(authorization(&headers).to_owned());
    Json(json!({
        "disable_create_dm":true,
        "disable_push_msg":false,
        "channel_ids":["channel/id"],
        "channel_push_max_num":12
    }))
}

async fn guild_mute(
    State(observations): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(guild_id, "guild/id");
    assert_eq!(authorization(&headers), "QQBot token-1");
    observations
        .requests
        .lock()
        .unwrap()
        .push(("guild-mute".to_owned(), body.clone()));
    if body.get("user_ids").is_some() {
        Json(json!({"user_ids":["user/1"]})).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

async fn member_mute(
    State(observations): State<Arc<Observations>>,
    Path((guild_id, user_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild_id, "guild/id");
    assert_eq!(user_id, "user/id");
    assert_eq!(authorization(&headers), "QQBot token-1");
    observations
        .requests
        .lock()
        .unwrap()
        .push((format!("member:{user_id}"), body));
    StatusCode::NO_CONTENT
}

async fn permissions(Path(guild_id): Path<String>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(guild_id, "guild/id");
    assert_eq!(authorization(&headers), "QQBot token-1");
    Json(json!({
        "apis":[{
            "path":"/guilds/{guild_id}",
            "method":"GET",
            "desc":"获取频道",
            "auth_status":1
        }]
    }))
}

async fn demand(
    State(observations): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    observations
        .demand_attempts
        .lock()
        .unwrap()
        .push((guild_id.clone(), authorization(&headers).to_owned()));
    observations
        .requests
        .lock()
        .unwrap()
        .push(("demand".to_owned(), body.clone()));
    match guild_id.as_str() {
        "unauthorized" => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":610_014,"message":"send demand failed"})),
        )
            .into_response(),
        "redirect" => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/guilds/redirect-target/api_permission/demand")],
        )
            .into_response(),
        "server-error" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code":610_014,"message":"temporary demand failure"})),
        )
            .into_response(),
        _ => Json(json!({
        "guild_id":guild_id,
        "channel_id":body["channel_id"],
        "api_identify":body["api_identify"],
        "title":"申请接口权限",
        "desc":body["desc"]
        }))
        .into_response(),
    }
}

fn authorization(headers: &HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("request must contain authorization")
}

async fn adapter() -> (QqWebSocketAdapter, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/guilds/{guild_id}/message/setting", get(message_setting))
        .route("/guilds/{guild_id}/mute", patch(guild_mute))
        .route(
            "/guilds/{guild_id}/members/{user_id}/mute",
            patch(member_mute),
        )
        .route("/guilds/{guild_id}/api_permission", get(permissions))
        .route("/guilds/{guild_id}/api_permission/demand", post(demand))
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
    .expect("guild control Action timed out")
    .map(|result| result.raw)
}

#[tokio::test]
async fn exposes_all_guild_control_actions() {
    let (adapter, observations, server_task) = adapter().await;
    let setting = platform(
        &adapter,
        "qq.guild.message-setting.get",
        json!({"guild_id":"guild/id"}),
    )
    .await
    .unwrap();
    assert_eq!(setting["disable_create_dm"], true);

    assert_eq!(
        platform(
            &adapter,
            "qq.guild.mute.set",
            json!({"guild_id":"guild/id","mute_seconds":"120"}),
        )
        .await
        .unwrap(),
        Value::Null
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.member.mute.set",
            json!({
                "guild_id":"guild/id","user_id":"user/id",
                "mute_end_timestamp":"1641916800"
            }),
        )
        .await
        .unwrap(),
        Value::Null
    );
    let batch = platform(
        &adapter,
        "qq.guild.members.mute.set",
        json!({
            "guild_id":"guild/id","mute_seconds":"0",
            "user_ids":["user/1","user/2"]
        }),
    )
    .await
    .unwrap();
    assert_eq!(batch, json!({"user_ids":["user/1"]}));

    let permissions = platform(
        &adapter,
        "qq.guild.api-permission.list",
        json!({"guild_id":"guild/id"}),
    )
    .await
    .unwrap();
    assert_eq!(permissions["apis"][0]["auth_status"], 1);

    let demand = platform(
        &adapter,
        "qq.guild.api-permission.demand",
        json!({
            "guild_id":"guild/id",
            "channel_id":"channel/id",
            "api_identify":{"path":"/guilds/{guild_id}","method":"GET"},
            "desc":"显示频道信息"
        }),
    )
    .await
    .unwrap();
    assert_eq!(demand["title"], "申请接口权限");
    assert!(demand.get("url").is_none());

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *observations.setting_authorizations.lock().unwrap(),
        vec!["QQBot token-1".to_owned()]
    );
    assert_eq!(
        *observations.demand_attempts.lock().unwrap(),
        vec![("guild/id".to_owned(), "QQBot token-1".to_owned())]
    );
    assert_eq!(
        *observations.requests.lock().unwrap(),
        vec![
            ("guild-mute".to_owned(), json!({"mute_seconds":"120"})),
            (
                "member:user/id".to_owned(),
                json!({"mute_end_timestamp":"1641916800"})
            ),
            (
                "guild-mute".to_owned(),
                json!({"mute_seconds":"0","user_ids":["user/1","user/2"]})
            ),
            (
                "demand".to_owned(),
                json!({
                    "channel_id":"channel/id",
                    "api_identify":{"path":"/guilds/{guild_id}","method":"GET"},
                    "desc":"显示频道信息"
                })
            ),
        ]
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn preserves_open_methods_and_unbounded_mute_shapes() {
    let (adapter, observations, server_task) = adapter().await;
    let huge_timestamp = "18446744073709551616000000000000000000";
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.mute.set",
            json!({"guild_id":"guild/id","mute_end_timestamp":huge_timestamp}),
        )
        .await
        .unwrap(),
        Value::Null
    );
    let user_ids: Vec<String> = (0..500).map(|index| format!("user-{index}")).collect();
    let batch = platform(
        &adapter,
        "qq.guild.members.mute.set",
        json!({
            "guild_id":"guild/id","mute_seconds":"0","user_ids":user_ids
        }),
    )
    .await
    .unwrap();
    assert_eq!(batch, json!({"user_ids":["user/1"]}));
    for method in ["CUSTOM", "gEt"] {
        let demand = platform(
            &adapter,
            "qq.guild.api-permission.demand",
            json!({
                "guild_id":"guild/id",
                "channel_id":"channel/id",
                "api_identify":{"path":"/guilds/{guild_id}","method":method},
                "desc":"显示频道信息"
            }),
        )
        .await
        .unwrap();
        assert_eq!(demand["api_identify"]["method"], method);
    }
    {
        let requests = observations.requests.lock().unwrap();
        assert_eq!(requests[0].1["mute_end_timestamp"], huge_timestamp);
        assert_eq!(requests[1].1["user_ids"].as_array().unwrap().len(), 500);
        assert_eq!(requests[2].1["api_identify"]["method"], "CUSTOM");
        assert_eq!(requests[3].1["api_identify"]["method"], "gEt");
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn demand_action_is_never_replayed() {
    let (adapter, observations, server_task) = adapter().await;
    for (guild_id, expected) in [
        ("unauthorized", "610014"),
        ("redirect", "307"),
        ("server-error", "500"),
    ] {
        let error = platform(
            &adapter,
            "qq.guild.api-permission.demand",
            json!({
                "guild_id":guild_id,
                "channel_id":"channel/id",
                "api_identify":{"path":"/guilds/{guild_id}","method":"GET"},
                "desc":"显示频道信息"
            }),
        )
        .await
        .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic demand Action error");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }
    let setting = platform(
        &adapter,
        "qq.guild.message-setting.get",
        json!({"guild_id":"guild/id"}),
    )
    .await
    .unwrap();
    assert_eq!(setting["disable_push_msg"], false);
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observations.setting_authorizations.lock().unwrap(),
        vec!["QQBot token-2".to_owned()]
    );
    {
        let attempts = observations.demand_attempts.lock().unwrap();
        for guild_id in ["unauthorized", "redirect", "server-error"] {
            assert_eq!(
                attempts
                    .iter()
                    .filter(|(actual, _)| actual == guild_id)
                    .count(),
                1
            );
        }
        assert!(
            attempts
                .iter()
                .all(|(guild_id, _)| guild_id != "redirect-target")
        );
        assert_eq!(attempts[0].1, "QQBot token-1");
        assert!(
            attempts[1..]
                .iter()
                .all(|(_, authorization)| authorization == "QQBot token-2")
        );
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_guild_control_actions_before_io() {
    let (adapter, observations, server_task) = adapter().await;
    for (name, payload) in [
        ("qq.guild.message-setting.get", json!({"guild_id":" "})),
        (
            "qq.guild.mute.set",
            json!({"guild_id":" ","mute_seconds":"0"}),
        ),
        (
            "qq.guild.member.mute.set",
            json!({"guild_id":" ","user_id":"user","mute_seconds":"0"}),
        ),
        (
            "qq.guild.members.mute.set",
            json!({"guild_id":" ","mute_seconds":"0","user_ids":["user"]}),
        ),
        ("qq.guild.api-permission.list", json!({"guild_id":" "})),
        (
            "qq.guild.api-permission.demand",
            json!({
                "guild_id":" ","channel_id":"channel/id",
                "api_identify":{"path":"/guilds/{guild_id}","method":"GET"},
                "desc":"显示频道信息"
            }),
        ),
        ("qq.guild.mute.set", json!({"guild_id":"guild/id"})),
        (
            "qq.guild.member.mute.set",
            json!({"guild_id":"guild/id","user_id":" ","mute_seconds":"0"}),
        ),
        (
            "qq.guild.members.mute.set",
            json!({"guild_id":"guild/id","mute_seconds":"0","user_ids":[]}),
        ),
        (
            "qq.guild.api-permission.demand",
            json!({
                "guild_id":"guild/id","channel_id":"channel/id",
                "api_identify":{"path":"/guilds/{guild_id}","method":"GET POST"},
                "desc":"显示频道信息"
            }),
        ),
        (
            "qq.guild.api-permission.demand",
            json!({
                "guild_id":"guild/id","channel_id":"channel/id",
                "api_identify":{
                    "path":"/guilds/{guild_id}","method":"GET","typo":true
                },
                "desc":"显示频道信息"
            }),
        ),
        (
            "qq.guild.api-permission.list",
            json!({"guild_id":"guild/id","typo":true}),
        ),
    ] {
        assert!(matches!(
            platform(&adapter, name, payload).await,
            Err(AdapterError::Action(_))
        ));
    }
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert!(observations.requests.lock().unwrap().is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
