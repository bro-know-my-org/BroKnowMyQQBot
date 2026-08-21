use std::{collections::HashMap, sync::Arc};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
};
use bot_core::{Action, Adapter, AdapterError};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    api_calls: AtomicUsize,
    requests: Mutex<Vec<(String, Value)>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations.token_calls.fetch_add(1, Ordering::Relaxed);
    Json(json!({"access_token":"token","expires_in":7200}))
}

fn observe_authorized(observations: &Observations, headers: &HeaderMap) {
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("QQBot token")
    );
    observations.api_calls.fetch_add(1, Ordering::Relaxed);
}

async fn list_panels(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    observe_authorized(&observations, &headers);
    observations
        .requests
        .lock()
        .await
        .push(("list".to_owned(), json!(query)));
    Json(json!({
        "records":[],
        "next_cursor":"",
        "is_end":true
    }))
}

async fn create_panel(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    observe_authorized(&observations, &headers);
    observations
        .requests
        .lock()
        .await
        .push(("create".to_owned(), body));
    Json(json!({"panel_id":"panel-created"}))
}

async fn get_panel(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Path(panel_id): Path<String>,
) -> Json<Value> {
    observe_authorized(&observations, &headers);
    Json(json!({
        "panel_id":panel_id,
        "scope":"c2c",
        "target_type":"specific",
        "panel":{"items":[],"remark":"detail"},
        "version":3,
        "user_openids":["user-1"],
        "group_openids":[]
    }))
}

async fn update_panel(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Path(panel_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observe_authorized(&observations, &headers);
    observations
        .requests
        .lock()
        .await
        .push((format!("update:{panel_id}"), body));
    Json(json!({"version":4}))
}

async fn delete_panel(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Path(panel_id): Path<String>,
) -> StatusCode {
    observe_authorized(&observations, &headers);
    observations
        .requests
        .lock()
        .await
        .push((format!("delete:{panel_id}"), json!(null)));
    StatusCode::NO_CONTENT
}

async fn update_targets(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Path(panel_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    observe_authorized(&observations, &headers);
    observations
        .requests
        .lock()
        .await
        .push((format!("target:{panel_id}"), body));
    if panel_id == "panel-empty" {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Json(json!({})).into_response()
    }
}

async fn adapter() -> (QqWebSocketAdapter, JoinHandle<()>, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/v2/panels", get(list_panels).post(create_panel))
        .route(
            "/v2/panels/{panel_id}",
            get(get_panel).put(update_panel).delete(delete_panel),
        )
        .route("/v2/panels/{panel_id}/target", put(update_targets))
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
        server_task,
        observations,
    )
}

async fn platform(adapter: &QqWebSocketAdapter, name: &str, payload: Value) -> Value {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        adapter.execute(Action::Platform {
            name: name.to_owned(),
            payload,
        }),
    )
    .await
    .expect("panel action timed out")
    .unwrap()
    .raw
}

fn assert_boundary_payloads(requests: &[(String, Value)]) {
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].0, "list");
    assert_eq!(requests[0].1["scope"], "group");
    assert_eq!(requests[0].1["cursor"], "cursor-1");
    assert_eq!(requests[0].1["limit"], "50");
    assert_eq!(requests[1].0, "create");
    assert_eq!(requests[1].1["scope"], "c2c");
    assert_eq!(requests[1].1["target_type"], "specific");
    assert_eq!(requests[1].1["panel"]["remark"], "action panel");
    assert_eq!(requests[1].1["user_openids"].as_array().unwrap().len(), 20);
    assert_eq!(
        requests[1].1["panel"]["items"][0],
        json!({
            "name":"命令0","desc":"执行命令","type":"command","only_admin":false
        })
    );
    assert_eq!(requests[2].0, "update:panel/id");
    assert_eq!(requests[2].1["panel"]["remark"], "action panel");
    assert_eq!(
        requests[2].1["panel"]["items"].as_array().unwrap().len(),
        20
    );
    assert_eq!(requests[2].1["panel"]["items"][0]["name"], "命令0");
    assert_eq!(requests[3].0, "target:panel/id");
    assert_eq!(requests[3].1["op"], "add");
    assert_eq!(requests[3].1["user_openids"].as_array().unwrap().len(), 20);
    assert_eq!(requests[4].0, "target:panel-empty");
    assert_eq!(requests[4].1, json!({"op":"del","user_openids":["user-1"]}));
    assert_eq!(requests[5].0, "delete:panel/id");
}

