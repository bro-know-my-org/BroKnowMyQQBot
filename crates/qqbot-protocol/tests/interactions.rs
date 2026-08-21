use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{post, put},
};
use qqbot_protocol::{
    ApiError, InteractionEvent, InteractionResponseRequest, InteractionValidationError,
    OpenApiClient, TokenManager,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use url::Url;

#[derive(Default)]
struct Observations {
    token_calls: AtomicUsize,
    fail_token_after_first: AtomicBool,
    requests: Mutex<Vec<(String, Value)>>,
}

async fn token(State(observations): State<Arc<Observations>>) -> Response {
    let call = observations.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
    if call > 1 && observations.fail_token_after_first.load(Ordering::SeqCst) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"message":"token refresh failed"})),
        )
            .into_response();
    }
    Json(json!({"access_token":"token","expires_in":7200})).into_response()
}

async fn respond(
    State(observations): State<Arc<Observations>>,
    Path(interaction_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("QQBot token")
    );
    observations
        .requests
        .lock()
        .unwrap()
        .push((interaction_id.clone(), body));
    match interaction_id.as_str() {
        "interaction-empty" => StatusCode::NO_CONTENT.into_response(),
        "platform-error" => Json(json!({"code":630_003,"message":"appid invalid"})).into_response(),
        "rate-limited" => (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "3")],
            Json(json!({"code":630_008,"message":"preprocess failed"})),
        )
            .into_response(),
        "server-error" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"code":630_004,"message":"set failed"})),
        )
            .into_response(),
        "unauthorized" => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code":630_006,"message":"header appid failed"})),
        )
            .into_response(),
        "redirect" => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/interactions/redirect-target")],
        )
            .into_response(),
        "permanent-redirect" => (
            StatusCode::PERMANENT_REDIRECT,
            [("location", "/interactions/permanent-redirect-target")],
        )
            .into_response(),
        _ => Json(json!({})).into_response(),
    }
}

async fn client() -> (OpenApiClient, Arc<Observations>, JoinHandle<()>) {
    let observations = Arc::new(Observations::default());
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/interactions/{interaction_id}", put(respond))
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

async fn respond_with_timeout(
    api: &OpenApiClient,
    interaction_id: &str,
    request: &InteractionResponseRequest,
) -> Result<(), ApiError> {
    timeout(
        Duration::from_secs(2),
        api.respond_interaction(interaction_id, request),
    )
    .await
    .expect("interaction response timed out")
}

#[test]
fn models_interaction_events_and_response_codes() {
    let event_value = json!({
        "id":"interaction-id",
        "type":11,
        "scene":"group",
        "chat_type":1,
        "timestamp":"2026-08-22T10:00:00+08:00",
        "group_openid":"group-id",
        "group_member_openid":"member-id",
        "data":{
            "type":11,
            "resolved":{"button_data":"approve","future_resolved":true},
            "future_data":"kept"
        },
        "future_top":{"kept":true},
        "guild_id":"guild-id",
        "channel_id":"channel-id",
        "user_openid":"user-id",
        "application_id":"application-id"
    });
    let event: InteractionEvent = serde_json::from_value(event_value.clone()).unwrap();
    assert!(event.validate().is_ok());
    assert!(event.requires_response());
    assert_eq!(event.extra["future_top"], json!({"kept":true}));
    assert_eq!(event.data.as_ref().unwrap().extra["future_data"], "kept");
    for (interaction_type, expected) in [
        (11, true),
        (12, true),
        (13, false),
        (18, false),
        (999, false),
    ] {
        let mut classified = event.clone();
        classified.interaction_type = interaction_type;
        assert_eq!(classified.requires_response(), expected);
    }
    for field in [
        "guild_id",
        "channel_id",
        "user_openid",
        "group_openid",
        "group_member_openid",
        "application_id",
    ] {
        let mut empty_value = event_value.clone();
        empty_value
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), json!(""));
        let empty: InteractionEvent = serde_json::from_value(empty_value).unwrap();
        assert_eq!(
            empty.validate().unwrap_err(),
            InteractionValidationError::EmptyField { field }
        );

        let mut whitespace_value = event_value.clone();
        whitespace_value
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), json!("bad id"));
        let whitespace: InteractionEvent = serde_json::from_value(whitespace_value).unwrap();
        assert_eq!(
            whitespace.validate().unwrap_err(),
            InteractionValidationError::InvalidField { field }
        );
    }
    let mut mismatched = event.clone();
    mismatched.interaction_type = 13;
    assert_eq!(
        mismatched.validate().unwrap_err(),
        InteractionValidationError::MismatchedInteractionType {
            top_level: 13,
            data: 11,
        }
    );

    let future: InteractionEvent = serde_json::from_value(json!({
        "id":"future-id","type":999,"timestamp":"2026-08-22T10:00:00Z"
    }))
    .unwrap();
    assert!(future.validate().is_ok());
    assert!(!future.requires_response());

    assert_eq!(
        serde_json::to_value(InteractionResponseRequest::default()).unwrap(),
        json!({})
    );
    for code in 0..=5 {
        let request = InteractionResponseRequest { code: Some(code) };
        assert!(request.validate("interaction-id").is_ok());
        assert_eq!(serde_json::to_value(request).unwrap(), json!({"code":code}));
    }
}

