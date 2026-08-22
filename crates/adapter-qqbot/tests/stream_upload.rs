use std::{sync::Arc, time::Duration};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use bot_core::{Action, Adapter, AdapterError, MediaAttachment, MessageTarget, SendMediaAction};
use futures_util::stream;
use qqbot_protocol::{
    ApiError, OpenApiClient, StreamUploadValidationError, TokenManager, UploadPart,
    UploadPrepareResponse,
};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use url::Url;

#[derive(Debug, Clone, PartialEq)]
struct ObservedRequest {
    method: Method,
    path: String,
    authorization: Option<String>,
    content_type: Option<String>,
    body: Value,
}

struct TestState {
    requests: Mutex<Vec<ObservedRequest>>,
    token_count: Mutex<u32>,
}

struct TestServerTask(Option<JoinHandle<()>>);

impl TestServerTask {
    async fn abort_and_wait(mut self) {
        let task = self.0.take().expect("test server task should be present");
        task.abort();
        match task.await {
            Err(error) if error.is_cancelled() => {}
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Ok(()) => panic!("test server exited unexpectedly"),
            Err(error) => panic!("test server task failed: {error}"),
        }
    }
}

impl Drop for TestServerTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn endpoint(
    State(state): State<Arc<TestState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let json_body = serde_json::from_slice(&body)
        .unwrap_or_else(|error| panic!("request to {path} contained invalid JSON: {error}"));
    let request_number = {
        let mut requests = state.requests.lock().await;
        requests.push(ObservedRequest {
            method: method.clone(),
            path: path.clone(),
            authorization,
            content_type,
            body: json_body,
        });
        requests
            .iter()
            .filter(|request| request.path == path)
            .count()
    };
    mock_response(&state, &path, request_number).await
}

async fn mock_response(state: &Arc<TestState>, path: &str, request_number: usize) -> Response {
    if path == "/app/getAppAccessToken" {
        let mut token_count = state.token_count.lock().await;
        *token_count += 1;
        return Json(json!({
            "access_token":format!("token-{token_count}"),"expires_in":7200
        }))
        .into_response();
    }
    if path.contains("/redirect/") {
        let location = path.replace("/redirect/", "/redirect-target/");
        return (StatusCode::TEMPORARY_REDIRECT, [("location", location)]).into_response();
    }
    let always_unauthorized = matches!(
        path,
        "/v2/users/unauthorized/stream_messages"
            | "/v2/groups/unauthorized/upload_part_finish"
            | "/v2/groups/unauthorized-finalize/files"
            | "/v2/groups/url-send-unauthorized/files"
            | "/v2/users/unauthorized-prepare/upload_prepare"
    );
    let first_attempt_unauthorized = matches!(
        path,
        "/v2/users/legacy-retry/files" | "/v2/users/inline-retry/files"
    ) && request_number == 1;
    if always_unauthorized || first_attempt_unauthorized {
        return unauthorized_response();
    }
    if path == "/v2/users/server-error/stream_messages" {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if path == "/v2/users/truncated/stream_messages" {
        return Response::builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(stream::once(async {
                Err::<Bytes, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "test response closed after request was accepted",
                ))
            })))
            .unwrap();
    }
    if path.ends_with("/stream_messages") {
        return Json(json!({
            "id":"stream-id","timestamp":"2026-08-22T10:00:00+08:00",
            "ext_info":{"ref_idx":"1"},"remain_msg_len":8
        }))
        .into_response();
    }
    if path.ends_with("/upload_prepare") {
        return Json(json!({
            "upload_id":"upload-id","block_size":"5",
            "parts":[{
                "index":0,"presigned_url":"https://upload.example/part-0",
                "block_size":"5"
            }],
            "upload_config":{"concurrency":1,"retry_timeout":300,"retry_delay":1}
        }))
        .into_response();
    }
    if path.ends_with("/upload_part_finish") {
        if path.starts_with("/v2/groups/") {
            return StatusCode::NO_CONTENT.into_response();
        }
        return Json(json!({})).into_response();
    }
    if path.ends_with("/files") {
        if matches!(
            path,
            "/v2/groups/merge-only/files" | "/v2/groups/url-send-missing-id/files"
        ) {
            return Json(json!({
                "file_uuid":"file-uuid","file_info":"opaque-file-info","ttl":3600,
                "raw_url":"https://download.example/file"
            }))
            .into_response();
        }
        return Json(json!({
            "file_uuid":"file-uuid","file_info":"opaque-file-info","ttl":3600,
            "id":"sent-message-id","raw_url":"https://download.example/file"
        }))
        .into_response();
    }
    if path == "/v2/users/inline-retry/messages" {
        return Json(json!({"id":"inline-message"})).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"code":610_014,"message":"unauthorized"})),
    )
        .into_response()
}

