use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
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
    token_calls: AtomicUsize,
    api_calls: AtomicUsize,
    bodies: Mutex<Vec<Value>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations.token_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token":"token","expires_in":7200}))
}

fn assert_authorization(headers: &HeaderMap) {
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("QQBot token")
    );
}

async fn generate_share_link(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_authorization(&headers);
    observations.api_calls.fetch_add(1, Ordering::SeqCst);
    observations.bodies.lock().await.push(body);
    Json(json!({"url_link":"https://qun.qq.com/share-link"}))
}

async fn get_menu(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
) -> Json<Value> {
    assert_authorization(&headers);
    observations.api_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "version":3,
        "menu":null,
        "future_field":{"enabled":true}
    }))
}

async fn update_menu(
    State(observations): State<Arc<Observations>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_authorization(&headers);
    observations.api_calls.fetch_add(1, Ordering::SeqCst);
    observations.bodies.lock().await.push(body);
    Json(json!({"version":4}))
}

async fn adapter() -> (QqWebSocketAdapter, JoinHandle<()>, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/v2/generate_url_link", post(generate_share_link))
        .route("/v2/menu", get(get_menu).put(update_menu))
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
async fn exposes_share_link_action_boundaries() {
    let (adapter, server_task, observations) = adapter().await;

    for payload in [
        json!({"callback_data":"campaign-1"}),
        json!({"callback_data":"x".repeat(32)}),
        json!({"callback_data":"中".repeat(32)}),
        json!({}),
    ] {
        assert_eq!(
            platform(&adapter, "qq.bot.share-link.create", payload).await["url_link"],
            "https://qun.qq.com/share-link"
        );
    }

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 4);
    let bodies = observations.bodies.lock().await;
    assert_eq!(bodies[0], json!({"callback_data":"campaign-1"}));
    assert_eq!(bodies[1], json!({"callback_data":"x".repeat(32)}));
    assert_eq!(bodies[2], json!({"callback_data":"中".repeat(32)}));
    assert_eq!(bodies[3], json!({}));
    drop(bodies);

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn exposes_custom_menu_action_wire_shapes() {
    let (adapter, server_task, observations) = adapter().await;

    assert_eq!(
        platform(&adapter, "qq.bot.menu.get", json!({})).await,
        json!({"version":3,"menu":null})
    );
    let update = json!({"menu":{"items":[
        {"name":"帮助","type":"send_message","send_message":"/help"},
        {"name":"官网","type":"link","link":"https://example.com"},
        {"name":"开关","type":"switch","switch":{"switch_id":"notifications","default":true}},
        {"name":"更多","type":"menu","sub_menu_items":[
            {"name":"命令","type":"send_message","send_message":"/commands"},
            {"name":"文档","type":"link","link":"https://example.com/docs"}
        ]}
    ]}});
    assert_eq!(
        platform(&adapter, "qq.bot.menu.update", update.clone(),).await["version"],
        4
    );
    assert_eq!(
        platform(&adapter, "qq.bot.menu.update", json!({"menu":{"items":[]}}),).await["version"],
        4
    );
    let ten_items = json!({"menu":{"items":vec![
        json!({"name":"12345678中","type":"send_message","send_message":"/help"}); 10
    ]}});
    platform(&adapter, "qq.bot.menu.update", ten_items.clone()).await;
    let empty_nested = json!({"menu":{"items":[{
        "name":"更多","type":"menu","sub_menu_items":[]
    }]}});
    platform(&adapter, "qq.bot.menu.update", empty_nested.clone()).await;
    let five_nested = json!({"menu":{"items":[{
        "name":"更多","type":"menu","sub_menu_items":vec![
            json!({"name":"123456789012中","type":"link","link":"https://example.com"}); 5
        ]
    }]}});
    platform(&adapter, "qq.bot.menu.update", five_nested.clone()).await;

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 6);
    let bodies = observations.bodies.lock().await;
    assert_eq!(bodies[0], update);
    assert_eq!(bodies[1], json!({"menu":{"items":[]}}));
    assert_eq!(bodies[2], ten_items);
    assert_eq!(bodies[3], empty_nested);
    assert_eq!(bodies[4], five_nested);
    drop(bodies);

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_share_link_action_before_io() {
    let (adapter, server_task, observations) = adapter().await;
    for callback_data in ["x".repeat(33), "中".repeat(33)] {
        let error = adapter
            .execute(Action::Platform {
                name: "qq.bot.share-link.create".to_owned(),
                payload: json!({"callback_data":callback_data}),
            })
            .await
            .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic Action error");
        };
        assert!(message.contains("share-link"));
        assert!(message.contains("33"));
    }
    let error = adapter
        .execute(Action::Platform {
            name: "qq.bot.share-link.create".to_owned(),
            payload: json!({"callback_data":"ok","unexpected":true}),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown field `unexpected`"));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the declarative validation matrix is clearer when kept in one zero-I/O test"
)]
async fn rejects_invalid_custom_menu_actions_before_io() {
    let (adapter, server_task, observations) = adapter().await;

    let mut invalid_cases = vec![
        (
            "qq.bot.menu.get",
            json!(null),
            "expected an empty JSON object",
        ),
        (
            "qq.bot.menu.get",
            json!({"unexpected":true}),
            "expected an empty JSON object",
        ),
        ("qq.bot.menu.update", json!({}), "must contain menu"),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[]},"unexpected":true}),
            "unknown field `unexpected` at payload",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[],"unexpected":true}}),
            "unknown field `unexpected` at menu",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"帮助","type":"send_message","send_message":"/help",
                "unexpected":true
            }]}}),
            "unknown field `unexpected` at menu.items[0]",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"开关","type":"switch",
                "switch":{"switch_id":"notifications","default":true,"unexpected":true}
            }]}}),
            "unknown field `unexpected` at menu.items[0].switch",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"更多","type":"menu","sub_menu_items":[{
                    "name":"官网","type":"link","link":"https://example.com",
                    "unexpected":true
                }]
            }]}}),
            "unknown field `unexpected` at menu.items[0].sub_menu_items[0]",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"官网","type":"link","link":"http://example.com"
            }]}}),
            "HTTPS",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"官网","type":"link","link":" https://example.com "
            }]}}),
            "custom menu link",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"更多","type":"menu","sub_menu_items":[{
                    "name":"官网","type":"link","link":"https://example.com/\npath"
                }]
            }]}}),
            "custom sub-menu link",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"帮助","type":"send_message","send_message":"/help",
                "link":"https://example.com"
            }]}}),
            "custom menu item type SendMessage",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"开关","type":"switch","switch":{"switch_id":" ","default":false}
            }]}}),
            "must not be empty",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"更多","type":"menu","sub_menu_items":[
                    {"name":"官网","type":"link","link":"https://example.com"},
                    {"name":"官网","type":"link","link":"https://example.com"},
                    {"name":"官网","type":"link","link":"https://example.com"},
                    {"name":"官网","type":"link","link":"https://example.com"},
                    {"name":"官网","type":"link","link":"https://example.com"},
                    {"name":"官网","type":"link","link":"https://example.com"}
                ]
            }]}}),
            "more than 5",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{"name":"bad","type":"unknown"}]}}),
            "unknown variant",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"中文中文中文","type":"send_message","send_message":"/help"
            }]}}),
            "weighted characters",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"更多","type":"menu","sub_menu_items":[{
                    "name":"中文中文中文中文","type":"link","link":"https://example.com"
                }]
            }]}}),
            "weighted characters",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"开关","type":"switch","send_message":"extra",
                "switch":{"switch_id":"notifications","default":false}
            }]}}),
            "custom menu item type Switch",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{"name":"更多","type":"menu"}]}}),
            "custom menu item type Menu",
        ),
        (
            "qq.bot.menu.update",
            json!({"menu":{"items":[{
                "name":"更多","type":"menu","sub_menu_items":[{
                    "name":"命令","type":"send_message","send_message":"/help",
                    "link":"https://example.com"
                }]
            }]}}),
            "custom sub-menu item type SendMessage",
        ),
    ];
    invalid_cases.push((
        "qq.bot.menu.update",
        json!({"menu":{"items":vec![
            json!({"name":"帮助","type":"send_message","send_message":"/help"}); 11
        ]}}),
        "more than 10",
    ));
    for (name, payload, expected) in invalid_cases {
        let error = adapter
            .execute(Action::Platform {
                name: name.to_owned(),
                payload,
            })
            .await
            .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic Action error");
        };
        assert!(
            message.contains(expected),
            "expected `{expected}` in `{message}`"
        );
    }

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert_eq!(observations.api_calls.load(Ordering::SeqCst), 0);
    assert!(observations.bodies.lock().await.is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
