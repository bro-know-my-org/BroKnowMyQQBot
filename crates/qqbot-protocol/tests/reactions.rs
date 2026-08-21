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
    http::{Method, StatusCode},
    routing::{get, post, put},
};
use qqbot_protocol::{
    ApiError, OpenApiClient, ReactionEmoji, ReactionUsersRequest, ReactionValidationError,
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
struct Observations {
    methods: Mutex<Vec<Method>>,
    queries: Mutex<Vec<HashMap<String, String>>>,
}

async fn mutate_reaction(
    State(observations): State<Arc<Observations>>,
    method: Method,
    Path((channel, message, emoji_type, emoji_id)): Path<(String, String, u32, String)>,
) -> StatusCode {
    assert_eq!(channel, "channel/id");
    assert_eq!(message, "message/id");
    assert!(matches!(
        (emoji_type, emoji_id.as_str()),
        (1, "203") | (2, "😀")
    ));
    observations.methods.lock().unwrap().push(method);
    StatusCode::NO_CONTENT
}

async fn reaction_users(
    State(observations): State<Arc<Observations>>,
    Path((channel, message, emoji_type, emoji_id)): Path<(String, String, u32, String)>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(channel, "channel/id");
    assert_eq!(message, "message/id");
    assert_eq!(emoji_type, 1);
    assert_eq!(emoji_id, "203");
    observations.queries.lock().unwrap().push(query.clone());
    match query.get("cookie").map(String::as_str) {
        Some("missing-users") => Json(json!({"cookie":"next","is_end":false})),
        Some("missing-cookie") => Json(json!({"users":[],"is_end":false})),
        Some("missing-is-end") => Json(json!({"users":[],"cookie":"next"})),
        _ => Json(json!({
            "users":[{"id":"user/id","username":"member","avatar":"avatar"}],
            "cookie":"next",
            "is_end":false
        })),
    }
}

async fn client() -> (OpenApiClient, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route(
            "/channels/{channel}/messages/{message}/reactions/{emoji_type}/{emoji_id}",
            put(mutate_reaction)
                .delete(mutate_reaction)
                .get(reaction_users),
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
async fn calls_reaction_endpoints_with_exact_paths_and_queries() {
    let (api, observations, server_task) = client().await;
    let emoji = ReactionEmoji {
        id: "203".to_owned(),
        emoji_type: 1,
    };
    api.add_channel_message_reaction("channel/id", "message/id", &emoji)
        .await
        .unwrap();
    api.remove_channel_message_reaction("channel/id", "message/id", &emoji)
        .await
        .unwrap();
    api.add_channel_message_reaction(
        "channel/id",
        "message/id",
        &ReactionEmoji {
            id: "😀".to_owned(),
            emoji_type: 2,
        },
    )
    .await
    .unwrap();
    let first = api
        .channel_message_reaction_users(
            "channel/id",
            "message/id",
            &emoji,
            &ReactionUsersRequest {
                cookie: None,
                limit: Some(50),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.users[0].id, "user/id");
    api.channel_message_reaction_users(
        "channel/id",
        "message/id",
        &emoji,
        &ReactionUsersRequest {
            cookie: Some("next".to_owned()),
            limit: None,
        },
    )
    .await
    .unwrap();
    for cookie in ["missing-users", "missing-cookie", "missing-is-end"] {
        assert!(matches!(
            api.channel_message_reaction_users(
                "channel/id",
                "message/id",
                &emoji,
                &ReactionUsersRequest {
                    cookie: Some(cookie.to_owned()),
                    limit: None,
                },
            )
            .await,
            Err(ApiError::Decode(_))
        ));
    }
    assert_eq!(
        *observations.methods.lock().unwrap(),
        vec![Method::PUT, Method::DELETE, Method::PUT]
    );
    assert_eq!(
        *observations.queries.lock().unwrap(),
        vec![
            HashMap::from([("limit".to_owned(), "50".to_owned())]),
            HashMap::from([("cookie".to_owned(), "next".to_owned())]),
            HashMap::from([("cookie".to_owned(), "missing-users".to_owned())]),
            HashMap::from([("cookie".to_owned(), "missing-cookie".to_owned())]),
            HashMap::from([("cookie".to_owned(), "missing-is-end".to_owned())]),
        ]
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
            "/channels/{channel}/messages/{message}/reactions/{emoji_type}/{emoji_id}",
            get(counted).put(counted).delete(counted),
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
async fn rejects_invalid_reaction_requests_before_io() {
    let (api, calls, server_task) = invalid_client().await;
    let valid = ReactionEmoji {
        id: "203".to_owned(),
        emoji_type: 1,
    };
    assert!(matches!(
        api.add_channel_message_reaction(" ", "message/id", &valid)
            .await,
        Err(ApiError::InvalidRequest(message)) if message.contains("channel_id")
    ));
    assert!(matches!(
        api.add_channel_message_reaction("channel/id", " ", &valid)
            .await,
        Err(ApiError::InvalidRequest(message)) if message.contains("message_id")
    ));
    assert!(matches!(
        api.add_channel_message_reaction(
            "channel/id",
            "message/id",
            &ReactionEmoji {
                id: " ".to_owned(),
                emoji_type: 1,
            },
        )
        .await,
        Err(ApiError::InvalidReactionRequest(
            ReactionValidationError::EmptyField { field: "emoji.id" }
        ))
    ));
    for (request, expected) in [
        (
            ReactionUsersRequest {
                cookie: None,
                limit: Some(0),
            },
            ReactionValidationError::PageLimitOutOfRange { limit: 0 },
        ),
        (
            ReactionUsersRequest {
                cookie: None,
                limit: Some(51),
            },
            ReactionValidationError::PageLimitOutOfRange { limit: 51 },
        ),
        (
            ReactionUsersRequest {
                cookie: Some("next".to_owned()),
                limit: Some(20),
            },
            ReactionValidationError::LimitWithCookie,
        ),
        (
            ReactionUsersRequest {
                cookie: Some(" ".to_owned()),
                limit: None,
            },
            ReactionValidationError::EmptyField { field: "cookie" },
        ),
    ] {
        assert!(matches!(
            api.channel_message_reaction_users("channel/id", "message/id", &valid, &request).await,
            Err(ApiError::InvalidReactionRequest(actual)) if actual == expected
        ));
    }
    for emoji_type in [0, 3] {
        let emoji = ReactionEmoji {
            id: "203".to_owned(),
            emoji_type,
        };
        assert!(matches!(
            api.remove_channel_message_reaction("channel/id", "message/id", &emoji).await,
            Err(ApiError::InvalidReactionRequest(ReactionValidationError::InvalidEmojiType { emoji_type: actual })) if actual == emoji_type
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