async fn harness() -> (
    QqWebSocketAdapter,
    OpenApiClient,
    Arc<TestState>,
    TestServerTask,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let state = Arc::new(TestState {
        requests: Mutex::new(Vec::new()),
        token_count: Mutex::new(0),
    });
    let app = Router::new()
        .fallback(any(endpoint))
        .with_state(Arc::clone(&state));
    let server_task = TestServerTask(Some(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    })));
    let base_url = Url::parse(&format!("{origin}/")).unwrap();
    let tokens = TokenManager::with_client_and_endpoint(
        Client::new(),
        base_url.join("app/getAppAccessToken").unwrap(),
        "app-id",
        SecretString::from("secret".to_owned().into_boxed_str()),
    );
    let api = OpenApiClient::with_base_url(base_url, tokens).unwrap();
    (
        QqWebSocketAdapter::new(QqWebSocketConfig::default(), api.clone()),
        api,
        state,
        server_task,
    )
}

async fn platform(
    adapter: &QqWebSocketAdapter,
    name: &str,
    payload: Value,
) -> Result<bot_core::ActionResult, AdapterError> {
    adapter
        .execute(Action::Platform {
            name: name.to_owned(),
            payload,
        })
        .await
}

fn action_message(error: AdapterError, context: &str) -> String {
    let AdapterError::Action(message) = error else {
        panic!("{context} must be a deterministic Action error: {error}");
    };
    message
}

fn unknown_action_message(error: AdapterError, context: &str) -> String {
    let AdapterError::ActionUnknown(message) = error else {
        panic!("{context} must be ActionUnknown: {error}");
    };
    message
}

fn assert_action_message(error: AdapterError, context: &str, expected: &str) {
    assert_eq!(action_message(error, context), expected);
}

fn prepare_payload(target_field: &str, target: &str) -> Value {
    let mut payload = json!({
        "file_type":2,"file_size":"5","file_name":"video.mp4",
        "md5":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "md5_10m":"cccccccccccccccccccccccccccccccc"
    });
    payload[target_field] = Value::from(target);
    payload
}

fn finish_payload(target_field: &str, target: &str) -> Value {
    let mut payload = json!({
        "upload_id":"upload-id","part_index":0,"block_size":"5",
        "md5":"dddddddddddddddddddddddddddddddd"
    });
    payload[target_field] = Value::from(target);
    payload
}

