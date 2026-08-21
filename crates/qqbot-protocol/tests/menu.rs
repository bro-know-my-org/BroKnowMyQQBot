use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use qqbot_protocol::{
    ApiError, BotMenu, BotMenuItem, BotMenuItemType, BotMenuResponse, BotMenuSwitch,
    BotSubMenuItem, BotSubMenuItemType, GenerateShareLinkRequest, MenuValidationError,
    OpenApiClient, ShareLinkValidationError, TokenManager, UpdateBotMenuRequest,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    bodies: Mutex<Vec<Value>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations.token_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn generate_share_link(
    State(observations): State<Arc<Observations>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations.bodies.lock().await.push(body);
    Json(json!({"url_link":"https://qun.qq.com/share-link"}))
}

async fn get_menu() -> Json<Value> {
    Json(json!({
        "version": 7,
        "future_response_field": true,
        "menu": {
            "future_menu_field": true,
            "items": [
                {"name":"帮助","type":"send_message","send_message":"/help","future_item_field":true},
                {"name":"官网","type":"link","link":"https://example.com"},
                {"name":"开关","type":"switch","switch":{"switch_id":"notifications","default":true,"future_switch_field":true}},
                {"name":"更多","type":"menu","sub_menu_items":[
                    {"name":"官网","type":"link","link":"https://example.com","future_sub_item_field":true},
                    {"name":"命令","type":"send_message","send_message":"/commands"}
                ]}
            ]
        }
    }))
}

async fn update_menu(
    State(observations): State<Arc<Observations>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations.bodies.lock().await.push(body);
    Json(json!({"version":8}))
}

async fn client() -> (OpenApiClient, JoinHandle<()>, Arc<Observations>) {
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
        OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        server_task,
        observations,
    )
}

fn valid_menu_request() -> UpdateBotMenuRequest {
    UpdateBotMenuRequest {
        menu: Some(BotMenu {
            items: vec![
                BotMenuItem {
                    name: "开关".to_owned(),
                    item_type: BotMenuItemType::Switch,
                    sub_menu_items: None,
                    send_message: None,
                    link: None,
                    switch: Some(BotMenuSwitch {
                        switch_id: "notifications".to_owned(),
                        default: true,
                    }),
                },
                BotMenuItem {
                    name: "更多".to_owned(),
                    item_type: BotMenuItemType::Menu,
                    sub_menu_items: Some(vec![BotSubMenuItem {
                        name: "官网".to_owned(),
                        item_type: BotSubMenuItemType::Link,
                        send_message: None,
                        link: Some("https://example.com/docs".to_owned()),
                    }]),
                    send_message: None,
                    link: None,
                    switch: None,
                },
            ],
        }),
    }
}

fn send_message_item(name: &str) -> BotMenuItem {
    BotMenuItem {
        name: name.to_owned(),
        item_type: BotMenuItemType::SendMessage,
        sub_menu_items: None,
        send_message: Some("/help".to_owned()),
        link: None,
        switch: None,
    }
}

