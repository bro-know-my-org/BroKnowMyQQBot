use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::Path,
    http::StatusCode,
    routing::{delete, get, post},
};
use bot_core::{Action, ActionResult, Adapter, AdapterError};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use url::Url;

async fn token() -> Json<Value> {
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn get_mute(Path(group): Path<String>) -> (StatusCode, Json<Value>) {
    match group.as_str() {
        "group/id" => (
            StatusCode::OK,
            Json(json!({
                "global_rule":{"mode":"none","schedule_rules":[],"recurring_rules":[]},
                "members":[]
            })),
        ),
        "forbidden" => (
            StatusCode::FORBIDDEN,
            Json(json!({"code":11281,"message":"forbidden"})),
        ),
        "server-error" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code":50000,"message":"temporary failure"})),
        ),
        _ => panic!("unexpected group path: {group}"),
    }
}

async fn set_mute(Path(group): Path<String>, Json(body): Json<Value>) -> StatusCode {
    assert_eq!(group, "group/id");
    assert!(
        [
            json!({"members":[{
                "op":"add",
                "member_openid":"member/id",
                "mute_expire_at":"2099-08-11T10:00:00Z"
            }]}),
            json!({"members":[{
                "op":"del",
                "member_openid":"member/id"
            }]}),
            json!({"members":[{
                "op":"del",
                "member_openid":"member/id",
                "mute_expire_at":""
            }]})
        ]
        .contains(&body)
    );
    StatusCode::NO_CONTENT
}

async fn list_requests(Path(group): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(group, "group/id");
    assert_eq!(body, json!({"limit":20}));
    Json(json!({"list":[],"next_cursor":""}))
}

async fn approve_request(
    Path((group, member)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(group, "group/id");
    assert_eq!(member, "member/id");
    assert_eq!(body, json!({"op":"approve","join_request_id":"join/id"}));
    StatusCode::NO_CONTENT
}

async fn create_strategy(Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body, json!({"group_openids":["group/id"],"is_enable":"on"}));
    Json(json!({
        "strategy_id":"strategy/id",
        "is_enable":"on",
        "expire_at":"2027-08-10T10:00:00Z"
    }))
}

async fn list_strategies(Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body, json!({"limit":20}));
    Json(json!({"strategies":[],"next_cursor":""}))
}

async fn update_strategy(Path(strategy): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(strategy, "strategy/id");
    assert_eq!(body, json!({"is_enable":"off"}));
    Json(json!({"is_enable":"off","expire_at":"2027-08-10T10:00:00Z"}))
}

async fn delete_strategy(Path(strategy): Path<String>) -> StatusCode {
    assert_eq!(strategy, "strategy/id");
    StatusCode::NO_CONTENT
}

async fn execute_strategy(Path(strategy): Path<String>) -> StatusCode {
    assert_eq!(strategy, "strategy/id");
    StatusCode::ACCEPTED
}

async fn update_whitelist(Path(strategy): Path<String>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(strategy, "strategy/id");
    assert_eq!(body, json!({"op":"add","whitelist_users":["123456"]}));
    Json(json!({
        "strategy_id":"strategy/id",
        "whitelist_user_count":1,
        "updated_at":"2026-08-10T10:00:00Z"
    }))
}

