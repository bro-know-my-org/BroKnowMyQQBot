use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post, put},
};
use qqbot_protocol::{
    ApiError, GuildMemberPageRequest, GuildRequestValidationError, GuildRoleMemberPageRequest,
    GuildRoleMemberRequest, GuildRoleMutation, OpenApiClient, RemoveGuildMemberRequest,
    TokenManager,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

async fn token() -> Json<Value> {
    Json(json!({"access_token":"token","expires_in":7200}))
}

#[derive(Default)]
struct ProtocolObservations {
    member_queries: Mutex<Vec<(String, String)>>,
    role_member_queries: Mutex<Vec<(String, String)>>,
    remove_member_bodies: Mutex<Vec<Value>>,
    create_role_bodies: Mutex<Vec<Value>>,
    update_role_bodies: Mutex<Vec<Value>>,
    role_member_put_bodies: Mutex<Vec<(String, Value)>>,
    role_member_delete_bodies: Mutex<Vec<(String, Value)>>,
}

fn member() -> Value {
    json!({
        "user": {
            "id": "user/id",
            "username": "member",
            "avatar": "https://example.com/avatar.png",
            "bot": false
        },
        "nick": "member nick",
        "roles": ["role/id"],
        "joined_at": "2026-08-22T10:00:00+08:00"
    })
}

async fn online_count(Path(channel): Path<String>) -> Json<Value> {
    assert_eq!(channel, "channel/id");
    Json(json!({"online_nums":7}))
}

async fn members(
    State(observations): State<Arc<ProtocolObservations>>,
    Path(guild): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    let after = query.get("after").cloned().unwrap();
    observations
        .member_queries
        .lock()
        .unwrap()
        .push((after.clone(), query.get("limit").cloned().unwrap()));
    let mut response_member = member();
    if after == "missing-username" {
        response_member["user"]
            .as_object_mut()
            .unwrap()
            .remove("username");
    } else if after == "missing-nick" {
        response_member.as_object_mut().unwrap().remove("nick");
    }
    Json(json!([response_member]))
}

async fn role_members(
    State(observations): State<Arc<ProtocolObservations>>,
    Path((guild, role)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    assert_eq!(role, "role/id");
    observations.role_member_queries.lock().unwrap().push((
        query.get("start_index").cloned().unwrap(),
        query.get("limit").cloned().unwrap(),
    ));
    match query.get("start_index").map(String::as_str) {
        Some("missing-data") => Json(json!({"next":"final"})),
        Some("missing-next") => Json(json!({"data":[]})),
        _ => Json(json!({"data":[member()],"next":"final"})),
    }
}

async fn get_member(Path((guild, user)): Path<(String, String)>) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    Json(member())
}

async fn remove_member(
    State(observations): State<Arc<ProtocolObservations>>,
    Path((guild, user)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    observations.remove_member_bodies.lock().unwrap().push(body);
    StatusCode::NO_CONTENT
}

async fn roles(Path(guild): Path<String>) -> Json<Value> {
    assert!(matches!(guild.as_str(), "guild/id" | "missing-roles"));
    let mut response = json!({
        "guild_id":"guild/id",
        "roles":[{
            "id":"role/id",
            "name":"moderator",
            "color":123,
            "hoist":1,
            "number":2,
            "member_limit":2000
        }],
        "role_num_limit":"30"
    });
    if guild == "missing-roles" {
        response.as_object_mut().unwrap().remove("roles");
    }
    Json(response)
}

fn role_mutation_result(include_guild: bool) -> Value {
    let mut result = json!({
        "role_id":"role/id",
        "role":{
            "id":"role/id",
            "name":"moderator",
            "color":123,
            "hoist":1,
            "number":2,
            "member_limit":2000
        }
    });
    if include_guild {
        result["guild_id"] = json!("guild/id");
    }
    result
}

async fn create_role(
    State(observations): State<Arc<ProtocolObservations>>,
    Path(guild): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    observations.create_role_bodies.lock().unwrap().push(body);
    Json(role_mutation_result(false))
}

async fn update_role(
    State(observations): State<Arc<ProtocolObservations>>,
    Path((guild, role)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    assert!(matches!(role.as_str(), "role/id" | "missing-guild"));
    observations.update_role_bodies.lock().unwrap().push(body);
    Json(role_mutation_result(role != "missing-guild"))
}

async fn delete_role(Path((guild, role)): Path<(String, String)>) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(role, "role/id");
    StatusCode::NO_CONTENT
}

async fn add_role_member(
    State(observations): State<Arc<ProtocolObservations>>,
    Path((guild, user, role)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    observations
        .role_member_put_bodies
        .lock()
        .unwrap()
        .push((role, body));
    StatusCode::NO_CONTENT
}

async fn remove_role_member(
    State(observations): State<Arc<ProtocolObservations>>,
    Path((guild, user, role)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    observations
        .role_member_delete_bodies
        .lock()
        .unwrap()
        .push((role, body));
    StatusCode::NO_CONTENT
}

async fn client() -> (OpenApiClient, JoinHandle<()>, Arc<ProtocolObservations>) {
    let observations = Arc::new(ProtocolObservations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/channels/{channel}/online_nums", get(online_count))
        .route("/guilds/{guild}/members", get(members))
        .route("/guilds/{guild}/roles/{role}/members", get(role_members))
        .route(
            "/guilds/{guild}/members/{user}",
            get(get_member).delete(remove_member),
        )
        .route("/guilds/{guild}/roles", get(roles).post(create_role))
        .route(
            "/guilds/{guild}/roles/{role}",
            patch(update_role).delete(delete_role),
        )
        .route(
            "/guilds/{guild}/members/{user}/roles/{role}",
            put(add_role_member).delete(remove_role_member),
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
        OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        server_task,
        observations,
    )
}

async fn counted_members(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    Json(json!([]))
}

async fn counted_token(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    token().await
}

async fn counted_unit(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
    calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn invalid_client() -> (OpenApiClient, Arc<AtomicUsize>, JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/app/getAppAccessToken", post(counted_token))
        .route("/guilds/{guild}/members", get(counted_members))
        .route(
            "/guilds/{guild}/members/{user}",
            axum::routing::delete(counted_unit),
        )
        .route(
            "/guilds/{guild}/members/{user}/roles/{role}",
            put(counted_unit).delete(counted_unit),
        )
        .with_state(Arc::clone(&calls));
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
        calls,
        server_task,
    )
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn calls_guild_member_and_role_endpoints() {
    let (api, server_task, observations) = client().await;

    let count = api.channel_online_member_count("channel/id").await.unwrap();
    assert_eq!(count.online_nums, 7);

    let members = api
        .guild_members(
            "guild/id",
            &GuildMemberPageRequest {
                after: "after/id".to_owned(),
                limit: 20,
            },
        )
        .await
        .unwrap();
    assert_eq!(members[0].user.id, "user/id");

    let role_members = api
        .guild_role_members(
            "guild/id",
            "role/id",
            &GuildRoleMemberPageRequest {
                start_index: "next/id".to_owned(),
                limit: 30,
            },
        )
        .await
        .unwrap();
    assert_eq!(role_members.next, "final");
    for start_index in ["missing-data", "missing-next"] {
        assert!(matches!(
            api.guild_role_members(
                "guild/id",
                "role/id",
                &GuildRoleMemberPageRequest {
                    start_index: start_index.to_owned(),
                    limit: 30,
                },
            )
            .await,
            Err(ApiError::Decode(_))
        ));
    }

    for limit in [1, 400] {
        assert_eq!(
            api.guild_members(
                "guild/id",
                &GuildMemberPageRequest {
                    after: "0".to_owned(),
                    limit,
                },
            )
            .await
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            api.guild_role_members(
                "guild/id",
                "role/id",
                &GuildRoleMemberPageRequest {
                    start_index: "0".to_owned(),
                    limit,
                },
            )
            .await
            .unwrap()
            .next,
            "final"
        );
    }

    assert_eq!(
        api.guild_member("guild/id", "user/id").await.unwrap().nick,
        "member nick"
    );
    for after in ["missing-username", "missing-nick"] {
        assert!(matches!(
            api.guild_members(
                "guild/id",
                &GuildMemberPageRequest {
                    after: after.to_owned(),
                    limit: 20,
                },
            )
            .await,
            Err(ApiError::Decode(_))
        ));
    }
    for add_blacklist in [false, true] {
        for delete_history_msg_days in [-1, 0, 3, 7, 15, 30] {
            api.remove_guild_member(
                "guild/id",
                "user/id",
                &RemoveGuildMemberRequest {
                    add_blacklist,
                    delete_history_msg_days,
                },
            )
            .await
            .unwrap();
        }
    }

    assert_eq!(api.guild_roles("guild/id").await.unwrap().roles.len(), 1);
    assert!(matches!(
        api.guild_roles("missing-roles").await,
        Err(ApiError::Decode(_))
    ));
    let created = api
        .create_guild_role(
            "guild/id",
            &GuildRoleMutation {
                color: Some(123),
                ..GuildRoleMutation::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(created.role_id, "role/id");
    api.create_guild_role(
        "guild/id",
        &GuildRoleMutation {
            name: Some("moderator".to_owned()),
            ..GuildRoleMutation::default()
        },
    )
    .await
    .unwrap();
    api.create_guild_role(
        "guild/id",
        &GuildRoleMutation {
            name: Some("moderator".to_owned()),
            color: Some(123),
            hoist: Some(1),
        },
    )
    .await
    .unwrap();
    for hoist in [0, 1] {
        api.create_guild_role(
            "guild/id",
            &GuildRoleMutation {
                hoist: Some(hoist),
                ..GuildRoleMutation::default()
            },
        )
        .await
        .unwrap();
    }
    let updated = api
        .update_guild_role(
            "guild/id",
            "role/id",
            &GuildRoleMutation {
                name: Some("moderator".to_owned()),
                ..GuildRoleMutation::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.guild_id, "guild/id");
    assert!(matches!(
        api.update_guild_role(
            "guild/id",
            "missing-guild",
            &GuildRoleMutation {
                name: Some("malformed".to_owned()),
                ..GuildRoleMutation::default()
            },
        )
        .await,
        Err(ApiError::Decode(_))
    ));
    for request in [
        GuildRoleMutation {
            color: Some(123),
            ..GuildRoleMutation::default()
        },
        GuildRoleMutation {
            hoist: Some(0),
            ..GuildRoleMutation::default()
        },
        GuildRoleMutation {
            hoist: Some(1),
            ..GuildRoleMutation::default()
        },
        GuildRoleMutation {
            name: Some("moderator".to_owned()),
            color: Some(123),
            hoist: Some(1),
        },
    ] {
        api.update_guild_role("guild/id", "role/id", &request)
            .await
            .unwrap();
    }
    api.delete_guild_role("guild/id", "role/id").await.unwrap();

    let channel_role_member = GuildRoleMemberRequest::for_channel("channel/id");
    api.add_guild_role_member("guild/id", "user/id", "5", &channel_role_member)
        .await
        .unwrap();
    api.remove_guild_role_member("guild/id", "user/id", "5", &channel_role_member)
        .await
        .unwrap();
    api.add_guild_role_member(
        "guild/id",
        "user/id",
        "2",
        &GuildRoleMemberRequest::default(),
    )
    .await
    .unwrap();
    api.remove_guild_role_member(
        "guild/id",
        "user/id",
        "2",
        &GuildRoleMemberRequest::default(),
    )
    .await
    .unwrap();
    api.add_guild_role_member(
        "guild/id",
        "user/id",
        "2",
        &GuildRoleMemberRequest::for_channel("channel/id"),
    )
    .await
    .unwrap();
    api.remove_guild_role_member(
        "guild/id",
        "user/id",
        "2",
        &GuildRoleMemberRequest::for_channel("channel/id"),
    )
    .await
    .unwrap();

    assert_eq!(
        *observations.member_queries.lock().unwrap(),
        vec![
            ("after/id".to_owned(), "20".to_owned()),
            ("0".to_owned(), "1".to_owned()),
            ("0".to_owned(), "400".to_owned()),
            ("missing-username".to_owned(), "20".to_owned()),
            ("missing-nick".to_owned(), "20".to_owned()),
        ]
    );
    assert_eq!(
        *observations.role_member_queries.lock().unwrap(),
        vec![
            ("next/id".to_owned(), "30".to_owned()),
            ("missing-data".to_owned(), "30".to_owned()),
            ("missing-next".to_owned(), "30".to_owned()),
            ("0".to_owned(), "1".to_owned()),
            ("0".to_owned(), "400".to_owned()),
        ]
    );
    let expected_remove_bodies = [false, true]
        .into_iter()
        .flat_map(|add_blacklist| {
            [-1, 0, 3, 7, 15, 30].into_iter().map(move |days| {
                json!({
                    "add_blacklist":add_blacklist,
                    "delete_history_msg_days":days
                })
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        *observations.remove_member_bodies.lock().unwrap(),
        expected_remove_bodies
    );
    assert_eq!(
        *observations.create_role_bodies.lock().unwrap(),
        vec![
            json!({"color":123}),
            json!({"name":"moderator"}),
            json!({"name":"moderator","color":123,"hoist":1}),
            json!({"hoist":0}),
            json!({"hoist":1}),
        ]
    );
    assert_eq!(
        *observations.update_role_bodies.lock().unwrap(),
        vec![
            json!({"name":"moderator"}),
            json!({"name":"malformed"}),
            json!({"color":123}),
            json!({"hoist":0}),
            json!({"hoist":1}),
            json!({"name":"moderator","color":123,"hoist":1}),
        ]
    );
    let expected_role_member_bodies = vec![
        ("5".to_owned(), json!({"channel":{"id":"channel/id"}})),
        ("2".to_owned(), json!({})),
        ("2".to_owned(), json!({"channel":{"id":"channel/id"}})),
    ];
    assert_eq!(
        *observations.role_member_put_bodies.lock().unwrap(),
        expected_role_member_bodies
    );
    assert_eq!(
        *observations.role_member_delete_bodies.lock().unwrap(),
        expected_role_member_bodies
    );

    server_task.abort();
    let result = server_task.await;
    assert!(result.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn rejects_invalid_member_page_and_removal_requests_before_io() {
    let (api, calls, server_task) = invalid_client().await;
    assert!(matches!(
        api.guild_members(
            "guild/id",
            &GuildMemberPageRequest {
                after: "0".to_owned(),
                limit: 401,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::PageLimitOutOfRange { limit: 401 }
        ))
    ));
    assert!(matches!(
        api.guild_members(
            "guild/id",
            &GuildMemberPageRequest {
                after: "0".to_owned(),
                limit: 0,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::PageLimitOutOfRange { limit: 0 }
        ))
    ));
    assert!(matches!(
        api.guild_members(
            "guild/id",
            &GuildMemberPageRequest {
                after: String::new(),
                limit: 20,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::EmptyPageCursor
        ))
    ));
    assert!(matches!(
        api.guild_role_members(
            "guild/id",
            "role/id",
            &GuildRoleMemberPageRequest {
                start_index: String::new(),
                limit: 20,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::EmptyPageCursor
        ))
    ));
    assert!(matches!(
        api.guild_role_members(
            "guild/id",
            "role/id",
            &GuildRoleMemberPageRequest {
                start_index: "0".to_owned(),
                limit: 0,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::PageLimitOutOfRange { limit: 0 }
        ))
    ));
    assert!(matches!(
        api.guild_role_members(
            "guild/id",
            "role/id",
            &GuildRoleMemberPageRequest {
                start_index: "0".to_owned(),
                limit: 401,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::PageLimitOutOfRange { limit: 401 }
        ))
    ));
    assert!(matches!(
        api.remove_guild_member(
            "guild/id",
            "user/id",
            &RemoveGuildMemberRequest {
                add_blacklist: false,
                delete_history_msg_days: 2,
            },
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::InvalidHistoryDeletionDays { days: 2 }
        ))
    ));
    assert!(matches!(
        api.create_guild_role("guild/id", &GuildRoleMutation::default())
            .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::EmptyRoleMutation
        ))
    ));
    assert!(matches!(
        api.update_guild_role("guild/id", "role/id", &GuildRoleMutation::default())
            .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::EmptyRoleMutation
        ))
    ));
    for (request, expected) in [
        (
            GuildRoleMutation {
                name: Some(" ".to_owned()),
                ..GuildRoleMutation::default()
            },
            GuildRequestValidationError::EmptyRoleName,
        ),
        (
            GuildRoleMutation {
                hoist: Some(2),
                ..GuildRoleMutation::default()
            },
            GuildRequestValidationError::InvalidRoleHoist { hoist: 2 },
        ),
    ] {
        assert!(matches!(
            api.create_guild_role("guild/id", &request).await,
            Err(ApiError::InvalidGuildRequest(error)) if error == expected
        ));
        assert!(matches!(
            api.update_guild_role("guild/id", "role/id", &request).await,
            Err(ApiError::InvalidGuildRequest(error)) if error == expected
        ));
    }
    assert!(matches!(
        api.add_guild_role_member(
            "guild/id",
            "user/id",
            "5",
            &GuildRoleMemberRequest::default(),
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::MissingChannelForRoleFive
        ))
    ));
    assert!(matches!(
        api.remove_guild_role_member(
            "guild/id",
            "user/id",
            "5",
            &GuildRoleMemberRequest::default(),
        )
        .await,
        Err(ApiError::InvalidGuildRequest(
            GuildRequestValidationError::MissingChannelForRoleFive
        ))
    ));
    let blank_channel = GuildRoleMemberRequest::for_channel(" ");
    for role_id in ["5", "2"] {
        assert!(matches!(
            api.add_guild_role_member("guild/id", "user/id", role_id, &blank_channel)
                .await,
            Err(ApiError::InvalidGuildRequest(
                GuildRequestValidationError::EmptyRoleMemberChannelId
            ))
        ));
        assert!(matches!(
            api.remove_guild_role_member("guild/id", "user/id", role_id, &blank_channel)
                .await,
            Err(ApiError::InvalidGuildRequest(
                GuildRequestValidationError::EmptyRoleMemberChannelId
            ))
        ));
    }
    macro_rules! assert_invalid_path {
        ($call:expr) => {
            assert!(matches!($call.await, Err(ApiError::InvalidRequest(_))));
        };
    }
    let member_page = GuildMemberPageRequest::default();
    let role_member_page = GuildRoleMemberPageRequest::default();
    let role_mutation = GuildRoleMutation {
        name: Some("moderator".to_owned()),
        ..GuildRoleMutation::default()
    };
    let member_removal = RemoveGuildMemberRequest::default();
    let role_member = GuildRoleMemberRequest::default();
    assert_invalid_path!(api.channel_online_member_count(" "));
    assert_invalid_path!(api.guild_members(" ", &member_page));
    assert_invalid_path!(api.guild_role_members(" ", "role/id", &role_member_page));
    assert_invalid_path!(api.guild_role_members("guild/id", " ", &role_member_page));
    assert_invalid_path!(api.guild_member(" ", "user/id"));
    assert_invalid_path!(api.guild_member("guild/id", " "));
    assert_invalid_path!(api.remove_guild_member(" ", "user/id", &member_removal));
    assert_invalid_path!(api.remove_guild_member("guild/id", " ", &member_removal));
    assert_invalid_path!(api.guild_roles(" "));
    assert_invalid_path!(api.create_guild_role(" ", &role_mutation));
    assert_invalid_path!(api.update_guild_role(" ", "role/id", &role_mutation));
    assert_invalid_path!(api.update_guild_role("guild/id", " ", &role_mutation));
    assert_invalid_path!(api.delete_guild_role(" ", "role/id"));
    assert_invalid_path!(api.delete_guild_role("guild/id", " "));
    assert_invalid_path!(api.add_guild_role_member(" ", "user/id", "2", &role_member));
    assert_invalid_path!(api.add_guild_role_member("guild/id", " ", "2", &role_member));
    assert_invalid_path!(api.add_guild_role_member("guild/id", "user/id", " ", &role_member));
    assert_invalid_path!(api.remove_guild_role_member(" ", "user/id", "2", &role_member));
    assert_invalid_path!(api.remove_guild_role_member("guild/id", " ", "2", &role_member));
    assert_invalid_path!(api.remove_guild_role_member("guild/id", "user/id", " ", &role_member));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    let result = server_task.await;
    assert!(result.is_err_and(|error| error.is_cancelled()));
}
