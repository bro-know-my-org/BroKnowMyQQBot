use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use qqbot_protocol::{
    ApiError, GuildApiPermissionDemand, GuildApiPermissionDemandIdentify,
    GuildApiPermissionDemandRequest, GuildControlValidationError, GuildMembersMuteRequest,
    GuildMessageSetting, GuildMuteRequest, OpenApiClient, TokenManager,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    fail_second_token: AtomicBool,
    guild_mutes: Mutex<Vec<Value>>,
    member_mutes: Mutex<Vec<(String, Value)>>,
    demands: Mutex<Vec<(String, String, Value)>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Response {
    let call = observations.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
    if call == 2 && observations.fail_second_token.load(Ordering::SeqCst) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message":"token refresh failed"})),
        )
            .into_response();
    }
    Json(json!({"access_token":format!("token-{call}"),"expires_in":7200})).into_response()
}

async fn message_setting(Path(guild_id): Path<String>, headers: HeaderMap) -> Json<Value> {
    let expected = match guild_id.as_str() {
        "after-unauthorized" => "QQBot token-2",
        "after-failed-refresh" => "QQBot token-3",
        _ => "QQBot token-1",
    };
    assert_eq!(authorization(&headers), expected);
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
    assert_eq!(authorization(&headers), "QQBot token-1");
    observations.guild_mutes.lock().unwrap().push(body.clone());
    if body.get("user_ids").is_some() {
        if guild_id == "missing-batch" {
            Json(json!({})).into_response()
        } else {
            Json(json!({"user_ids":["user/1"]})).into_response()
        }
    } else {
        StatusCode::NO_CONTENT.into_response()
    }
}

async fn member_mute(
    State(observations): State<Arc<Observations>>,
    Path((_guild_id, user_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(authorization(&headers), "QQBot token-1");
    observations
        .member_mutes
        .lock()
        .unwrap()
        .push((user_id, body));
    StatusCode::NO_CONTENT
}

async fn api_permissions(Path(guild_id): Path<String>, headers: HeaderMap) -> Json<Value> {
    assert_eq!(authorization(&headers), "QQBot token-1");
    if guild_id == "missing-permissions" {
        return Json(json!({}));
    }
    Json(json!({
        "apis":[
            {
                "path":"/guilds/{guild_id}/members/{user_id}",
                "method":"GET",
                "desc":"获取频道成员",
                "auth_status":0
            },
            {
                "path":"/channels/{channel_id}/messages",
                "method":"POST",
                "desc":"创建消息",
                "auth_status":1
            }
        ]
    }))
}

async fn demand(
    State(observations): State<Arc<Observations>>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(authorization(&headers), "QQBot token-1");
    observations.demands.lock().unwrap().push((
        guild_id.clone(),
        authorization(&headers).to_owned(),
        body.clone(),
    ));
    if guild_id == "unauthorized" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":610_014,"message":"send demand failed"})),
        )
            .into_response();
    }
    if guild_id == "redirect" {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/guilds/redirect-target/api_permission/demand")],
        )
            .into_response();
    }
    if guild_id == "server-error" {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code":610_014,"message":"temporary demand failure"})),
        )
            .into_response();
    }
    Json(json!({
        "guild_id":guild_id,
        "channel_id":body["channel_id"],
        "api_identify":body["api_identify"],
        "title":"申请接口权限",
        "desc":body["desc"]
    }))
    .into_response()
}

fn authorization(headers: &HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("request must contain authorization")
}

async fn client() -> (OpenApiClient, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/guilds/{guild_id}/message/setting", get(message_setting))
        .route("/guilds/{guild_id}/mute", patch(guild_mute))
        .route(
            "/guilds/{guild_id}/members/{user_id}/mute",
            patch(member_mute),
        )
        .route("/guilds/{guild_id}/api_permission", get(api_permissions))
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
        OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        observations,
        server_task,
    )
}

fn demand_request() -> GuildApiPermissionDemandRequest {
    GuildApiPermissionDemandRequest {
        channel_id: "channel/id".to_owned(),
        api_identify: GuildApiPermissionDemandIdentify {
            path: "/guilds/{guild_id}".to_owned(),
            method: "GET".to_owned(),
        },
        desc: "显示频道信息".to_owned(),
    }
}