fn assert_authenticated_request_contracts(requests: &[ObservedRequest]) {
    let prepare_body = json!({
        "file_type":2,"file_size":"5","file_name":"video.mp4",
        "md5":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "sha1":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "md5_10m":"cccccccccccccccccccccccccccccccc"
    });
    let finish_body = json!({
        "upload_id":"upload-id","part_index":0,"block_size":"5",
        "md5":"dddddddddddddddddddddddddddddddd"
    });
    let finalize_body = json!({
        "file_type":2,"upload_id":"upload-id","file_name":"video.mp4","srv_send_msg":true
    });
    for (path, body) in [
        (
            "/v2/users/user%2Fid/stream_messages",
            json!({
                "input_mode":"append","input_state":1,"index":0,
                "content_type":"markdown","content_raw":"第一片","msg_id":"message-id",
                "msg_seq":1,"is_wakeup":false
            }),
        ),
        ("/v2/users/user%2Fid/upload_prepare", prepare_body.clone()),
        ("/v2/groups/group%2Fid/upload_prepare", prepare_body),
        (
            "/v2/users/user%2Fid/upload_part_finish",
            finish_body.clone(),
        ),
        ("/v2/groups/group%2Fid/upload_part_finish", finish_body),
        ("/v2/users/user%2Fid/files", finalize_body.clone()),
        ("/v2/groups/group%2Fid/files", finalize_body),
    ] {
        let matching = requests
            .iter()
            .filter(|request| request.path == path)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{path} must be sent exactly once");
        let request = matching[0];
        assert_eq!(request.method, Method::POST, "wrong method for {path}");
        assert_eq!(
            request.authorization.as_deref(),
            Some("QQBot token-1"),
            "wrong QQBot authorization for {path}"
        );
        assert_eq!(request.content_type.as_deref(), Some("application/json"));
        assert_eq!(request.body, body, "wrong JSON body for {path}");
    }
}

fn assert_non_idempotent_request_counts(requests: &[ObservedRequest]) {
    assert!(
        requests
            .iter()
            .all(|request| !request.path.contains("redirect-target"))
    );
    for suffix in [
        "/v2/users/redirect/stream_messages",
        "/v2/groups/redirect/upload_prepare",
        "/v2/groups/redirect/upload_part_finish",
        "/v2/groups/redirect/files",
        "/v2/users/unauthorized/stream_messages",
        "/v2/users/server-error/stream_messages",
        "/v2/users/truncated/stream_messages",
    ] {
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.path == suffix)
                .count(),
            1,
            "{suffix} must be sent exactly once"
        );
    }
}

fn assert_request_authorization(requests: &[ObservedRequest], path: &str, expected: &str) {
    let request = requests
        .iter()
        .find(|request| request.path == path)
        .unwrap_or_else(|| panic!("missing request {path}"));
    assert_eq!(request.authorization.as_deref(), Some(expected));
}