async fn adapter() -> (QqWebSocketAdapter, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route(
            "/v2/groups/{group}/restrict_chat_setting",
            get(get_mute).post(set_mute),
        )
        .route("/v2/groups/{group}/join_request_list", get(list_requests))
        .route(
            "/v2/groups/{group}/approval_join_request/{member}",
            post(approve_request),
        )
        .route(
            "/v2/groups/join_approval_strategy",
            get(list_strategies).post(create_strategy),
        )
        .route(
            "/v2/groups/join_approval_strategy/{strategy}",
            delete(delete_strategy).patch(update_strategy),
        )
        .route(
            "/v2/groups/join_approval_strategy/{strategy}/execute",
            post(execute_strategy),
        )
        .route(
            "/v2/groups/join_approval_strategy/{strategy}/whitelist_users",
            post(update_whitelist),
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

async fn execute(adapter: &QqWebSocketAdapter, name: &str, payload: Value) -> Value {
    execute_result(adapter, name, payload).await.unwrap().raw
}

async fn execute_result(
    adapter: &QqWebSocketAdapter,
    name: &str,
    payload: Value,
) -> Result<ActionResult, AdapterError> {
    adapter
        .execute(Action::Platform {
            name: name.to_owned(),
            payload,
        })
        .await
}

#[tokio::test]
async fn exposes_all_august_group_management_actions() {
    let (adapter, server_task) = adapter().await;

    assert_eq!(
        execute(
            &adapter,
            "qq.group.mute.get",
            json!({"group_openid":"group/id"}),
        )
        .await["global_rule"]["mode"],
        "none"
    );
    execute(
        &adapter,
        "qq.group.mute.set",
        json!({
            "group_openid":"group/id",
            "members":[{
                "op":"add",
                "member_openid":"member/id",
                "mute_expire_at":"2099-08-11T10:00:00Z"
            }]
        }),
    )
    .await;
    for member in [
        json!({"op":"del","member_openid":"member/id"}),
        json!({"op":"del","member_openid":"member/id","mute_expire_at":""}),
    ] {
        execute(
            &adapter,
            "qq.group.mute.set",
            json!({"group_openid":"group/id","members":[member]}),
        )
        .await;
    }
    execute(
        &adapter,
        "qq.group.join-request.list",
        json!({"group_openid":"group/id","limit":20}),
    )
    .await;
    execute(
        &adapter,
        "qq.group.join-request.review",
        json!({
            "group_openid":"group/id",
            "member_openid":"member/id",
            "op":"approve",
            "join_request_id":"join/id"
        }),
    )
    .await;
    assert_eq!(
        execute(
            &adapter,
            "qq.group.join-strategy.create",
            json!({"group_openids":["group/id"],"is_enable":"on"}),
        )
        .await["strategy_id"],
        "strategy/id"
    );
    execute(&adapter, "qq.group.join-strategy.list", json!({"limit":20})).await;
    execute(
        &adapter,
        "qq.group.join-strategy.update",
        json!({"strategy_id":"strategy/id","is_enable":"off"}),
    )
    .await;
    execute(
        &adapter,
        "qq.group.join-strategy.whitelist",
        json!({
            "strategy_id":"strategy/id",
            "op":"add",
            "whitelist_users":["123456"]
        }),
    )
    .await;
    execute(
        &adapter,
        "qq.group.join-strategy.execute",
        json!({"strategy_id":"strategy/id"}),
    )
    .await;
    execute(
        &adapter,
        "qq.group.join-strategy.delete",
        json!({"strategy_id":"strategy/id"}),
    )
    .await;
    server_task.abort();
}

#[tokio::test]
async fn rejects_invalid_group_actions_and_classifies_http_failures() {
    let (adapter, server_task) = adapter().await;
    let invalid_payloads = [
        (
            "qq.group.mute.get",
            json!({"group_openid":"   "}),
            "group_openid",
        ),
        (
            "qq.group.mute.set",
            json!({"group_openid":"group/id","members":[]}),
            "between 1 and 10 members",
        ),
        (
            "qq.group.join-request.list",
            json!({"group_openid":"group/id","limit":101}),
            "limit must be between 1 and 100",
        ),
        (
            "qq.group.join-request.review",
            json!({
                "group_openid":"group/id",
                "member_openid":"member/id",
                "op":"approve",
                "join_request_id":"join/id",
                "reject_reason":"not valid for approve"
            }),
            "only accepts rejection fields",
        ),
        (
            "qq.group.join-strategy.update",
            json!({"strategy_id":"strategy/id"}),
            "must contain at least one field",
        ),
        (
            "qq.group.join-strategy.whitelist",
            json!({"strategy_id":"strategy/id","op":"add","whitelist_users":["abc"]}),
            "ASCII-decimal QQ numbers",
        ),
    ];
    for (name, payload, expected) in invalid_payloads {
        let error = execute_result(&adapter, name, payload).await.unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic Action error")
        };
        assert!(message.contains("invalid QQ OpenAPI request"));
        assert!(message.contains(expected));
    }

    for (group_openid, expected_status, expected_code) in [
        ("forbidden", "403", "11281"),
        ("server-error", "500", "50000"),
    ] {
        let error = execute_result(
            &adapter,
            "qq.group.mute.get",
            json!({"group_openid":group_openid}),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            AdapterError::Action(message)
                if message.contains(expected_status) && message.contains(expected_code)
        ));
    }
    server_task.abort();
}
