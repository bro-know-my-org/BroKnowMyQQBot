use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post, put},
};
use bot_core::{Action, Adapter, AdapterError};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};
use url::Url;

async fn token() -> Json<Value> {
    Json(json!({"access_token":"token","expires_in":7200}))
}

fn member() -> Value {
    json!({
        "user":{"id":"user/id","username":"member","avatar":"","bot":false},
        "nick":"member nick",
        "roles":["role/id"],
        "joined_at":"2026-08-22T10:00:00+08:00"
    })
}

fn role() -> Value {
    json!({
        "id":"role/id",
        "name":"moderator",
        "color":123,
        "hoist":1,
        "number":2,
        "member_limit":2000
    })
}

async fn online_count(Path(channel): Path<String>) -> Json<Value> {
    assert_eq!(channel, "channel/id");
    Json(json!({"online_nums":9}))
}

async fn members(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path(guild): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    observations.member_queries.lock().unwrap().push((
        query.get("after").cloned().unwrap(),
        query.get("limit").cloned().unwrap(),
    ));
    Json(json!([member()]))
}

async fn role_members(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path((guild, role_id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    assert_eq!(role_id, "role/id");
    observations.role_member_queries.lock().unwrap().push((
        query.get("start_index").cloned().unwrap(),
        query.get("limit").cloned().unwrap(),
    ));
    match query.get("start_index").map(String::as_str) {
        Some("missing-data") => Json(json!({"next":"next"})),
        Some("missing-next") => Json(json!({"data":[]})),
        _ => Json(json!({"data":[member()],"next":"next"})),
    }
}

async fn get_member(Path((guild, user)): Path<(String, String)>) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    Json(member())
}

async fn remove_member(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path((guild, user)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    observations.remove_member_bodies.lock().unwrap().push(body);
    StatusCode::NO_CONTENT
}

async fn roles(Path(guild): Path<String>) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    Json(json!({"guild_id":"guild/id","roles":[role()],"role_num_limit":"30"}))
}

async fn create_role(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path(guild): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    observations.create_role_bodies.lock().unwrap().push(body);
    Json(json!({"role_id":"role/id","role":role()}))
}

async fn update_role(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path((guild, role_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(guild, "guild/id");
    assert_eq!(role_id, "role/id");
    observations.update_role_bodies.lock().unwrap().push(body);
    Json(json!({"guild_id":"guild/id","role_id":"role/id","role":role()}))
}

async fn delete_role(Path((guild, role_id)): Path<(String, String)>) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(role_id, "role/id");
    StatusCode::NO_CONTENT
}

#[derive(Default)]
struct GuildManagementObservations {
    puts: AtomicUsize,
    deletes: AtomicUsize,
    member_queries: Mutex<Vec<(String, String)>>,
    role_member_queries: Mutex<Vec<(String, String)>>,
    remove_member_bodies: Mutex<Vec<Value>>,
    create_role_bodies: Mutex<Vec<Value>>,
    update_role_bodies: Mutex<Vec<Value>>,
    role_member_put_bodies: Mutex<Vec<(String, Value)>>,
    role_member_delete_bodies: Mutex<Vec<(String, Value)>>,
    member_permission_updates: Mutex<Vec<Value>>,
    role_permission_updates: Mutex<Vec<Value>>,
}

async fn add_role_member(
    State(counts): State<Arc<GuildManagementObservations>>,
    Path((guild, user, role_id)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    counts
        .role_member_put_bodies
        .lock()
        .unwrap()
        .push((role_id, body));
    counts.puts.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn remove_role_member(
    State(counts): State<Arc<GuildManagementObservations>>,
    Path((guild, user, role_id)): Path<(String, String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(guild, "guild/id");
    assert_eq!(user, "user/id");
    counts
        .role_member_delete_bodies
        .lock()
        .unwrap()
        .push((role_id, body));
    counts.deletes.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn member_permissions(Path((channel, user)): Path<(String, String)>) -> Json<Value> {
    assert_eq!(channel, "channel/id");
    assert_eq!(user, "user/id");
    Json(json!({
        "channel_id":"channel/id",
        "user_id":"user/id",
        "permissions":"4"
    }))
}

async fn update_member_permissions(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path((channel, user)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(channel, "channel/id");
    assert_eq!(user, "user/id");
    observations
        .member_permission_updates
        .lock()
        .unwrap()
        .push(body);
    StatusCode::NO_CONTENT
}

async fn role_permissions(Path((channel, role)): Path<(String, String)>) -> Json<Value> {
    assert_eq!(channel, "channel/id");
    assert_eq!(role, "role/id");
    Json(json!({
        "channel_id":"channel/id",
        "role_id":"role/id",
        "permissions":"5"
    }))
}

async fn update_role_permissions(
    State(observations): State<Arc<GuildManagementObservations>>,
    Path((channel, role)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(channel, "channel/id");
    assert_eq!(role, "role/id");
    observations
        .role_permission_updates
        .lock()
        .unwrap()
        .push(body);
    StatusCode::NO_CONTENT
}

async fn adapter() -> (
    QqWebSocketAdapter,
    JoinHandle<()>,
    Arc<GuildManagementObservations>,
) {
    let observations = Arc::new(GuildManagementObservations::default());
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
        .route(
            "/channels/{channel}/members/{user}/permissions",
            get(member_permissions).put(update_member_permissions),
        )
        .route(
            "/channels/{channel}/roles/{role}/permissions",
            get(role_permissions).put(update_role_permissions),
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
        server_task,
        observations,
    )
}

async fn counted_token(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    token().await
}

async fn counted_unit(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
    calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn invalid_adapter() -> (QqWebSocketAdapter, JoinHandle<()>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/app/getAppAccessToken", post(counted_token))
        .route(
            "/guilds/{guild}/members/{user}/roles/{role}",
            put(counted_unit).delete(counted_unit),
        )
        .route(
            "/channels/{channel}/members/{user}/permissions",
            put(counted_unit),
        )
        .route(
            "/channels/{channel}/roles/{role}/permissions",
            put(counted_unit),
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
        QqWebSocketAdapter::new(
            QqWebSocketConfig::default(),
            OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        ),
        server_task,
        calls,
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
async fn exposes_channel_permission_actions() {
    let (adapter, server_task, observations) = adapter().await;
    let member = platform(
        &adapter,
        "qq.channel.member.permission.get",
        json!({"channel_id":"channel/id","user_id":"user/id"}),
    )
    .await;
    assert_eq!(member["permissions"], "4");
    assert_eq!(member["user_id"], "user/id");
    let role = platform(
        &adapter,
        "qq.channel.role.permission.get",
        json!({"channel_id":"channel/id","role_id":"role/id"}),
    )
    .await;
    assert_eq!(role["permissions"], "5");
    assert_eq!(role["role_id"], "role/id");

    for (name, target) in [
        ("qq.channel.member.permission.update", "user_id"),
        ("qq.channel.role.permission.update", "role_id"),
    ] {
        let mut payload = json!({"channel_id":"channel/id","add":"5","remove":"4"});
        payload[target] = json!(if target == "user_id" {
            "user/id"
        } else {
            "role/id"
        });
        assert_eq!(platform(&adapter, name, payload).await, Value::Null);
    }
    assert_eq!(
        *observations.member_permission_updates.lock().unwrap(),
        vec![json!({"add":"5","remove":"4"})]
    );
    assert_eq!(
        *observations.role_permission_updates.lock().unwrap(),
        vec![json!({"add":"5","remove":"4"})]
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_channel_permission_actions_before_io() {
    let (adapter, server_task, calls) = invalid_adapter().await;
    let cases = [
        (
            "qq.channel.member.permission.get",
            json!({"channel_id":" ","user_id":"user/id"}),
            "`channel_id`",
        ),
        (
            "qq.channel.member.permission.get",
            json!({"channel_id":"channel/id","user_id":" "}),
            "`user_id`",
        ),
        (
            "qq.channel.role.permission.get",
            json!({"channel_id":"channel/id","role_id":" "}),
            "`role_id`",
        ),
        (
            "qq.channel.member.permission.update",
            json!({"channel_id":"channel/id","user_id":"user/id","add":"-1","remove":"0"}),
            "`add`",
        ),
        (
            "qq.channel.role.permission.update",
            json!({"channel_id":"channel/id","role_id":"role/id","add":"0","remove":" "}),
            "`remove`",
        ),
        (
            "qq.channel.member.permission.update",
            json!({"channel_id":"channel/id","user_id":"user/id","add":"2","remove":"0"}),
            "manage-channel bit",
        ),
        (
            "qq.channel.role.permission.update",
            json!({"channel_id":"channel/id","role_id":"role/id","add":"0","remove":"2"}),
            "manage-channel bit",
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
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn exposes_guild_member_and_role_actions() {
    let (adapter, server_task, observations) = adapter().await;

    assert_eq!(
        platform(
            &adapter,
            "qq.channel.online-count",
            json!({"channel_id":"channel/id"}),
        )
        .await["online_nums"],
        9
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.member.list",
            json!({"guild_id":"guild/id","after":"0","limit":25}),
        )
        .await[0]["user"]["id"],
        "user/id"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.role.member.list",
            json!({"guild_id":"guild/id","role_id":"role/id","start_index":"0","limit":10}),
        )
        .await["next"],
        "next"
    );
    for start_index in ["missing-data", "missing-next"] {
        let error = adapter
            .execute(Action::Platform {
                name: "qq.guild.role.member.list".to_owned(),
                payload: json!({
                    "guild_id":"guild/id",
                    "role_id":"role/id",
                    "start_index":start_index,
                    "limit":10
                }),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AdapterError::ActionUnknown(_)));
    }
    platform(
        &adapter,
        "qq.guild.member.list",
        json!({"guild_id":"guild/id","after":"after/id","limit":25}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.member.list",
        json!({
            "guild_id":"guild/id",
            "role_id":"role/id",
            "start_index":"next/id",
            "limit":10
        }),
    )
    .await;
    for limit in [1, 400] {
        assert_eq!(
            platform(
                &adapter,
                "qq.guild.member.list",
                json!({"guild_id":"guild/id","after":"0","limit":limit}),
            )
            .await[0]["user"]["id"],
            "user/id"
        );
        assert_eq!(
            platform(
                &adapter,
                "qq.guild.role.member.list",
                json!({
                    "guild_id":"guild/id",
                    "role_id":"role/id",
                    "start_index":"0",
                    "limit":limit
                }),
            )
            .await["next"],
            "next"
        );
    }
    assert_eq!(
        *observations.member_queries.lock().unwrap(),
        vec![
            ("0".to_owned(), "25".to_owned()),
            ("after/id".to_owned(), "25".to_owned()),
            ("0".to_owned(), "1".to_owned()),
            ("0".to_owned(), "400".to_owned()),
        ]
    );
    assert_eq!(
        *observations.role_member_queries.lock().unwrap(),
        vec![
            ("0".to_owned(), "10".to_owned()),
            ("missing-data".to_owned(), "10".to_owned()),
            ("missing-next".to_owned(), "10".to_owned()),
            ("next/id".to_owned(), "10".to_owned()),
            ("0".to_owned(), "1".to_owned()),
            ("0".to_owned(), "400".to_owned()),
        ]
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.member.get",
            json!({"guild_id":"guild/id","user_id":"user/id"}),
        )
        .await["nick"],
        "member nick"
    );
    for delete_history_msg_days in [-1, 0, 3, 7, 15, 30] {
        platform(
            &adapter,
            "qq.guild.member.remove",
            json!({
                "guild_id":"guild/id",
                "user_id":"user/id",
                "add_blacklist":true,
                "delete_history_msg_days":delete_history_msg_days
            }),
        )
        .await;
    }
    platform(
        &adapter,
        "qq.guild.member.remove",
        json!({
            "guild_id":"guild/id",
            "user_id":"user/id",
            "add_blacklist":false,
            "delete_history_msg_days":0
        }),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.member.remove",
        json!({"guild_id":"guild/id","user_id":"user/id"}),
    )
    .await;
    let mut expected_remove_bodies = [-1, 0, 3, 7, 15, 30]
        .into_iter()
        .map(|days| json!({"add_blacklist":true,"delete_history_msg_days":days}))
        .collect::<Vec<_>>();
    expected_remove_bodies.push(json!({
        "add_blacklist":false,
        "delete_history_msg_days":0
    }));
    expected_remove_bodies.push(json!({
        "add_blacklist":false,
        "delete_history_msg_days":0
    }));
    assert_eq!(
        *observations.remove_member_bodies.lock().unwrap(),
        expected_remove_bodies
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.role.list",
            json!({"guild_id":"guild/id"}),
        )
        .await["roles"][0]["id"],
        "role/id"
    );
    platform(
        &adapter,
        "qq.guild.role.create",
        json!({"guild_id":"guild/id","name":"moderator","color":123,"hoist":1}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.create",
        json!({"guild_id":"guild/id","name":"moderator"}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.create",
        json!({"guild_id":"guild/id","hoist":0}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.create",
        json!({"guild_id":"guild/id","hoist":1}),
    )
    .await;
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.role.create",
            json!({"guild_id":"guild/id","color":123}),
        )
        .await["role_id"],
        "role/id"
    );
    assert_eq!(
        platform(
            &adapter,
            "qq.guild.role.update",
            json!({"guild_id":"guild/id","role_id":"role/id","name":"moderator"}),
        )
        .await["guild_id"],
        "guild/id"
    );
    platform(
        &adapter,
        "qq.guild.role.update",
        json!({"guild_id":"guild/id","role_id":"role/id","color":123}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.update",
        json!({"guild_id":"guild/id","role_id":"role/id","hoist":0}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.update",
        json!({"guild_id":"guild/id","role_id":"role/id","hoist":1}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.update",
        json!({
            "guild_id":"guild/id",
            "role_id":"role/id",
            "name":"moderator",
            "color":123,
            "hoist":1
        }),
    )
    .await;
    assert_eq!(
        *observations.create_role_bodies.lock().unwrap(),
        vec![
            json!({"name":"moderator","color":123,"hoist":1}),
            json!({"name":"moderator"}),
            json!({"hoist":0}),
            json!({"hoist":1}),
            json!({"color":123}),
        ]
    );
    assert_eq!(
        *observations.update_role_bodies.lock().unwrap(),
        vec![
            json!({"name":"moderator"}),
            json!({"color":123}),
            json!({"hoist":0}),
            json!({"hoist":1}),
            json!({"name":"moderator","color":123,"hoist":1}),
        ]
    );
    platform(
        &adapter,
        "qq.guild.role.delete",
        json!({"guild_id":"guild/id","role_id":"role/id"}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.member.add",
        json!({
            "guild_id":"guild/id",
            "user_id":"user/id",
            "role_id":"5",
            "channel_id":"channel/id"
        }),
    )
    .await;
    assert_eq!(observations.puts.load(Ordering::SeqCst), 1);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 0);
    platform(
        &adapter,
        "qq.guild.role.member.remove",
        json!({
            "guild_id":"guild/id",
            "user_id":"user/id",
            "role_id":"5",
            "channel_id":"channel/id"
        }),
    )
    .await;
    assert_eq!(observations.puts.load(Ordering::SeqCst), 1);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 1);

    platform(
        &adapter,
        "qq.guild.role.member.add",
        json!({"guild_id":"guild/id","user_id":"user/id","role_id":"2"}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.member.remove",
        json!({"guild_id":"guild/id","user_id":"user/id","role_id":"2"}),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.member.add",
        json!({
            "guild_id":"guild/id",
            "user_id":"user/id",
            "role_id":"2",
            "channel_id":"channel/id"
        }),
    )
    .await;
    platform(
        &adapter,
        "qq.guild.role.member.remove",
        json!({
            "guild_id":"guild/id",
            "user_id":"user/id",
            "role_id":"2",
            "channel_id":"channel/id"
        }),
    )
    .await;
    assert_eq!(observations.puts.load(Ordering::SeqCst), 3);
    assert_eq!(observations.deletes.load(Ordering::SeqCst), 3);
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
async fn rejects_invalid_guild_management_payloads_before_io() {
    let (adapter, server_task, calls) = invalid_adapter().await;
    let cases = [
        (
            "qq.channel.online-count",
            json!({"channel_id":" "}),
            "`channel_id`",
        ),
        (
            "qq.guild.member.get",
            json!({"guild_id":" ","user_id":"user/id"}),
            "`guild_id`",
        ),
        (
            "qq.guild.member.get",
            json!({"guild_id":"guild/id","user_id":" "}),
            "`user_id`",
        ),
        (
            "qq.guild.role.member.list",
            json!({"guild_id":"guild/id","role_id":" ","start_index":"0","limit":1}),
            "`role_id`",
        ),
        (
            "qq.guild.role.member.add",
            json!({"guild_id":"guild/id","user_id":"user/id","role_id":"5"}),
            "requires a channel id",
        ),
        (
            "qq.guild.role.member.remove",
            json!({"guild_id":"guild/id","user_id":"user/id","role_id":"5"}),
            "requires a channel id",
        ),
        (
            "qq.guild.role.member.add",
            json!({
                "guild_id":"guild/id",
                "user_id":"user/id",
                "role_id":"5",
                "channel_id":" "
            }),
            "channel id must not be empty",
        ),
        (
            "qq.guild.role.member.remove",
            json!({
                "guild_id":"guild/id",
                "user_id":"user/id",
                "role_id":"5",
                "channel_id":" "
            }),
            "channel id must not be empty",
        ),
        (
            "qq.guild.role.member.add",
            json!({
                "guild_id":"guild/id",
                "user_id":"user/id",
                "role_id":"2",
                "channel_id":" "
            }),
            "channel id must not be empty",
        ),
        (
            "qq.guild.role.member.remove",
            json!({
                "guild_id":"guild/id",
                "user_id":"user/id",
                "role_id":"2",
                "channel_id":" "
            }),
            "channel id must not be empty",
        ),
        (
            "qq.guild.role.create",
            json!({"guild_id":"guild/id"}),
            "at least one field",
        ),
        (
            "qq.guild.role.update",
            json!({"guild_id":"guild/id","role_id":"role/id"}),
            "at least one field",
        ),
        (
            "qq.guild.role.create",
            json!({"guild_id":"guild/id","hoist":2}),
            "hoist must be 0 or 1",
        ),
        (
            "qq.guild.role.update",
            json!({"guild_id":"guild/id","role_id":"role/id","hoist":2}),
            "hoist must be 0 or 1",
        ),
        (
            "qq.guild.role.create",
            json!({"guild_id":"guild/id","name":" "}),
            "name must not be empty",
        ),
        (
            "qq.guild.role.update",
            json!({"guild_id":"guild/id","role_id":"role/id","name":" "}),
            "name must not be empty",
        ),
        (
            "qq.guild.member.list",
            json!({"guild_id":"guild/id","after":"","limit":20}),
            "cursor must not be empty",
        ),
        (
            "qq.guild.member.list",
            json!({"guild_id":"guild/id","after":"0","limit":0}),
            "limit must be between 1 and 400",
        ),
        (
            "qq.guild.member.list",
            json!({"guild_id":"guild/id","after":"0","limit":401}),
            "limit must be between 1 and 400",
        ),
        (
            "qq.guild.role.member.list",
            json!({"guild_id":"guild/id","role_id":"role/id","start_index":"","limit":20}),
            "cursor must not be empty",
        ),
        (
            "qq.guild.role.member.list",
            json!({"guild_id":"guild/id","role_id":"role/id","start_index":"0","limit":0}),
            "limit must be between 1 and 400",
        ),
        (
            "qq.guild.role.member.list",
            json!({"guild_id":"guild/id","role_id":"role/id","start_index":"0","limit":401}),
            "limit must be between 1 and 400",
        ),
        (
            "qq.guild.member.remove",
            json!({
                "guild_id":"guild/id",
                "user_id":"user/id",
                "delete_history_msg_days":2
            }),
            "deletion days must be -1, 0, 3, 7, 15, or 30",
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
        assert!(
            message.contains(expected),
            "expected {name} error `{message}` to contain `{expected}`"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    let result = server_task.await;
    assert!(result.is_err_and(|error| error.is_cancelled()));
}