#[test]
fn validates_guild_control_models() {
    assert_eq!(
        GuildMuteRequest::default().validate().unwrap_err(),
        GuildControlValidationError::MissingMuteTiming
    );
    let both = GuildMuteRequest {
        mute_end_timestamp: Some("1641916800".to_owned()),
        mute_seconds: Some("120".to_owned()),
    };
    assert!(both.validate().is_ok());
    assert_eq!(
        serde_json::to_value(&both).unwrap(),
        json!({"mute_end_timestamp":"1641916800","mute_seconds":"120"})
    );
    let unbounded = GuildMuteRequest {
        mute_end_timestamp: Some("18446744073709551616000000000000000000".to_owned()),
        mute_seconds: None,
    };
    assert!(unbounded.validate().is_ok());
    let unmute = GuildMuteRequest {
        mute_end_timestamp: None,
        mute_seconds: Some("0".to_owned()),
    };
    assert!(unmute.validate().is_ok());
    let invalid = GuildMuteRequest {
        mute_end_timestamp: None,
        mute_seconds: Some("-1".to_owned()),
    };
    assert_eq!(
        invalid.validate().unwrap_err(),
        GuildControlValidationError::InvalidField {
            field: "mute_seconds"
        }
    );
    assert_eq!(
        GuildMembersMuteRequest {
            timing: unmute,
            user_ids: Vec::new(),
        }
        .validate()
        .unwrap_err(),
        GuildControlValidationError::EmptyUserIds
    );
    let large_batch = GuildMembersMuteRequest {
        timing: unbounded,
        user_ids: (0..500).map(|index| format!("user-{index}")).collect(),
    };
    assert!(large_batch.validate().is_ok());
    let mut formatted_description = demand_request();
    formatted_description.desc = "第一行\n\t第二行".to_owned();
    assert!(formatted_description.validate().is_ok());
    assert!(
        serde_json::from_value::<GuildApiPermissionDemandRequest>(json!({
            "channel_id":"channel/id",
            "api_identify":{
                "path":"/guilds/{guild_id}","method":"GET","typo":true
            },
            "desc":"显示频道信息"
        }))
        .is_err()
    );
    let forward_compatible_response: GuildApiPermissionDemand = serde_json::from_value(json!({
        "guild_id":"guild/id",
        "channel_id":"channel/id",
        "api_identify":{"path":"/guilds/{guild_id}","method":"GET","future":true},
        "title":"申请接口权限",
        "desc":"显示频道信息"
    }))
    .unwrap();
    assert_eq!(forward_compatible_response.api_identify.method, "GET");
    let sparse: GuildMessageSetting = serde_json::from_value(json!({})).unwrap();
    assert_eq!(sparse.disable_create_dm, None);
    assert_eq!(sparse.channel_ids, None);
}