#[test]
fn validates_interaction_scenes() {
    for scene_event in [
        json!({
            "id":"c2c-id","type":12,"scene":"c2c",
            "timestamp":"2026-08-22T10:00:00Z","user_openid":"user-id"
        }),
        json!({
            "id":"guild-id","type":13,"scene":"guild",
            "timestamp":"2026-08-22T10:00:00Z","guild_id":"guild-id",
            "channel_id":"channel-id","user_openid":"user-id"
        }),
    ] {
        let event: InteractionEvent = serde_json::from_value(scene_event).unwrap();
        assert!(event.validate().is_ok());
    }
    for scene in ["c2c", "group", "guild"] {
        let event: InteractionEvent = serde_json::from_value(json!({
            "id":"sparse-scene-id","type":13,"scene":scene,
            "timestamp":"2026-08-22T10:00:00Z"
        }))
        .unwrap();
        assert!(event.validate().is_ok());
    }
    for scene in ["", "   "] {
        let event: InteractionEvent = serde_json::from_value(json!({
            "id":"scene-id","type":13,"scene":scene,
            "timestamp":"2026-08-22T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            event.validate().unwrap_err(),
            InteractionValidationError::EmptyField { field: "scene" }
        );
    }
    for scene in ["group ", "c2c\n", "c2c\0"] {
        let event: InteractionEvent = serde_json::from_value(json!({
            "id":"scene-id","type":13,"scene":scene,
            "timestamp":"2026-08-22T10:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            event.validate().unwrap_err(),
            InteractionValidationError::InvalidField { field: "scene" }
        );
    }
    let future_scene: InteractionEvent = serde_json::from_value(json!({
        "id":"future-scene","type":999,"scene":"future_scene",
        "timestamp":"2026-08-22T10:00:00Z"
    }))
    .unwrap();
    assert!(future_scene.validate().is_ok());
    let casing_variant: InteractionEvent = serde_json::from_value(json!({
        "id":"casing-scene","type":13,"scene":"Guild",
        "timestamp":"2026-08-22T10:00:00Z"
    }))
    .unwrap();
    assert!(casing_variant.validate().is_ok());
}

#[tokio::test]
async fn calls_interaction_endpoint_and_accepts_both_unit_responses() {
    let (api, observations, server_task) = client().await;
    respond_with_timeout(
        &api,
        "interaction/id",
        &InteractionResponseRequest { code: Some(0) },
    )
    .await
    .unwrap();
    respond_with_timeout(
        &api,
        "interaction-empty",
        &InteractionResponseRequest::default(),
    )
    .await
    .unwrap();

    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *observations.requests.lock().unwrap(),
        vec![
            ("interaction/id".to_owned(), json!({"code":0})),
            ("interaction-empty".to_owned(), json!({})),
        ]
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn rejects_invalid_interactions_before_authentication() {
    let (api, observations, server_task) = client().await;
    for (interaction_id, request, expected) in [
        (
            " ",
            InteractionResponseRequest::default(),
            InteractionValidationError::EmptyField {
                field: "interaction_id",
            },
        ),
        (
            "INTERACTION_CREATE:interaction-id",
            InteractionResponseRequest::default(),
            InteractionValidationError::PrefixedInteractionId,
        ),
        (
            "bad id",
            InteractionResponseRequest::default(),
            InteractionValidationError::InvalidInteractionId,
        ),
        (
            "interaction-id",
            InteractionResponseRequest { code: Some(6) },
            InteractionValidationError::InvalidResponseCode { code: 6 },
        ),
    ] {
        assert!(matches!(
            respond_with_timeout(&api, interaction_id, &request).await,
            Err(ApiError::InvalidInteractionRequest(actual)) if actual == expected
        ));
    }
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 0);
    assert!(observations.requests.lock().unwrap().is_empty());
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn preserves_platform_and_http_errors() {
    let (api, observations, server_task) = client().await;
    assert!(matches!(
        respond_with_timeout(
            &api,
            "platform-error",
            &InteractionResponseRequest::default()
        )
        .await,
        Err(ApiError::Platform { code: 630_003, .. })
    ));
    assert!(matches!(
        respond_with_timeout(
            &api,
            "rate-limited",
            &InteractionResponseRequest::default()
        ).await,
        Err(ApiError::HttpStatus { status: StatusCode::TOO_MANY_REQUESTS, code: Some(630_008), retry_after: Some(delay), .. })
            if delay.as_secs() == 3
    ));
    assert!(matches!(
        respond_with_timeout(&api, "server-error", &InteractionResponseRequest::default()).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: Some(630_004),
            ..
        })
    ));
    assert!(matches!(
        respond_with_timeout(&api, "unauthorized", &InteractionResponseRequest::default()).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            code: Some(630_006),
            ..
        })
    ));
    assert!(matches!(
        respond_with_timeout(&api, "redirect", &InteractionResponseRequest::default()).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::TEMPORARY_REDIRECT,
            ..
        })
    ));
    assert!(matches!(
        respond_with_timeout(
            &api,
            "permanent-redirect",
            &InteractionResponseRequest::default()
        )
        .await,
        Err(ApiError::HttpStatus {
            status: StatusCode::PERMANENT_REDIRECT,
            ..
        })
    ));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 2);
    {
        let requests = observations.requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        for interaction_id in [
            "platform-error",
            "rate-limited",
            "server-error",
            "unauthorized",
            "redirect",
            "permanent-redirect",
        ] {
            assert_eq!(
                requests
                    .iter()
                    .filter(|(actual, _)| actual == interaction_id)
                    .count(),
                1,
                "interaction response must not be retried for {interaction_id}"
            );
        }
        assert!(requests.iter().all(|(interaction_id, _)| !matches!(
            interaction_id.as_str(),
            "redirect-target" | "permanent-redirect-target"
        )));
    }
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}

#[tokio::test]
async fn preserves_unauthorized_error_when_token_refresh_fails() {
    let (api, observations, server_task) = client().await;
    observations
        .fail_token_after_first
        .store(true, Ordering::SeqCst);
    assert!(matches!(
        respond_with_timeout(&api, "unauthorized", &InteractionResponseRequest::default()).await,
        Err(ApiError::HttpStatus {
            status: StatusCode::UNAUTHORIZED,
            code: Some(630_006),
            message: Some(message),
            trace_id: None,
            retry_after: None,
        }) if message == "header appid failed"
    ));
    assert_eq!(observations.token_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        observations
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(interaction_id, _)| interaction_id == "unauthorized")
            .count(),
        1
    );
    server_task.abort();
    assert!(server_task.await.is_err_and(|error| error.is_cancelled()));
}
