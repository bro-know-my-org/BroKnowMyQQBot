use std::{collections::HashMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use qqbot_protocol::{
    ApiError, CreatePanelRequest, OpenApiClient, Panel, PanelItem, PanelItemType, PanelListRequest,
    PanelScope, PanelTargetOperation, PanelTargetType, PanelValidationError, TokenManager,
    UpdatePanelRequest, UpdatePanelTargetsRequest,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    requests: Mutex<Vec<(String, Value)>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Json<Value> {
    observations.token_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({"access_token":"token","expires_in":7200}))
}

async fn list_panels(
    State(observations): State<Arc<Observations>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push(("list".to_owned(), json!(query)));
    Json(json!({
        "records":[{
            "panel_id":"panel-list",
            "scope":"c2c",
            "target_type":"all",
            "panel":{"items":[],"remark":"listed"},
            "version":2
        }],
        "next_cursor":"next",
        "is_end":false
    }))
}

async fn create_panel(
    State(observations): State<Arc<Observations>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push(("create".to_owned(), body));
    Json(json!({"panel_id":"panel-created"}))
}

async fn get_panel(Path(panel_id): Path<String>) -> Json<Value> {
    Json(json!({
        "panel_id":panel_id,
        "scope":"group",
        "target_type":"specific",
        "panel":{"items":[],"remark":"detail","version":6},
        "created_at":"2026-08-12T10:00:00+08:00",
        "updated_at":"2026-08-13T11:30:00+08:00",
        "version":7,
        "user_openids":[],
        "group_openids":["group-1"]
    }))
}

async fn update_panel(
    State(observations): State<Arc<Observations>>,
    Path(panel_id): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    observations
        .requests
        .lock()
        .await
        .push((format!("update:{panel_id}"), body));
    Json(json!({"version":8}))
}

async fn delete_panel(
    State(observations): State<Arc<Observations>>,
    Path(panel_id): Path<String>,
) -> StatusCode {
    observations
        .requests
        .lock()
        .await
        .push((format!("delete:{panel_id}"), json!(null)));
    StatusCode::NO_CONTENT
}

async fn update_targets(
    State(observations): State<Arc<Observations>>,
    Path(panel_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
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

async fn client() -> (OpenApiClient, JoinHandle<()>, Arc<Observations>) {
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
        OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        server_task,
        observations,
    )
}

fn panel() -> Panel {
    Panel {
        items: Some(vec![
            PanelItem {
                name: "帮助".to_owned(),
                desc: "查看帮助".to_owned(),
                item_type: PanelItemType::Command,
                only_admin: false,
                link: None,
            },
            PanelItem {
                name: "官网".to_owned(),
                desc: "打开官网".to_owned(),
                item_type: PanelItemType::Link,
                only_admin: true,
                link: Some("https://example.com".to_owned()),
            },
        ]),
        remark: Some("panel remark".to_owned()),
        version: None,
    }
}

async fn assert_invalid_create_targets_before_authentication(client: &OpenApiClient) {
    for scope in [PanelScope::C2c, PanelScope::Group] {
        let field = if scope == PanelScope::C2c {
            "user_openids"
        } else {
            "group_openids"
        };
        for (openids, expected) in [
            (Vec::new(), PanelValidationError::MissingTargetObjects),
            (
                (0..21).map(|index| format!("openid-{index}")).collect(),
                PanelValidationError::TooManyTargets {
                    field,
                    count: 21,
                    maximum: 20,
                },
            ),
            (
                vec![String::new()],
                PanelValidationError::EmptyOpenId { field, index: 0 },
            ),
            (
                vec!["bad id".to_owned()],
                PanelValidationError::InvalidOpenId { field, index: 0 },
            ),
        ] {
            let request = CreatePanelRequest {
                scope,
                target_type: PanelTargetType::Specific,
                user_openids: (scope == PanelScope::C2c).then_some(openids.clone()),
                group_openids: (scope == PanelScope::Group).then_some(openids),
                panel: panel(),
            };
            match client.create_panel(&request).await.unwrap_err() {
                ApiError::InvalidPanelRequest(actual) => assert_eq!(actual, expected),
                other => panic!("expected invalid panel request, got {other}"),
            }
        }
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "all typed panel endpoint response contracts are easier to verify in one flow"
)]
async fn executes_all_panel_endpoints_with_typed_responses() {
    let (client, server_task, observations) = client().await;

    let page = client
        .panels(&PanelListRequest {
            scope: PanelScope::C2c,
            cursor: Some(String::new()),
            limit: Some(50),
        })
        .await
        .unwrap();
    assert_eq!(page.records[0].panel_id, "panel-list");
    assert!(page.records[0].created_at.is_none());
    assert_eq!(page.records[0].version, 2);
    assert_eq!(page.next_cursor, "next");
    assert!(!page.is_end);

    let created = client
        .create_panel(&CreatePanelRequest {
            scope: PanelScope::Group,
            target_type: PanelTargetType::Specific,
            user_openids: None,
            group_openids: Some(vec!["group-1".to_owned()]),
            panel: panel(),
        })
        .await
        .unwrap();
    assert_eq!(created.panel_id, "panel-created");

    let detail = client.panel("panel/id").await.unwrap();
    assert_eq!(detail.record.panel_id, "panel/id");
    assert_eq!(detail.record.panel.version, Some(6));
    assert_eq!(detail.record.version, 7);
    assert_eq!(detail.group_openids, ["group-1"]);
    assert!(detail.user_openids.is_empty());
    assert_eq!(
        detail.record.created_at.unwrap().to_rfc3339(),
        "2026-08-12T10:00:00+08:00"
    );
    assert_eq!(
        detail.record.updated_at.unwrap().to_rfc3339(),
        "2026-08-13T11:30:00+08:00"
    );

    let version = client
        .update_panel("panel/id", &UpdatePanelRequest { panel: panel() })
        .await
        .unwrap();
    assert_eq!(version.version, 8);
    client
        .update_panel_targets(
            "panel/id",
            &UpdatePanelTargetsRequest {
                op: PanelTargetOperation::Add,
                user_openids: None,
                group_openids: Some(vec!["group-1".to_owned()]),
            },
        )
        .await
        .unwrap();
    client
        .update_panel_targets(
            "panel-empty",
            &UpdatePanelTargetsRequest {
                op: PanelTargetOperation::Del,
                user_openids: Some(vec!["user-1".to_owned()]),
                group_openids: None,
            },
        )
        .await
        .unwrap();
    client.delete_panel("panel/id").await.unwrap();

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    let requests = observations.requests.lock().await;
    assert_eq!(requests[0].1["scope"], "c2c");
    assert_eq!(requests[0].1["cursor"], "");
    assert_eq!(requests[0].1["limit"], "50");
    assert_eq!(
        requests[1].1,
        json!({
            "scope":"group",
            "target_type":"specific",
            "group_openids":["group-1"],
            "panel":{
                "items":[
                    {"name":"帮助","desc":"查看帮助","type":"command","only_admin":false},
                    {"name":"官网","desc":"打开官网","type":"link","only_admin":true,
                     "link":"https://example.com"}
                ],
                "remark":"panel remark"
            }
        })
    );
    assert_eq!(requests[2].0, "update:panel/id");
    assert_eq!(requests[2].1, json!({"panel":requests[1].1["panel"]}));
    assert_eq!(requests[3].0, "target:panel/id");
    assert_eq!(
        requests[3].1,
        json!({"op":"add","group_openids":["group-1"]})
    );
    assert_eq!(requests[4].0, "target:panel-empty");
    assert_eq!(requests[4].1, json!({"op":"del","user_openids":["user-1"]}));
    assert_eq!(requests[5].0, "delete:panel/id");
    drop(requests);

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_panel_requests_before_authentication() {
    let (client, server_task, observations) = client().await;

    assert!(matches!(
        client
            .panels(&PanelListRequest {
                scope: PanelScope::C2c,
                cursor: None,
                limit: Some(0),
            })
            .await,
        Err(ApiError::InvalidPanelRequest(
            PanelValidationError::PageLimitOutOfRange { limit: 0 }
        ))
    ));

    let mut invalid_create = CreatePanelRequest {
        scope: PanelScope::Channel,
        target_type: PanelTargetType::Specific,
        user_openids: None,
        group_openids: None,
        panel: panel(),
    };
    assert!(matches!(
        client.create_panel(&invalid_create).await,
        Err(ApiError::InvalidPanelRequest(
            PanelValidationError::SpecificScopeUnsupported { .. }
        ))
    ));
    invalid_create.scope = PanelScope::Group;
    invalid_create.target_type = PanelTargetType::All;
    invalid_create.group_openids = Some(vec!["group-1".to_owned()]);
    assert!(matches!(
        client.create_panel(&invalid_create).await,
        Err(ApiError::InvalidPanelRequest(
            PanelValidationError::UnexpectedTargetField { .. }
        ))
    ));

    assert_invalid_create_targets_before_authentication(&client).await;

    let mut invalid_panel = panel();
    invalid_panel.items.as_mut().unwrap()[1].link = Some("http://example.com".to_owned());
    assert!(matches!(
        client
            .update_panel(
                "panel-1",
                &UpdatePanelRequest {
                    panel: invalid_panel
                }
            )
            .await,
        Err(ApiError::InvalidPanelRequest(
            PanelValidationError::InvalidItem { index: 1, source }
        )) if *source == PanelValidationError::InvalidHttpsUrl
    ));

    assert!(matches!(
        client
            .update_panel_targets(
                "panel-1",
                &UpdatePanelTargetsRequest {
                    op: PanelTargetOperation::Del,
                    user_openids: Some(vec!["user-1".to_owned()]),
                    group_openids: Some(vec!["group-1".to_owned()]),
                }
            )
            .await,
        Err(ApiError::InvalidPanelRequest(
            PanelValidationError::MultipleTargetKinds
        ))
    ));

    for result in [
        client.panel(" ").await.map(|_| ()),
        client
            .update_panel("", &UpdatePanelRequest { panel: panel() })
            .await
            .map(|_| ()),
        client.delete_panel(" ").await,
        client
            .update_panel_targets(
                "",
                &UpdatePanelTargetsRequest {
                    op: PanelTargetOperation::Add,
                    user_openids: Some(vec!["user-1".to_owned()]),
                    group_openids: None,
                },
            )
            .await,
    ] {
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert!(observations.requests.lock().await.is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[test]
fn serializes_every_panel_enum_variant() {
    for (scope, wire) in [
        (PanelScope::C2c, "c2c"),
        (PanelScope::Group, "group"),
        (PanelScope::Channel, "channel"),
        (PanelScope::Dm, "dm"),
    ] {
        assert_eq!(scope.as_str(), wire);
        assert_eq!(serde_json::to_value(scope).unwrap(), wire);
    }

    for (target_type, wire) in [
        (PanelTargetType::All, "all"),
        (PanelTargetType::Specific, "specific"),
    ] {
        assert_eq!(serde_json::to_value(target_type).unwrap(), wire);
    }

    for (item_type, wire) in [
        (PanelItemType::Command, "command"),
        (PanelItemType::Link, "link"),
    ] {
        assert_eq!(serde_json::to_value(item_type).unwrap(), wire);
    }

    for (operation, wire) in [
        (PanelTargetOperation::Add, "add"),
        (PanelTargetOperation::Del, "del"),
    ] {
        assert_eq!(serde_json::to_value(operation).unwrap(), wire);
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "panel limits are clearer as one table-like boundary contract"
)]
fn validates_panel_boundaries_and_conditional_fields() {
    let command = PanelItem {
        name: "123456789012中".to_owned(),
        desc: format!("{}中", "a".repeat(28)),
        item_type: PanelItemType::Command,
        only_admin: false,
        link: None,
    };
    for scope in [PanelScope::C2c, PanelScope::Group] {
        let request = CreatePanelRequest {
            scope,
            target_type: PanelTargetType::Specific,
            user_openids: None,
            group_openids: None,
            panel: panel(),
        };
        assert!(request.validate().is_ok());
        let wire = serde_json::to_value(&request).unwrap();
        assert!(!wire.as_object().unwrap().contains_key("user_openids"));
        assert!(!wire.as_object().unwrap().contains_key("group_openids"));
    }

    for scope in [
        PanelScope::C2c,
        PanelScope::Group,
        PanelScope::Channel,
        PanelScope::Dm,
    ] {
        assert!(
            CreatePanelRequest {
                scope,
                target_type: PanelTargetType::All,
                user_openids: None,
                group_openids: None,
                panel: panel(),
            }
            .validate()
            .is_ok()
        );
        for (user_openids, group_openids) in [
            (Some(vec!["user-1".to_owned()]), None),
            (None, Some(vec!["group-1".to_owned()])),
            (
                Some(vec!["user-1".to_owned()]),
                Some(vec!["group-1".to_owned()]),
            ),
        ] {
            assert!(matches!(
                CreatePanelRequest {
                    scope,
                    target_type: PanelTargetType::All,
                    user_openids,
                    group_openids,
                    panel: panel(),
                }
                .validate(),
                Err(PanelValidationError::UnexpectedTargetField { .. })
            ));
        }
    }

    for (scope, legal_user, legal_group) in [
        (PanelScope::C2c, true, false),
        (PanelScope::Group, false, true),
    ] {
        for (has_user, has_group) in [(false, false), (true, false), (false, true), (true, true)] {
            let result = CreatePanelRequest {
                scope,
                target_type: PanelTargetType::Specific,
                user_openids: has_user.then(|| vec!["user-1".to_owned()]),
                group_openids: has_group.then(|| vec!["group-1".to_owned()]),
                panel: panel(),
            }
            .validate();
            if (!has_user || legal_user) && (!has_group || legal_group) {
                assert!(result.is_ok());
            } else {
                assert!(matches!(
                    result,
                    Err(PanelValidationError::UnexpectedTargetField { .. })
                ));
            }
        }
    }

    for scope in [PanelScope::Channel, PanelScope::Dm] {
        for (has_user, has_group) in [(false, false), (true, false), (false, true), (true, true)] {
            assert!(matches!(
                CreatePanelRequest {
                    scope,
                    target_type: PanelTargetType::Specific,
                    user_openids: has_user.then(|| vec!["user-1".to_owned()]),
                    group_openids: has_group.then(|| vec!["group-1".to_owned()]),
                    panel: panel(),
                }
                .validate(),
                Err(PanelValidationError::SpecificScopeUnsupported { .. })
            ));
        }
    }

    for scope in [PanelScope::C2c, PanelScope::Group] {
        let request_with = |openids: Vec<String>| CreatePanelRequest {
            scope,
            target_type: PanelTargetType::Specific,
            user_openids: (scope == PanelScope::C2c).then_some(openids.clone()),
            group_openids: (scope == PanelScope::Group).then_some(openids),
            panel: panel(),
        };
        assert!(
            request_with((0..20).map(|index| format!("openid-{index}")).collect())
                .validate()
                .is_ok()
        );
        for (openids, expected) in [
            (Vec::new(), PanelValidationError::MissingTargetObjects),
            (
                (0..21).map(|index| format!("openid-{index}")).collect(),
                PanelValidationError::TooManyTargets {
                    field: if scope == PanelScope::C2c {
                        "user_openids"
                    } else {
                        "group_openids"
                    },
                    count: 21,
                    maximum: 20,
                },
            ),
            (
                vec![String::new()],
                PanelValidationError::EmptyOpenId {
                    field: if scope == PanelScope::C2c {
                        "user_openids"
                    } else {
                        "group_openids"
                    },
                    index: 0,
                },
            ),
            (
                vec!["bad id".to_owned()],
                PanelValidationError::InvalidOpenId {
                    field: if scope == PanelScope::C2c {
                        "user_openids"
                    } else {
                        "group_openids"
                    },
                    index: 0,
                },
            ),
        ] {
            assert_eq!(request_with(openids).validate().unwrap_err(), expected);
        }
    }
    let boundary_panel = Panel {
        items: Some(vec![command.clone(); 20]),
        remark: Some("中".repeat(255)),
        version: Some(1),
    };
    assert!(boundary_panel.validate().is_ok());
    assert_eq!(serde_json::to_value(&boundary_panel).unwrap()["version"], 1);
    assert!(matches!(
        Panel {
            items: Some(vec![command.clone(); 21]),
            remark: None,
            version: None,
        }
        .validate(),
        Err(PanelValidationError::TooManyItems { count: 21 })
    ));

    let mut invalid_name = command.clone();
    invalid_name.name.push('a');
    assert!(matches!(
        Panel {
            items: Some(vec![invalid_name]),
            remark: None,
            version: None,
        }
        .validate(),
        Err(PanelValidationError::InvalidItem { index: 0, source })
            if matches!(source.as_ref(), PanelValidationError::TextTooLong { maximum: 14, .. })
    ));
    let mut invalid_desc = command.clone();
    invalid_desc.desc.push('a');
    assert!(matches!(
        Panel {
            items: Some(vec![invalid_desc]),
            remark: None,
            version: None,
        }
        .validate(),
        Err(PanelValidationError::InvalidItem { index: 0, source })
            if matches!(source.as_ref(), PanelValidationError::TextTooLong { maximum: 30, .. })
    ));
    for (field, invalid_item) in [
        (
            "panel.items[].name",
            PanelItem {
                name: String::new(),
                ..command.clone()
            },
        ),
        (
            "panel.items[].desc",
            PanelItem {
                desc: "   ".to_owned(),
                ..command.clone()
            },
        ),
    ] {
        assert!(matches!(
            Panel {
                items: Some(vec![invalid_item]),
                remark: None,
                version: None,
            }
            .validate(),
            Err(PanelValidationError::InvalidItem { index: 0, source })
                if matches!(source.as_ref(), PanelValidationError::EmptyField { field: actual } if *actual == field)
        ));
    }
    assert!(matches!(
        Panel {
            items: None,
            remark: Some("中".repeat(256)),
            version: None,
        }
        .validate(),
        Err(PanelValidationError::CharacterLimitExceeded {
            length: 256,
            maximum: 255,
            ..
        })
    ));

    let mut command_with_link = command.clone();
    command_with_link.link = Some("https://example.com".to_owned());
    assert_eq!(
        Panel {
            items: Some(vec![command_with_link]),
            remark: None,
            version: None,
        }
        .validate()
        .unwrap_err(),
        PanelValidationError::InvalidItem {
            index: 0,
            source: Box::new(PanelValidationError::UnexpectedLink),
        }
    );
    let link_without_url = PanelItem {
        item_type: PanelItemType::Link,
        ..command.clone()
    };
    assert_eq!(
        Panel {
            items: Some(vec![link_without_url]),
            remark: None,
            version: None,
        }
        .validate()
        .unwrap_err(),
        PanelValidationError::InvalidItem {
            index: 0,
            source: Box::new(PanelValidationError::MissingLink),
        }
    );
    for link in [
        "http://example.com",
        " https://example.com",
        "https://example.com/has\u{00a0}space",
        "https://example.com/control\u{0085}path",
    ] {
        let invalid_link = PanelItem {
            item_type: PanelItemType::Link,
            link: Some(link.to_owned()),
            ..command.clone()
        };
        assert_eq!(
            Panel {
                items: Some(vec![invalid_link]),
                remark: None,
                version: None,
            }
            .validate()
            .unwrap_err(),
            PanelValidationError::InvalidItem {
                index: 0,
                source: Box::new(PanelValidationError::InvalidHttpsUrl),
            }
        );
    }

    assert!(
        PanelListRequest {
            scope: PanelScope::Dm,
            cursor: None,
            limit: Some(50),
        }
        .validate()
        .is_ok()
    );
    assert!(matches!(
        PanelListRequest {
            scope: PanelScope::Dm,
            cursor: None,
            limit: Some(51),
        }
        .validate(),
        Err(PanelValidationError::PageLimitOutOfRange { limit: 51 })
    ));

    let targets = UpdatePanelTargetsRequest {
        op: PanelTargetOperation::Add,
        user_openids: Some((0..20).map(|index| format!("user-{index}")).collect()),
        group_openids: None,
    };
    assert!(targets.validate().is_ok());
    let mut too_many_targets = targets.clone();
    too_many_targets
        .user_openids
        .as_mut()
        .unwrap()
        .push("user-20".to_owned());
    assert!(matches!(
        too_many_targets.validate(),
        Err(PanelValidationError::TooManyTargets { count: 21, .. })
    ));
    assert_eq!(
        UpdatePanelTargetsRequest {
            op: PanelTargetOperation::Del,
            user_openids: None,
            group_openids: None,
        }
        .validate()
        .unwrap_err(),
        PanelValidationError::MissingTargetObjects
    );
    for (openids, expected) in [
        (Vec::new(), PanelValidationError::MissingTargetObjects),
        (
            vec!["user 1".to_owned()],
            PanelValidationError::InvalidOpenId {
                field: "user_openids",
                index: 0,
            },
        ),
        (
            vec!["user-0".to_owned(), String::new()],
            PanelValidationError::EmptyOpenId {
                field: "user_openids",
                index: 1,
            },
        ),
        (
            vec!["user-0".to_owned(), "bad id".to_owned()],
            PanelValidationError::InvalidOpenId {
                field: "user_openids",
                index: 1,
            },
        ),
    ] {
        assert_eq!(
            UpdatePanelTargetsRequest {
                op: PanelTargetOperation::Del,
                user_openids: Some(openids),
                group_openids: None,
            }
            .validate()
            .unwrap_err(),
            expected
        );
    }
}