#[tokio::test]
async fn calls_all_guild_control_endpoints() {
    let (api, observations, server_task) = client().await;
    let setting = api.guild_message_setting("guild/id").await.unwrap();
    assert_eq!(setting.disable_create_dm, Some(true));
    assert_eq!(setting.channel_push_max_num, Some(12));

    let timing = GuildMuteRequest {
        mute_end_timestamp: Some("1641916800".to_owned()),
        mute_seconds: Some("120".to_owned()),
    };
    api.set_guild_mute("guild/id", &timing).await.unwrap();
    api.set_guild_member_mute("guild/id", "user/id", &timing)
        .await
        .unwrap();
    let batch = api
        .set_guild_members_mute(
            "guild/id",
            &GuildMembersMuteRequest {
                timing: GuildMuteRequest {
                    mute_end_timestamp: None,
                    mute_seconds: Some("0".to_owned()),
                },
                user_ids: vec!["user/1".to_owned(), "user/2".to_owned()],
            },
        )
        .await
        .unwrap();
    assert_eq!(batch.user_ids, vec!["user/1"]);

    let permissions = api.guild_api_permissions("guild/id").await.unwrap();
    assert_eq!(permissions.apis.len(), 2);
    assert_eq!(permissions.apis[0].auth_status, 0);
    let demand = api
        .demand_guild_api_permission("guild/id", &demand_request())
        .await
        .unwrap();
    assert_eq!(demand.guild_id, "guild/id");
    assert_eq!(demand.api_identify.method, "GET");

    assert_eq!(
        *observations.guild_mutes.lock().unwrap(),
        vec![
            json!({"mute_end_timestamp":"1641916800","mute_seconds":"120"}),
            json!({"mute_seconds":"0","user_ids":["user/1","user/2"]}),
        ]
    );
    assert_eq!(
        *observations.member_mutes.lock().unwrap(),
        vec![(
            "user/id".to_owned(),
            json!({"mute_end_timestamp":"1641916800","mute_seconds":"120"})
        )]
    );
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn preserves_open_api_method_values() {
    let (api, observations, server_task) = client().await;
    for method in ["CUSTOM", "gEt"] {
        let mut request = demand_request();
        request.api_identify.method = method.to_owned();
        let response = api
            .demand_guild_api_permission("guild/id", &request)
            .await
            .unwrap();
        assert_eq!(response.api_identify.method, method);
    }
    {
        let demands = observations.demands.lock().unwrap();
        assert_eq!(demands[0].2["api_identify"]["method"], "CUSTOM");
        assert_eq!(demands[1].2["api_identify"]["method"], "gEt");
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn demand_is_not_replayed_after_unauthorized() {
    let (api, observations, server_task) = client().await;
    assert!(matches!(
        api.demand_guild_api_permission("unauthorized", &demand_request())
            .await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            code: Some(610_014),
            message: Some(message),
            ..
        }) if message == "send demand failed"
    ));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 2);
    let setting = api
        .guild_message_setting("after-unauthorized")
        .await
        .unwrap();
    assert_eq!(setting.disable_push_msg, Some(false));
    {
        let demands = observations.demands.lock().unwrap();
        assert_eq!(demands.len(), 1);
        assert_eq!(demands[0].0, "unauthorized");
        assert_eq!(demands[0].1, "QQBot token-1");
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn failed_refresh_invalidates_only_the_rejected_token() {
    let (api, observations, server_task) = client().await;
    observations.fail_second_token.store(true, Ordering::SeqCst);
    assert!(matches!(
        api.demand_guild_api_permission("unauthorized", &demand_request())
            .await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            code: Some(610_014),
            ..
        })
    ));
    let setting = api
        .guild_message_setting("after-failed-refresh")
        .await
        .unwrap();
    assert_eq!(setting.disable_create_dm, Some(true));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        observations
            .demands
            .lock()
            .unwrap()
            .iter()
            .filter(|(guild_id, _, _)| guild_id == "unauthorized")
            .count(),
        1
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn demand_is_never_redirected_or_retried() {
    let (api, observations, server_task) = client().await;
    for (guild_id, expected_status) in [
        ("redirect", StatusCode::TEMPORARY_REDIRECT),
        ("server-error", StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        assert!(matches!(
            api.demand_guild_api_permission(guild_id, &demand_request()).await,
            Err(ApiError::HttpStatus { status, .. }) if status == expected_status
        ));
    }
    {
        let demands = observations.demands.lock().unwrap();
        for guild_id in ["redirect", "server-error"] {
            assert_eq!(
                demands
                    .iter()
                    .filter(|(actual, _, _)| actual == guild_id)
                    .count(),
                1
            );
        }
        assert!(
            demands
                .iter()
                .all(|(guild_id, _, _)| guild_id != "redirect-target")
        );
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_missing_required_response_collections() {
    let (api, _observations, server_task) = client().await;
    let request = GuildMembersMuteRequest {
        timing: GuildMuteRequest {
            mute_end_timestamp: None,
            mute_seconds: Some("0".to_owned()),
        },
        user_ids: vec!["user/1".to_owned()],
    };
    assert!(matches!(
        api.set_guild_members_mute("missing-batch", &request).await,
        Err(ApiError::Decode(_))
    ));
    assert!(matches!(
        api.guild_api_permissions("missing-permissions").await,
        Err(ApiError::Decode(_))
    ));
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_guild_control_requests_before_io() {
    let (api, observations, server_task) = client().await;
    let valid_timing = GuildMuteRequest {
        mute_end_timestamp: None,
        mute_seconds: Some("0".to_owned()),
    };
    assert!(matches!(
        api.guild_message_setting(" ").await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        api.set_guild_mute(" ", &valid_timing).await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        api.set_guild_member_mute(" ", "user", &valid_timing).await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        api.set_guild_member_mute("guild", " ", &valid_timing).await,
        Err(ApiError::InvalidRequest(_))
    ));
    let valid_batch = GuildMembersMuteRequest {
        timing: valid_timing.clone(),
        user_ids: vec!["user".to_owned()],
    };
    assert!(matches!(
        api.set_guild_members_mute(" ", &valid_batch).await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        api.guild_api_permissions(" ").await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        api.demand_guild_api_permission(" ", &demand_request())
            .await,
        Err(ApiError::InvalidRequest(_))
    ));
    assert!(matches!(
        api.set_guild_mute("guild", &GuildMuteRequest::default())
            .await,
        Err(ApiError::InvalidGuildControlRequest(
            GuildControlValidationError::MissingMuteTiming
        ))
    ));
    assert!(matches!(
        api.set_guild_members_mute(
            "guild",
            &GuildMembersMuteRequest {
                timing: valid_timing,
                user_ids: Vec::new(),
            },
        )
        .await,
        Err(ApiError::InvalidGuildControlRequest(
            GuildControlValidationError::EmptyUserIds
        ))
    ));
    let mut invalid_demand = demand_request();
    invalid_demand.api_identify.method = "GET POST".to_owned();
    assert!(matches!(
        api.demand_guild_api_permission("guild", &invalid_demand)
            .await,
        Err(ApiError::InvalidGuildControlRequest(
            GuildControlValidationError::InvalidField {
                field: "api_identify.method"
            }
        ))
    ));
    let mut invalid_path = demand_request();
    invalid_path.api_identify.path = "guilds/{guild_id}".to_owned();
    assert!(matches!(
        api.demand_guild_api_permission("guild", &invalid_path)
            .await,
        Err(ApiError::InvalidGuildControlRequest(
            GuildControlValidationError::InvalidField {
                field: "api_identify.path"
            }
        ))
    ));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert!(observations.guild_mutes.lock().unwrap().is_empty());
    assert!(observations.demands.lock().unwrap().is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