#[tokio::test]
async fn exposes_stream_prepare_part_finish_and_finalize() {
    let (adapter, _api, state, server_task) = harness().await;
    let stream = platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"user/id","input_mode":"append","input_state":1,"index":0,
            "content_type":"markdown","content_raw":"第一片","msg_id":"message-id",
            "msg_seq":1,"is_wakeup":false
        }),
    )
    .await
    .unwrap();
    assert_eq!(stream.message_id.as_deref(), Some("stream-id"));
    assert_eq!(stream.raw["remain_msg_len"], 8);

    let c2c_prepare = platform(
        &adapter,
        "qq.c2c.upload.prepare",
        prepare_payload("user_openid", "user/id"),
    )
    .await
    .unwrap();
    let prepared: UploadPrepareResponse = serde_json::from_value(c2c_prepare.raw).unwrap();
    assert_eq!(
        prepared.parts[0].presigned_url,
        "https://upload.example/part-0"
    );

    platform(
        &adapter,
        "qq.group.upload.prepare",
        prepare_payload("group_openid", "group/id"),
    )
    .await
    .unwrap();
    platform(
        &adapter,
        "qq.c2c.upload.part-finish",
        finish_payload("user_openid", "user/id"),
    )
    .await
    .unwrap();
    platform(
        &adapter,
        "qq.group.upload.part-finish",
        finish_payload("group_openid", "group/id"),
    )
    .await
    .unwrap();

    for target in [
        json!({"scope":"private","user_id":"user/id"}),
        json!({"scope":"group","group_id":"group/id"}),
    ] {
        let result = platform(
            &adapter,
            "qq.media.upload",
            json!({
                "target":target,"file_type":2,"upload_id":"upload-id",
                "file_name":"video.mp4","send":true
            }),
        )
        .await
        .unwrap();
        assert_eq!(result.message_id.as_deref(), Some("sent-message-id"));
        assert_eq!(result.raw["file_info"], "opaque-file-info");
    }

    let requests = state.requests.lock().await;
    assert_authenticated_request_contracts(&requests);
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 1);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn preserves_legacy_url_media_upload_action() {
    let (adapter, _api, state, server_task) = harness().await;
    let result = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"private","user_id":"legacy/user"},
            "file_type":2,"url":"https://cdn.example/video.mp4","file_name":"legacy-metadata",
            "send":false,"fragment":"legacy-metadata",
            "producer_meta":{"future":true}
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.message_id, None);

    let requests = state.requests.lock().await;
    let request = requests
        .iter()
        .find(|request| request.path == "/v2/users/legacy%2Fuser/files")
        .unwrap();
    assert_eq!(request.method, Method::POST);
    assert_eq!(request.authorization.as_deref(), Some("QQBot token-1"));
    assert_eq!(request.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        request.body,
        json!({
            "file_type":2,"url":"https://cdn.example/video.mp4","srv_send_msg":false
        })
    );
    assert!(request.body.get("upload_id").is_none());
    drop(requests);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn url_upload_returns_message_id_when_direct_send_is_requested() {
    let (adapter, _api, state, server_task) = harness().await;
    let result = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"group","group_id":"url-send"},
            "file_type":2,"url":"https://cdn.example/video.mp4","send":true
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.message_id.as_deref(), Some("sent-message-id"));
    let requests = state.requests.lock().await;
    let request = requests
        .iter()
        .find(|request| request.path == "/v2/groups/url-send/files")
        .unwrap();
    assert_eq!(request.body["srv_send_msg"], true);
    drop(requests);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn direct_send_without_response_message_id_remains_successful() {
    let (adapter, _api, state, server_task) = harness().await;
    let result = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"group","group_id":"url-send-missing-id"},
            "file_type":2,"url":"https://cdn.example/video.mp4","send":true
        }),
    )
    .await
    .unwrap();

    assert_eq!(result.message_id, None);
    assert_eq!(result.raw["file_uuid"], "file-uuid");
    assert_eq!(
        state
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request.path == "/v2/groups/url-send-missing-id/files")
            .count(),
        1
    );
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn url_direct_send_401_refreshes_token_without_replaying_the_message() {
    let (adapter, _api, state, server_task) = harness().await;
    let error = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"group","group_id":"url-send-unauthorized"},
            "file_type":2,"url":"https://cdn.example/video.mp4","send":true
        }),
    )
    .await
    .unwrap_err();
    assert_action_message(
        error,
        "URL direct-send 401",
        "QQ OpenAPI returned HTTP 401 Unauthorized; code=Some(610014), message=Some(\"unauthorized\")",
    );
    platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"after-url-send-refresh","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap();

    let requests = state.requests.lock().await;
    let direct_send = requests
        .iter()
        .filter(|request| request.path == "/v2/groups/url-send-unauthorized/files")
        .collect::<Vec<_>>();
    assert_eq!(direct_send.len(), 1);
    assert_eq!(
        direct_send[0].authorization.as_deref(),
        Some("QQBot token-1")
    );
    assert_request_authorization(
        &requests,
        "/v2/users/after-url-send-refresh/stream_messages",
        "QQBot token-2",
    );
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 2);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn merge_only_finalize_accepts_response_without_message_id() {
    let (adapter, _api, state, server_task) = harness().await;
    let result = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"group","group_id":"merge-only"},
            "file_type":2,"upload_id":"upload-id","file_name":"video.mp4","send":false
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.message_id, None);
    assert_eq!(result.raw["file_uuid"], "file-uuid");
    assert_eq!(
        state
            .requests
            .lock()
            .await
            .iter()
            .filter(|request| request.path == "/v2/groups/merge-only/files")
            .count(),
        1
    );
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn legacy_url_upload_refreshes_once_and_replays_with_the_new_token() {
    let (adapter, _api, state, server_task) = harness().await;
    let result = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"private","user_id":"legacy-retry"},
            "file_type":2,"url":"https://cdn.example/video.mp4","send":false
        }),
    )
    .await
    .unwrap();
    assert_eq!(result.message_id, None);

    let requests = state.requests.lock().await;
    let attempts = requests
        .iter()
        .filter(|request| request.path == "/v2/users/legacy-retry/files")
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].authorization.as_deref(), Some("QQBot token-1"));
    assert_eq!(attempts[1].authorization.as_deref(), Some("QQBot token-2"));
    assert_eq!(attempts[0].body, attempts[1].body);
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 2);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn legacy_inline_upload_refreshes_once_before_sending_the_media_message() {
    let (adapter, _api, state, server_task) = harness().await;
    let attachment =
        MediaAttachment::image("image/png", None, b"\x89PNG\r\n\x1a\n".to_vec()).unwrap();
    let result = adapter
        .execute(Action::SendMedia(SendMediaAction {
            target: MessageTarget::Private {
                user_id: "inline-retry".to_owned(),
            },
            attachment,
            caption: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.message_id.as_deref(), Some("inline-message"));

    let requests = state.requests.lock().await;
    let attempts = requests
        .iter()
        .filter(|request| request.path == "/v2/users/inline-retry/files")
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].authorization.as_deref(), Some("QQBot token-1"));
    assert_eq!(attempts[1].authorization.as_deref(), Some("QQBot token-2"));
    assert_eq!(attempts[0].body, attempts[1].body);
    assert_eq!(attempts[0].body["file_type"], 1);
    assert_eq!(attempts[0].body["file_data"], "iVBORw0KGgo=");
    let send = requests
        .iter()
        .find(|request| request.path == "/v2/users/inline-retry/messages")
        .unwrap();
    assert_eq!(send.authorization.as_deref(), Some("QQBot token-2"));
    assert_eq!(send.body["media"]["file_info"], "opaque-file-info");
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 2);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn part_finish_401_refreshes_token_without_replaying_the_request() {
    let (adapter, _api, state, server_task) = harness().await;
    let error = platform(
        &adapter,
        "qq.group.upload.part-finish",
        finish_payload("group_openid", "unauthorized"),
    )
    .await
    .unwrap_err();
    assert_action_message(
        error,
        "part-finish 401",
        "QQ OpenAPI returned HTTP 401 Unauthorized; code=Some(610014), message=Some(\"unauthorized\")",
    );
    platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"after-refresh","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap();

    let requests = state.requests.lock().await;
    let finish = requests
        .iter()
        .filter(|request| request.path == "/v2/groups/unauthorized/upload_part_finish")
        .collect::<Vec<_>>();
    assert_eq!(finish.len(), 1);
    assert_eq!(finish[0].authorization.as_deref(), Some("QQBot token-1"));
    let after = requests
        .iter()
        .find(|request| request.path == "/v2/users/after-refresh/stream_messages")
        .unwrap();
    assert_eq!(after.authorization.as_deref(), Some("QQBot token-2"));
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 2);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn finalize_401_refreshes_token_without_replaying_the_message_sending_request() {
    let (adapter, _api, state, server_task) = harness().await;
    let error = platform(
        &adapter,
        "qq.media.upload",
        json!({
            "target":{"scope":"group","group_id":"unauthorized-finalize"},
            "file_type":2,"upload_id":"upload-id","file_name":"video.mp4","send":true
        }),
    )
    .await
    .unwrap_err();
    assert_action_message(
        error,
        "finalize 401",
        "QQ OpenAPI returned HTTP 401 Unauthorized; code=Some(610014), message=Some(\"unauthorized\")",
    );
    platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"after-finalize-refresh","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap();

    let requests = state.requests.lock().await;
    let finalize = requests
        .iter()
        .filter(|request| request.path == "/v2/groups/unauthorized-finalize/files")
        .collect::<Vec<_>>();
    assert_eq!(finalize.len(), 1);
    assert_eq!(finalize[0].authorization.as_deref(), Some("QQBot token-1"));
    let after = requests
        .iter()
        .find(|request| request.path == "/v2/users/after-finalize-refresh/stream_messages")
        .unwrap();
    assert_eq!(after.authorization.as_deref(), Some("QQBot token-2"));
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 2);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn prepare_401_refreshes_token_without_replaying_upload_state_creation() {
    let (adapter, _api, state, server_task) = harness().await;
    let error = platform(
        &adapter,
        "qq.c2c.upload.prepare",
        prepare_payload("user_openid", "unauthorized-prepare"),
    )
    .await
    .unwrap_err();
    assert_action_message(
        error,
        "prepare 401",
        "QQ OpenAPI returned HTTP 401 Unauthorized; code=Some(610014), message=Some(\"unauthorized\")",
    );
    platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"after-prepare-refresh","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap();

    let requests = state.requests.lock().await;
    let prepare = requests
        .iter()
        .filter(|request| request.path == "/v2/users/unauthorized-prepare/upload_prepare")
        .collect::<Vec<_>>();
    assert_eq!(prepare.len(), 1);
    assert_eq!(prepare[0].authorization.as_deref(), Some("QQBot token-1"));
    let after = requests
        .iter()
        .find(|request| request.path == "/v2/users/after-prepare-refresh/stream_messages")
        .unwrap();
    assert_eq!(after.authorization.as_deref(), Some("QQBot token-2"));
    drop(requests);
    assert_eq!(*state.token_count.lock().await, 2);
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn rejects_invalid_payloads_and_presigned_part_lengths_before_io() {
    let (adapter, api, state, server_task) = harness().await;
    for (name, payload, expected) in [
        (
            "qq.c2c.stream-message.send",
            json!({
                "user_openid":"user","input_mode":"append","input_state":1,"index":1,
                "content_type":"text","content_raw":"content","msg_id":"message-id"
            }),
            "stream_msg_id",
        ),
        (
            "qq.c2c.upload.prepare",
            json!({
                "user_openid":"user","file_type":2,"file_size":5,"file_name":"video.mp4",
                "md5":"a","sha1":"b","md5_10m":"c"
            }),
            "string",
        ),
        (
            "qq.group.upload.part-finish",
            json!({
                "group_openid":"group","upload_id":"upload-id","part_index":0,
                "block_size":"5","md5":"bad","future":true
            }),
            "unknown field",
        ),
        (
            "qq.media.upload",
            json!({
                "target":{"scope":"group","group_id":"group"},"file_type":2,
                "url":"https://example.com/video.mp4","upload_id":"upload-id"
            }),
            "both url and upload_id",
        ),
        (
            "qq.media.upload",
            json!({
                "target":{"scope":"group","group_id":"group"},"file_type":2,
                "url":"https://example.com/video.mp4#secret"
            }),
            "fragment",
        ),
    ] {
        let error = platform(&adapter, name, payload).await.unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic Action error for {name}: {error}");
        };
        assert!(message.contains(expected), "unexpected error: {message}");
    }
    assert!(state.requests.lock().await.is_empty());

    let part = UploadPart {
        index: 0,
        presigned_url: "https://upload.example/part-0".to_owned(),
        block_size: qqbot_protocol::DecimalBytes::new("block_size", "5").unwrap(),
        extra: serde_json::Map::new(),
    };
    let error = api
        .upload_prepared_part(&part, b"1234".to_vec(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::InvalidStreamUploadRequest(StreamUploadValidationError::PartSizeMismatch {
            expected: 5,
            actual: 4
        })
    ));
    let error = api
        .upload_prepared_part(&part, b"12345".to_vec(), Duration::ZERO)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::InvalidStreamUploadRequest(StreamUploadValidationError::ZeroUploadTimeout)
    ));
    let error = api
        .upload_prepared_part(&part, b"12345".to_vec(), Duration::MAX)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::InvalidStreamUploadRequest(StreamUploadValidationError::InvalidUploadTimeout)
    ));

    let private_part = UploadPart {
        presigned_url: "https://127.0.0.1/upload".to_owned(),
        ..part
    };
    let error = api
        .upload_prepared_part(&private_part, b"12345".to_vec(), Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::InvalidStreamUploadRequest(
            StreamUploadValidationError::InvalidPresignedDestination
        )
    ));
    assert!(state.requests.lock().await.is_empty());
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn rejects_empty_upload_ids_before_io() {
    let (adapter, _api, state, server_task) = harness().await;
    for upload_id in ["", "   "] {
        let error = platform(
            &adapter,
            "qq.media.upload",
            json!({
                "target":{"scope":"group","group_id":"group"},"file_type":2,
                "upload_id":upload_id
            }),
        )
        .await
        .unwrap_err();
        let AdapterError::Action(message) = error else {
            panic!("expected deterministic Action error: {error}");
        };
        assert!(message.contains("must not be empty"));
    }
    assert!(state.requests.lock().await.is_empty());
    server_task.abort_and_wait().await;
}