fn nested_send_message_item(name: &str) -> BotSubMenuItem {
    BotSubMenuItem {
        name: name.to_owned(),
        item_type: BotSubMenuItemType::SendMessage,
        send_message: Some("/nested".to_owned()),
        link: None,
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the exact request and response contract is clearer in one end-to-end protocol test"
)]
async fn executes_share_link_and_custom_menu_requests() {
    let (client, server_task, observations) = client().await;

    let link = client
        .generate_share_link(&GenerateShareLinkRequest {
            callback_data: Some("campaign-1".to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(link.url_link, "https://qun.qq.com/share-link");
    for callback_data in ["x".repeat(32), "中".repeat(32)] {
        client
            .generate_share_link(&GenerateShareLinkRequest {
                callback_data: Some(callback_data),
            })
            .await
            .unwrap();
    }
    client
        .generate_share_link(&GenerateShareLinkRequest::default())
        .await
        .unwrap();

    let menu = client.bot_menu().await.unwrap();
    assert_eq!(
        menu,
        BotMenuResponse {
            version: 7,
            menu: Some(BotMenu {
                items: vec![
                    BotMenuItem {
                        name: "帮助".to_owned(),
                        item_type: BotMenuItemType::SendMessage,
                        sub_menu_items: None,
                        send_message: Some("/help".to_owned()),
                        link: None,
                        switch: None,
                    },
                    BotMenuItem {
                        name: "官网".to_owned(),
                        item_type: BotMenuItemType::Link,
                        sub_menu_items: None,
                        send_message: None,
                        link: Some("https://example.com".to_owned()),
                        switch: None,
                    },
                    BotMenuItem {
                        name: "开关".to_owned(),
                        item_type: BotMenuItemType::Switch,
                        sub_menu_items: None,
                        send_message: None,
                        link: None,
                        switch: Some(BotMenuSwitch {
                            switch_id: "notifications".to_owned(),
                            default: true,
                        }),
                    },
                    BotMenuItem {
                        name: "更多".to_owned(),
                        item_type: BotMenuItemType::Menu,
                        sub_menu_items: Some(vec![
                            BotSubMenuItem {
                                name: "官网".to_owned(),
                                item_type: BotSubMenuItemType::Link,
                                send_message: None,
                                link: Some("https://example.com".to_owned()),
                            },
                            BotSubMenuItem {
                                name: "命令".to_owned(),
                                item_type: BotSubMenuItemType::SendMessage,
                                send_message: Some("/commands".to_owned()),
                                link: None,
                            },
                        ]),
                        send_message: None,
                        link: None,
                        switch: None,
                    },
                ],
            }),
        }
    );

    let version = client.update_bot_menu(&valid_menu_request()).await.unwrap();
    assert_eq!(version.version, 8);
    client
        .update_bot_menu(&UpdateBotMenuRequest {
            menu: Some(BotMenu { items: Vec::new() }),
        })
        .await
        .unwrap();
    client
        .update_bot_menu(&UpdateBotMenuRequest {
            menu: Some(BotMenu {
                items: vec![BotMenuItem {
                    name: "更多".to_owned(),
                    item_type: BotMenuItemType::Menu,
                    sub_menu_items: Some(Vec::new()),
                    send_message: None,
                    link: None,
                    switch: None,
                }],
            }),
        })
        .await
        .unwrap();
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    let bodies = observations.bodies.lock().await;
    assert_eq!(bodies[0], json!({"callback_data":"campaign-1"}));
    assert_eq!(bodies[1], json!({"callback_data":"x".repeat(32)}));
    assert_eq!(bodies[2], json!({"callback_data":"中".repeat(32)}));
    assert_eq!(bodies[3], json!({}));
    assert_eq!(
        bodies[4],
        json!({"menu":{"items":[
            {"name":"开关","type":"switch","switch":{"switch_id":"notifications","default":true}},
            {"name":"更多","type":"menu","sub_menu_items":[
                {"name":"官网","type":"link","link":"https://example.com/docs"}
            ]}
        ]}})
    );
    assert_eq!(bodies[5], json!({"menu":{"items":[]}}));
    assert_eq!(
        bodies[6],
        json!({"menu":{"items":[{
            "name":"更多","type":"menu","sub_menu_items":[]
        }]}})
    );
    drop(bodies);

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_share_link_and_custom_menu_before_authentication() {
    let (client, server_task, observations) = client().await;

    for callback_data in ["x".repeat(33), "中".repeat(33)] {
        let error = client
            .generate_share_link(&GenerateShareLinkRequest {
                callback_data: Some(callback_data),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            ApiError::InvalidShareLinkRequest(ShareLinkValidationError::CallbackDataTooLong {
                length: 33
            })
        ));
        assert!(error.to_string().contains("share-link"));
    }
    assert!(matches!(
        client
            .update_bot_menu(&UpdateBotMenuRequest { menu: None })
            .await,
        Err(ApiError::InvalidMenuRequest(
            MenuValidationError::MissingMenu
        ))
    ));

    let mut request = valid_menu_request();
    let menu = request.menu.as_mut().unwrap();
    menu.items[0].name = "中文中文中文".to_owned();
    assert!(matches!(
        client.update_bot_menu(&request).await,
        Err(ApiError::InvalidMenuRequest(
            MenuValidationError::WeightedNameTooLong {
                weight: 12,
                maximum: 10,
                ..
            }
        ))
    ));

    let mut request = valid_menu_request();
    let menu = request.menu.as_mut().unwrap();
    menu.items[1].sub_menu_items.as_mut().unwrap()[0].link = Some("http://example.com".to_owned());
    assert!(matches!(
        client.update_bot_menu(&request).await,
        Err(ApiError::InvalidMenuRequest(
            MenuValidationError::InvalidHttpsUrl {
                field: "QQ custom sub-menu link"
            }
        ))
    ));

    let mut request = valid_menu_request();
    let menu = request.menu.as_mut().unwrap();
    menu.items[0].send_message = Some("unexpected".to_owned());
    assert!(matches!(
        client.update_bot_menu(&request).await,
        Err(ApiError::InvalidMenuRequest(
            MenuValidationError::IncompatibleBehavior {
                item_type: BotMenuItemType::Switch
            }
        ))
    ));

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert!(observations.bodies.lock().await.is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[test]
fn validates_menu_count_names_and_behavior_boundaries() {
    let base = send_message_item("12345678中");

    assert!(
        BotMenu {
            items: vec![base.clone(); 10]
        }
        .validate()
        .is_ok()
    );
    assert!(matches!(
        BotMenu {
            items: vec![base.clone(); 11]
        }
        .validate(),
        Err(MenuValidationError::TooManyItems { count: 11 })
    ));

    let mut oversized_name = base.clone();
    oversized_name.name.push('a');
    assert!(matches!(
        BotMenu {
            items: vec![oversized_name]
        }
        .validate(),
        Err(MenuValidationError::WeightedNameTooLong { maximum: 10, .. })
    ));

    let nested = nested_send_message_item("123456789012中");
    for count in [0, 5] {
        assert!(
            BotMenu {
                items: vec![BotMenuItem {
                    name: "更多".to_owned(),
                    item_type: BotMenuItemType::Menu,
                    sub_menu_items: Some(vec![nested.clone(); count]),
                    send_message: None,
                    link: None,
                    switch: None,
                }]
            }
            .validate()
            .is_ok()
        );
    }
    assert!(matches!(
        BotMenu {
            items: vec![BotMenuItem {
                name: "更多".to_owned(),
                item_type: BotMenuItemType::Menu,
                sub_menu_items: Some(vec![nested.clone(); 6]),
                send_message: None,
                link: None,
                switch: None,
            }]
        }
        .validate(),
        Err(MenuValidationError::TooManySubMenuItems { count: 6 })
    ));

    let mut oversized_nested = nested;
    oversized_nested.name.push('a');
    assert!(matches!(
        BotMenu {
            items: vec![BotMenuItem {
                name: "更多".to_owned(),
                item_type: BotMenuItemType::Menu,
                sub_menu_items: Some(vec![oversized_nested]),
                send_message: None,
                link: None,
                switch: None,
            }]
        }
        .validate(),
        Err(MenuValidationError::WeightedNameTooLong { maximum: 14, .. })
    ));
}

fn assert_invalid_menu_items(cases: impl IntoIterator<Item = (BotMenuItem, MenuValidationError)>) {
    for (invalid, expected) in cases {
        assert_eq!(
            BotMenu {
                items: vec![invalid]
            }
            .validate()
            .unwrap_err(),
            expected
        );
    }
}

#[test]
fn rejects_empty_and_missing_menu_behaviors() {
    assert_invalid_menu_items([
        (
            BotMenuItem {
                name: " ".to_owned(),
                ..send_message_item("帮助")
            },
            MenuValidationError::EmptyField {
                field: "QQ custom menu item name",
            },
        ),
        (
            BotMenuItem {
                send_message: Some(" ".to_owned()),
                ..send_message_item("帮助")
            },
            MenuValidationError::EmptyField {
                field: "QQ custom menu send_message",
            },
        ),
        (
            BotMenuItem {
                send_message: None,
                ..send_message_item("帮助")
            },
            MenuValidationError::IncompatibleBehavior {
                item_type: BotMenuItemType::SendMessage,
            },
        ),
        (
            BotMenuItem {
                name: "更多".to_owned(),
                item_type: BotMenuItemType::Menu,
                sub_menu_items: None,
                send_message: None,
                link: None,
                switch: None,
            },
            MenuValidationError::IncompatibleBehavior {
                item_type: BotMenuItemType::Menu,
            },
        ),
    ]);
}

#[test]
fn rejects_mutually_incompatible_menu_behaviors() {
    assert_invalid_menu_items([
        (
            BotMenuItem {
                sub_menu_items: Some(Vec::new()),
                ..send_message_item("帮助")
            },
            MenuValidationError::IncompatibleBehavior {
                item_type: BotMenuItemType::SendMessage,
            },
        ),
        (
            BotMenuItem {
                name: "官网".to_owned(),
                item_type: BotMenuItemType::Link,
                sub_menu_items: None,
                send_message: Some("extra".to_owned()),
                link: Some("https://example.com".to_owned()),
                switch: None,
            },
            MenuValidationError::IncompatibleBehavior {
                item_type: BotMenuItemType::Link,
            },
        ),
        (
            BotMenuItem {
                name: "更多".to_owned(),
                item_type: BotMenuItemType::Menu,
                sub_menu_items: Some(vec![BotSubMenuItem {
                    name: "官网".to_owned(),
                    item_type: BotSubMenuItemType::Link,
                    send_message: Some("extra".to_owned()),
                    link: Some("https://example.com".to_owned()),
                }]),
                send_message: None,
                link: None,
                switch: None,
            },
            MenuValidationError::IncompatibleSubMenuBehavior {
                item_type: BotSubMenuItemType::Link,
            },
        ),
        (
            BotMenuItem {
                name: "开关".to_owned(),
                item_type: BotMenuItemType::Switch,
                sub_menu_items: None,
                send_message: None,
                link: None,
                switch: Some(BotMenuSwitch {
                    switch_id: " ".to_owned(),
                    default: false,
                }),
            },
            MenuValidationError::EmptyField {
                field: "QQ custom menu switch_id",
            },
        ),
    ]);
}

#[test]
fn rejects_https_urls_that_require_whitespace_normalization() {
    for link in [
        " https://example.com",
        "https://example.com ",
        "\thttps://example.com",
        "https://example.com/\rpath",
        "https://example.com/\npath",
        "\0https://example.com",
        "https://example.com/has space",
        "https://example.com/has\u{00a0}space",
        "https://example.com/control\u{0085}path",
    ] {
        assert_eq!(
            BotMenu {
                items: vec![BotMenuItem {
                    name: "官网".to_owned(),
                    item_type: BotMenuItemType::Link,
                    sub_menu_items: None,
                    send_message: None,
                    link: Some(link.to_owned()),
                    switch: None,
                }]
            }
            .validate()
            .unwrap_err(),
            MenuValidationError::InvalidHttpsUrl {
                field: "QQ custom menu link"
            }
        );
        assert_eq!(
            BotMenu {
                items: vec![BotMenuItem {
                    name: "更多".to_owned(),
                    item_type: BotMenuItemType::Menu,
                    sub_menu_items: Some(vec![BotSubMenuItem {
                        name: "官网".to_owned(),
                        item_type: BotSubMenuItemType::Link,
                        send_message: None,
                        link: Some(link.to_owned()),
                    }]),
                    send_message: None,
                    link: None,
                    switch: None,
                }]
            }
            .validate()
            .unwrap_err(),
            MenuValidationError::InvalidHttpsUrl {
                field: "QQ custom sub-menu link"
            }
        );
    }
}
