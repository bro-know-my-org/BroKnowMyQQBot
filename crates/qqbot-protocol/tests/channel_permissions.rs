use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use qqbot_protocol::{
    ApiError, ChannelPermissionField, ChannelPermissionValidationError, OpenApiClient,
    TokenManager, UpdateChannelPermissionsRequest,
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
struct PermissionObservations {
    member_updates: Mutex<Vec<Value>>,
    role_updates: Mutex<Vec<Value>>,
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
    State(observations): State<Arc<PermissionObservations>>,
    Path((channel, user)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(channel, "channel/id");
    assert_eq!(user, "user/id");
    observations.member_updates.lock().unwrap().push(body);
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
    State(observations): State<Arc<PermissionObservations>>,
    Path((channel, role)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> StatusCode {
    assert_eq!(channel, "channel/id");
    assert_eq!(role, "role/id");
    observations.role_updates.lock().unwrap().push(body);
    StatusCode::NO_CONTENT
}

async fn client() -> (OpenApiClient, Arc<PermissionObservations>, JoinHandle<()>) {
    let observations = Arc::new(PermissionObservations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
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
        OpenApiClient::with_base_url(base_url, tokens).unwrap(),
        observations,
        server_task,
    )
}

#[tokio::test]
async fn calls_channel_member_and_role_permission_endpoints() {
    let (api, observations, server_task) = client().await;
    let member = api
        .channel_member_permissions("channel/id", "user/id")
        .await
        .unwrap();
    assert_eq!(member.channel_id, "channel/id");
    assert_eq!(member.user_id, "user/id");
    assert_eq!(member.permissions, "4");

    let role = api
        .channel_role_permissions("channel/id", "role/id")
        .await
        .unwrap();
    assert_eq!(role.channel_id, "channel/id");
    assert_eq!(role.role_id, "role/id");
    assert_eq!(role.permissions, "5");

    let request = UpdateChannelPermissionsRequest {
        add: "5".to_owned(),
        remove: "4".to_owned(),
    };
    api.update_channel_member_permissions("channel/id", "user/id", &request)
        .await
        .unwrap();
    api.update_channel_role_permissions("channel/id", "role/id", &request)
        .await
        .unwrap();
    assert_eq!(
        *observations.member_updates.lock().unwrap(),
        vec![json!({"add":"5","remove":"4"})]
    );
    assert_eq!(
        *observations.role_updates.lock().unwrap(),
        vec![json!({"add":"5","remove":"4"})]
    );

    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

async fn counted(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
    calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn invalid_client() -> (OpenApiClient, Arc<AtomicUsize>, JoinHandle<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/app/getAppAccessToken", post(counted))
        .route(
            "/channels/{channel}/members/{user}/permissions",
            put(counted),
        )
        .route("/channels/{channel}/roles/{role}/permissions", put(counted))
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
async fn rejects_invalid_channel_permission_requests_before_io() {
    let (api, calls, server_task) = invalid_client().await;
    let valid = UpdateChannelPermissionsRequest {
        add: "1".to_owned(),
        remove: "4".to_owned(),
    };
    for result in [
        api.channel_member_permissions(" ", "user/id").await.err(),
        api.channel_member_permissions("channel/id", " ")
            .await
            .err(),
        api.channel_role_permissions(" ", "role/id").await.err(),
        api.channel_role_permissions("channel/id", " ").await.err(),
        api.update_channel_member_permissions(" ", "user/id", &valid)
            .await
            .err(),
        api.update_channel_member_permissions("channel/id", " ", &valid)
            .await
            .err(),
        api.update_channel_role_permissions(" ", "role/id", &valid)
            .await
            .err(),
        api.update_channel_role_permissions("channel/id", " ", &valid)
            .await
            .err(),
    ] {
        assert!(matches!(result, Some(ApiError::InvalidRequest(_))));
    }

    for (request, expected) in [
        (
            UpdateChannelPermissionsRequest {
                add: String::new(),
                remove: "0".to_owned(),
            },
            ChannelPermissionValidationError::InvalidBitmap {
                field: ChannelPermissionField::Add,
            },
        ),
        (
            UpdateChannelPermissionsRequest {
                add: "0".to_owned(),
                remove: "-1".to_owned(),
            },
            ChannelPermissionValidationError::InvalidBitmap {
                field: ChannelPermissionField::Remove,
            },
        ),
        (
            UpdateChannelPermissionsRequest {
                add: "abc".to_owned(),
                remove: "0".to_owned(),
            },
            ChannelPermissionValidationError::InvalidBitmap {
                field: ChannelPermissionField::Add,
            },
        ),
        (
            UpdateChannelPermissionsRequest {
                add: "18446744073709551616".to_owned(),
                remove: "0".to_owned(),
            },
            ChannelPermissionValidationError::BitmapOverflow {
                field: ChannelPermissionField::Add,
            },
        ),
        (
            UpdateChannelPermissionsRequest {
                add: "2".to_owned(),
                remove: "0".to_owned(),
            },
            ChannelPermissionValidationError::ManageChannelPermission {
                field: ChannelPermissionField::Add,
            },
        ),
        (
            UpdateChannelPermissionsRequest {
                add: "0".to_owned(),
                remove: "2".to_owned(),
            },
            ChannelPermissionValidationError::ManageChannelPermission {
                field: ChannelPermissionField::Remove,
            },
        ),
    ] {
        for error in [
            api.update_channel_member_permissions("channel/id", "user/id", &request)
                .await
                .unwrap_err(),
            api.update_channel_role_permissions("channel/id", "role/id", &request)
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(
                error,
                ApiError::InvalidChannelPermissionRequest(actual) if actual == expected
            ));
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