#[tokio::test]
async fn exposes_all_panel_actions() {
    let (adapter, server_task, observations) = adapter().await;
    let boundary_openids = (0..20)
        .map(|index| format!("user-{index}"))
        .collect::<Vec<_>>();
    let boundary_items = (0..20)
        .map(|index| {
            json!({
                "name":format!("命令{index}"),
                "desc":"执行命令",
                "type":"command",
                "only_admin":false
            })
        })
        .collect::<Vec<_>>();

    assert!(
        platform(
            &adapter,
            "qq.bot.panel.list",
            json!({"scope":"group","cursor":"cursor-1","limit":50}),
        )
        .await["is_end"]
            .as_bool()
            .is_some_and(|is_end| is_end)
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.create",
            json!({
                "scope":"c2c","target_type":"specific",
                "user_openids":boundary_openids.clone(),
                "panel":{"items":boundary_items.clone(),"remark":"action panel"}
            }),
        )
        .await["panel_id"],
        "panel-created"
    );
    let detail = platform(&adapter, "qq.bot.panel.get", json!({"panel_id":"panel/id"})).await;
    assert_eq!(detail["panel_id"], "panel/id");
    assert_eq!(detail["user_openids"][0], "user-1");
    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.update",
            json!({
                "panel_id":"panel/id",
                "panel":{"items":boundary_items,"remark":"action panel"}
            }),
        )
        .await["version"],
        4
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.target.update",
            json!({
                "panel_id":"panel/id","op":"add",
                "user_openids":boundary_openids
            }),
        )
        .await,
        Value::Null
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.target.update",
            json!({"panel_id":"panel-empty","op":"del","user_openids":["user-1"]}),
        )
        .await,
        Value::Null
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.delete",
            json!({"panel_id":"panel/id"}),
        )
        .await,
        Value::Null
    );

    assert_eq!(observations.token_calls.load(Ordering::Relaxed), 1);
    assert_eq!(observations.api_calls.load(Ordering::Relaxed), 7);
    let requests = observations.requests.lock().await;
    assert_boundary_payloads(&requests);
    drop(requests);

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn creates_specific_panels_without_initial_targets() {
    let (adapter, server_task, observations) = adapter().await;

    for scope in ["c2c", "group"] {
        assert_eq!(
            platform(
                &adapter,
                "qq.bot.panel.create",
                json!({"scope":scope,"target_type":"specific","panel":{"items":[]}}),
            )
            .await["panel_id"],
            "panel-created"
        );
    }

    assert_eq!(observations.token_calls.load(Ordering::Relaxed), 1);
    assert_eq!(observations.api_calls.load(Ordering::Relaxed), 2);
    let requests = observations.requests.lock().await;
    assert_eq!(requests.len(), 2);
    for ((_, body), expected_scope) in requests.iter().zip(["c2c", "group"]) {
        assert_eq!(body["scope"], expected_scope);
        assert_eq!(body["target_type"], "specific");
        assert!(!body.as_object().unwrap().contains_key("user_openids"));
        assert!(!body.as_object().unwrap().contains_key("group_openids"));
    }

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn forwards_group_openids_for_create_and_target_update() {
    let (adapter, server_task, observations) = adapter().await;

    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.create",
            json!({
                "scope":"group","target_type":"specific",
                "group_openids":["group-1"],"panel":{"items":[]}
            }),
        )
        .await["panel_id"],
        "panel-created"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.bot.panel.target.update",
            json!({"panel_id":"panel-group","op":"add","group_openids":["group-1"]}),
        )
        .await,
        Value::Null
    );

    assert_eq!(observations.token_calls.load(Ordering::Relaxed), 1);
    assert_eq!(observations.api_calls.load(Ordering::Relaxed), 2);
    let requests = observations.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0, "create");
    assert_eq!(requests[0].1["scope"], "group");
    assert_eq!(requests[0].1["group_openids"], json!(["group-1"]));
    assert_eq!(requests[1].0, "target:panel-group");
    assert_eq!(
        requests[1].1,
        json!({"op":"add","group_openids":["group-1"]})
    );

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_panel_actions_before_io() {
    let (adapter, server_task, observations) = adapter().await;
    let too_many_openids = (0..21)
        .map(|index| format!("user-{index}"))
        .collect::<Vec<_>>();
    let too_many_items = (0..21)
        .map(|index| {
            json!({
                "name":format!("命令{index}"),
                "desc":"执行命令",
                "type":"command",
                "only_admin":false
            })
        })
        .collect::<Vec<_>>();
    let cases = vec![
        (
            "qq.bot.panel.list",
            json!({"scope":"c2c","limit":0}),
            "limit",
        ),
        (
            "qq.bot.panel.create",
            json!({"scope":"dm","target_type":"specific","panel":{}}),
            "does not support",
        ),
        (
            "qq.bot.panel.update",
            json!({"panel_id":"panel-1","panel":{"items":[{
                "name":"官网","desc":"打开官网","type":"link","only_admin":false,
                "link":"http://example.com"
            }]}}),
            "HTTPS",
        ),
        ("qq.bot.panel.get", json!({"panel_id":" "}), "`panel_id`"),
        ("qq.bot.panel.delete", json!({"panel_id":""}), "`panel_id`"),
        (
            "qq.bot.panel.target.update",
            json!({"panel_id":"panel-1","op":"del",
                   "user_openids":["user-1"],"group_openids":["group-1"]}),
            "both user_openids and group_openids",
        ),
        (
            "qq.bot.panel.target.update",
            json!({"panel_id":"panel-1","op":"add","user_openids":["user 1"]}),
            "whitespace or control characters",
        ),
        (
            "qq.bot.panel.target.update",
            json!({"panel_id":"panel-1","op":"add","user_openids":too_many_openids}),
            "more than 20 entries",
        ),
        (
            "qq.bot.panel.update",
            json!({"panel_id":"panel-1","panel":{"items":too_many_items}}),
            "more than 20 items",
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
            panic!("expected deterministic panel Action error");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }

    assert_eq!(observations.token_calls.load(Ordering::Relaxed), 0);
    assert_eq!(observations.api_calls.load(Ordering::Relaxed), 0);
    assert!(observations.requests.lock().await.is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