#[tokio::test]
async fn non_idempotent_operations_are_not_replayed_or_redirected() {
    let (adapter, _api, state, server_task) = harness().await;
    let cases = [
        (
            "qq.c2c.stream-message.send",
            json!({
                "user_openid":"redirect","input_mode":"append","input_state":10,"index":0,
                "content_type":"text","content_raw":"done","msg_id":"message-id"
            }),
        ),
        (
            "qq.group.upload.prepare",
            prepare_payload("group_openid", "redirect"),
        ),
        (
            "qq.group.upload.part-finish",
            finish_payload("group_openid", "redirect"),
        ),
        (
            "qq.media.upload",
            json!({
                "target":{"scope":"group","group_id":"redirect"},"file_type":2,
                "upload_id":"upload-id","file_name":"video.mp4","send":true
            }),
        ),
    ];
    for (name, payload) in cases {
        let error = platform(&adapter, name, payload).await.unwrap_err();
        assert_action_message(
            error,
            "redirect",
            "QQ OpenAPI returned HTTP 307 Temporary Redirect; code=None, message=None",
        );
    }
    let unauthorized = platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"unauthorized","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap_err();
    assert_action_message(
        unauthorized,
        "401",
        "QQ OpenAPI returned HTTP 401 Unauthorized; code=Some(610014), message=Some(\"unauthorized\")",
    );
    platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"after-stream-refresh","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap();
    let server_error = platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"server-error","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap_err();
    assert_action_message(
        server_error,
        "500",
        "QQ OpenAPI returned HTTP 500 Internal Server Error; code=None, message=None",
    );
    let truncated = platform(
        &adapter,
        "qq.c2c.stream-message.send",
        json!({
            "user_openid":"truncated","input_mode":"append","input_state":10,
            "index":0,"content_type":"text","content_raw":"done","msg_id":"message-id"
        }),
    )
    .await
    .unwrap_err();
    let message = unknown_action_message(truncated, "truncated response");
    assert_eq!(
        message,
        "QQ OpenAPI request failed: request transport failed"
    );

    let requests = state.requests.lock().await;
    assert_request_authorization(
        &requests,
        "/v2/users/after-stream-refresh/stream_messages",
        "QQBot token-2",
    );
    assert_eq!(*state.token_count.lock().await, 2);
    assert_non_idempotent_request_counts(&requests);
    drop(requests);
    server_task.abort_and_wait().await;
}
