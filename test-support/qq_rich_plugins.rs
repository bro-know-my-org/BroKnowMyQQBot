//! Static and WASM BPP APIs through the same Host and real QQ `OpenAPI` executor.

use std::{
    collections::BTreeMap,
    io::{Cursor, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use adapter_qqbot::{QqWebSocketAdapter, QqWebSocketConfig};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{post, put},
};
use base64::Engine as _;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, ButtonInteraction, CommonMessage,
    Event, EventEnvelope, EventSender, MessageTarget, RuntimeBuilder, Sender, ShutdownHandle,
    ShutdownSignal, shutdown_channel,
};
use builtin_plugins::QqMarkdownPagesPlugin;
use plugin_api::{
    ActionCompleted, ActionStatus, HandlerOutput, HostQueries, InitContext, PluginCommand,
    PluginError, PluginEventEnvelope, PluginId, PluginManifest, QQ_BUTTON_ACK, QQ_BUTTON_CLICKED,
    QQ_MARKDOWN_REPLY, QQ_MARKDOWN_SEND, QqButtonAck, RuntimeMode, StaticPlugin,
};
use plugin_host::{PluginStore, StaticPluginHost, ValidatedPluginPackage, WasmPlugin};
use qqbot_protocol::{OpenApiClient, TokenManager};
use reqwest::Client;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use url::Url;

#[path = "qq_rich_plugins/concurrency.rs"]
mod concurrency;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct MockQq {
    requests: Mutex<Vec<(String, Value)>>,
    pause_next_send: Mutex<Option<oneshot::Receiver<StatusCode>>>,
    pause_next_ack: Mutex<Option<oneshot::Receiver<()>>>,
    accepted: Notify,
    ack_started: Notify,
}

async fn token() -> Json<Value> {
    Json(json!({"access_token":"mock-token","expires_in":7200}))
}

async fn send(
    State(api): State<Arc<MockQq>>,
    Path((scope, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let denied = scope == "groups" && body.get("msg_id").is_none();
    api.requests
        .lock()
        .unwrap()
        .push((format!("{scope}/{id}"), body));
    let paused = api.pause_next_send.lock().unwrap().take();
    if let Some(paused) = paused {
        api.accepted.notify_one();
        let status = paused.await.unwrap();
        if !status.is_success() {
            return (status, Json(json!({"code":400_998})));
        }
    }
    if denied {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code":40_034_105,"message":"PRIVATE_CODE_DETAIL"})),
        );
    }
    (StatusCode::OK, Json(json!({"id":"new-page-id"})))
}

async fn ack(
    State(api): State<Arc<MockQq>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    api.requests
        .lock()
        .unwrap()
        .push((format!("interactions/{id}"), body));
    let paused = api.pause_next_ack.lock().unwrap().take();
    if let Some(paused) = paused {
        api.ack_started.notify_one();
        paused.await.unwrap();
    }
    match id.as_str() {
        "bad-ack" => (
            StatusCode::BAD_REQUEST,
            Json(json!({"code":400_999,"message":"PRIVATE_ACK_DETAIL"})),
        ),
        "unknown-ack" => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"code":500_999})),
        ),
        _ => (StatusCode::OK, Json(json!({}))),
    }
}

struct InputAdapter {
    qq: QqWebSocketAdapter,
    input: tokio::sync::Mutex<mpsc::Receiver<EventEnvelope>>,
    handled: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl Adapter for InputAdapter {
    fn id(&self) -> &AdapterId {
        self.qq.id()
    }

    fn platform(&self) -> &'static str {
        self.qq.platform()
    }

    async fn run(
        &self,
        events: EventSender,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let mut input = self.input.lock().await;
        events.mark_ready();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                e = input.recv() => {
                    let Some(event) = e else { return Ok(()) };
                    events.send(event).await.map_err(|_| AdapterError::EventQueueClosed)?;
                }
            }
        }
    }

    async fn event_handled(&self, event: &EventEnvelope) -> Result<(), AdapterError> {
        self.handled
            .send(event.id.to_string())
            .map_err(|_| AdapterError::EventQueueClosed)
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        self.qq.execute(action).await
    }
}

struct ObservedPlugin {
    inner: Arc<dyn StaticPlugin>,
    manifest: PluginManifest,
    receipts: Mutex<Vec<ActionCompleted>>,
    redirect_followup: AtomicBool,
    active_first_page: AtomicBool,
    use_ordinary_send: AtomicBool,
    slow_click: AtomicBool,
    slow_command: AtomicBool,
    button_invocations: AtomicUsize,
}

#[async_trait]
impl StaticPlugin for ObservedPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn init(&self, context: InitContext) -> Result<(), PluginError> {
        self.inner.init(context).await
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "action.completed" {
            self.receipts
                .lock()
                .unwrap()
                .push(serde_json::from_value(event.payload.clone()).unwrap());
        }
        if event.event_type == QQ_BUTTON_CLICKED {
            self.button_invocations.fetch_add(1, Ordering::Relaxed);
            if self.slow_click.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
        if event.event_type == QQ_BUTTON_CLICKED && self.use_ordinary_send.load(Ordering::Relaxed) {
            return Ok(HandlerOutput {
                commands: vec![PluginCommand {
                    command_id: "plain-send".to_owned(),
                    kind: "message.send".to_owned(),
                    idempotency_key: None,
                    deadline_ms: None,
                    payload: json!({"target":{"scope":"private","user_id":"other-reader"},"content":"outside the callback"}),
                }],
                ..HandlerOutput::default()
            });
        }
        let mut output = self.inner.on_event(event, queries).await?;
        if event.event_type == QQ_BUTTON_CLICKED && self.slow_command.load(Ordering::Relaxed) {
            output
                .commands
                .splice(0..0, concurrency::forbidden_click_commands());
        }
        if self.redirect_followup.load(Ordering::Relaxed) {
            for command in &mut output.commands {
                if command.kind == QQ_MARKDOWN_SEND {
                    command.payload["target"] =
                        json!({"scope":"private", "user_id":"other-reader"});
                }
            }
        }
        if self.active_first_page.load(Ordering::Relaxed) {
            for command in &mut output.commands {
                if command.kind == QQ_MARKDOWN_REPLY {
                    QQ_MARKDOWN_SEND.clone_into(&mut command.kind);
                    command.payload = json!({
                        "target":{"scope":"private","user_id":"reader"},
                        "markdown": command.payload.take(),
                    });
                }
            }
        }
        Ok(output)
    }
}

struct OtherPlugin {
    manifest: PluginManifest,
    button_deliveries: AtomicUsize,
}

#[async_trait]
impl StaticPlugin for OtherPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == QQ_BUTTON_CLICKED {
            self.button_deliveries.fetch_add(1, Ordering::Relaxed);
        }
        Ok(HandlerOutput {
            commands: vec![PluginCommand::qq_button_ack(
                "other-ack",
                QqButtonAck::Success,
            )],
            ..HandlerOutput::default()
        })
    }
}

struct Harness {
    input: mpsc::Sender<EventEnvelope>,
    handled: mpsc::UnboundedReceiver<String>,
    api: Arc<MockQq>,
    adapter_id: AdapterId,
    observed: Arc<ObservedPlugin>,
    slow_http: Arc<concurrency::SlowHttp>,
    other: Arc<OtherPlugin>,
    shutdown: ShutdownHandle,
    runtime: JoinHandle<()>,
    server: JoinHandle<()>,
}

impl Harness {
    async fn new(inner: Arc<dyn StaticPlugin>, deny_ack: bool) -> Self {
        let mock: Arc<MockQq> = Arc::default();
        let router = Router::new()
            .route("/app/getAppAccessToken", post(token))
            .route("/v2/{scope}/{id}/messages", post(send))
            .route("/interactions/{id}", put(ack))
            .with_state(mock.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let open_api = OpenApiClient::with_base_url(
            base.clone(),
            TokenManager::with_client_and_endpoint(
                Client::new(),
                base.join("app/getAppAccessToken").unwrap(),
                "test-id",
                SecretString::from("test-secret"),
            ),
        )
        .unwrap();
        let (input, rx) = mpsc::channel(4);
        let (handled, handled_rx) = mpsc::unbounded_channel();
        let adapter = Arc::new(InputAdapter {
            qq: QqWebSocketAdapter::new(QqWebSocketConfig::default(), open_api),
            input: tokio::sync::Mutex::new(rx),
            handled,
        });
        let manifest = concurrency::test_manifest(inner.manifest().clone());
        let observed = Arc::new(ObservedPlugin {
            inner,
            manifest,
            receipts: Mutex::default(),
            active_first_page: AtomicBool::new(false),
            use_ordinary_send: AtomicBool::new(false),
            slow_click: AtomicBool::new(false),
            slow_command: AtomicBool::new(false),
            button_invocations: AtomicUsize::new(0),
            redirect_followup: AtomicBool::new(false),
        });
        let mut other_manifest = observed.manifest().clone();
        other_manifest.id = PluginId::new("dev.bkm.other").unwrap();
        other_manifest.commands.clear();
        other_manifest
            .subscriptions
            .retain(|sub| sub.event != "action.completed");
        let other = Arc::new(OtherPlugin {
            manifest: other_manifest,
            button_deliveries: AtomicUsize::new(0),
        });
        let slow_http = Arc::new(concurrency::SlowHttp::default());
        let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap())
            .with_adapter(adapter.clone())
            .with_http_executor(slow_http.clone());
        let mut grants = observed.manifest().requested_capabilities();
        if deny_ack {
            grants.remove(QQ_BUTTON_ACK);
        }
        host.register(observed.clone(), "pager/test", BTreeMap::new(), grants)
            .await
            .unwrap();
        host.register_trusted(other.clone(), "other/test", BTreeMap::new())
            .await
            .unwrap();
        let adapter_id = adapter.id().clone();
        let (shutdown, signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(host))
            .build()
            .unwrap();
        let runtime = tokio::spawn(async move { runtime.run(signal).await.unwrap() });
        Self {
            input,
            handled: handled_rx,
            api: mock,
            adapter_id,
            observed,
            slow_http,
            other,
            shutdown,
            runtime,
            server,
        }
    }

    async fn submit(&self, id: &str, event: Event, age_secs: i64) {
        self.input
            .send(EventEnvelope {
                id: id.into(),
                adapter: self.adapter_id.clone(),
                delivery_id: None,
                timestamp: Some(chrono::Utc::now() - chrono::Duration::seconds(age_secs)),
                event,
                raw: json!({"secret":"do-not-forward"}),
            })
            .await
            .unwrap();
    }

    async fn deliver(&mut self, id: &str, event: Event, age_secs: i64) {
        self.submit(id, event, age_secs).await;
        assert_eq!(
            timeout(WAIT, self.handled.recv()).await.unwrap().as_deref(),
            Some(id)
        );
    }

    async fn command(&mut self, id: &str, target: MessageTarget) {
        self.deliver(id, command_event(id, target), 0).await;
    }

    fn last_token(&self) -> String {
        self.api
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .pointer("/keyboard/content/rows/0/buttons/0/action/data")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn requests(&self) -> Vec<(String, Value)> {
        self.api.requests.lock().unwrap().clone()
    }

    async fn stop(self) {
        self.shutdown.shutdown();
        timeout(WAIT, self.runtime).await.unwrap().unwrap();
        self.server.abort();
    }
}

fn command_event(id: &str, target: MessageTarget) -> Event {
    let actor_id = if let MessageTarget::Private { user_id } = &target {
        user_id.clone()
    } else {
        "reader".to_owned()
    };
    Event::Message(CommonMessage {
        message_id: format!("source-{id}"),
        target,
        sender: Sender {
            id: actor_id,
            display_name: None,
        },
        text: "/md-pages".to_owned(),
        segments: vec![],
        reply_to: None,
    })
}

fn group(id: &str) -> MessageTarget {
    MessageTarget::Group {
        group_id: id.to_owned(),
    }
}

fn c2c() -> MessageTarget {
    MessageTarget::Private {
        user_id: "reader".to_owned(),
    }
}

fn click(token: &str, interaction_id: &str, actor: &str, target: MessageTarget) -> Event {
    Event::ButtonInteraction(ButtonInteraction {
        interaction_id: interaction_id.to_owned(),
        target,
        actor_id: actor.to_owned(),
        button_id: "md-pages-next".to_owned(),
        token: token.to_owned(),
    })
}

fn wasm_package(manifest: &str, component: &[u8]) -> ValidatedPluginPackage {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("plugin.toml", manifest.as_bytes()),
        ("component.wasm", component),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(bytes).unwrap();
    }
    ValidatedPluginPackage::from_bytes(&archive.finish().unwrap().into_inner()).unwrap()
}

async fn wasm_pager() -> Arc<dyn StaticPlugin> {
    let source = include_str!("wasm-plugins/qq-markdown-pages/plugin.toml");
    let component = base64::engine::general_purpose::STANDARD
        .decode(include_str!("wasm-plugins/qq-markdown-pages/component.b64").trim())
        .unwrap();
    Arc::new(
        WasmPlugin::from_package(wasm_package(source, &component))
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn static_and_wasm_have_identical_qq_callback_and_wire_results() {
    let mut all = Vec::new();
    for plugin in [
        Arc::new(QqMarkdownPagesPlugin::default()) as Arc<dyn StaticPlugin>,
        wasm_pager().await,
    ] {
        let mut run = Harness::new(plugin, false).await;
        run.command("private", c2c()).await;
        let token = run.last_token();
        assert!(ButtonInteraction::is_managed_token(&token));
        run.deliver(
            "private-click",
            click(&token, "private-ack", "reader", c2c()),
            0,
        )
        .await;
        // Another dispatch ID for the same interaction cannot produce a second ACK.
        run.deliver(
            "private-click-retry",
            click(&token, "private-ack", "reader", c2c()),
            0,
        )
        .await;
        run.command("group", group("group-a")).await;
        let token = run.last_token();
        for (id, actor, target, age) in [
            ("wrong-chat", "reader", group("group-b"), 0),
            ("wrong-actor", "another-reader", group("group-a"), 0),
            ("expired", "reader", group("group-a"), 6),
            ("bad-ack", "reader", group("group-a"), 0),
            ("unknown-ack", "reader", group("group-a"), 0),
            ("group-ack", "reader", group("group-a"), 0),
        ] {
            run.deliver(id, click(&token, id, actor, target), age).await;
        }
        assert_eq!(run.other.button_deliveries.load(Ordering::Relaxed), 0);
        let calls = checked_wire_calls(run.requests());
        let statuses = checked_receipts(&run.observed.receipts.lock().unwrap());
        all.push((calls, statuses));
        run.stop().await;
    }
    assert_eq!(all[0], all[1]);
}

fn checked_wire_calls(mut calls: Vec<(String, Value)>) -> Vec<(String, Value)> {
    let paths: Vec<_> = calls.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "users/reader",
            "interactions/private-ack",
            "users/reader",
            "groups/group-a",
            "interactions/bad-ack",
            "interactions/unknown-ack",
            "interactions/group-ack",
            "groups/group-a",
        ]
    );
    assert_eq!(calls[0].1["msg_id"], "source-private");
    assert_eq!(calls[3].1["msg_id"], "source-group");
    for i in [2, 7] {
        assert!(calls[i].1.get("msg_id").is_none());
        assert!(calls[i].1.get("event_id").is_none());
    }
    for i in [1, 4, 5, 6] {
        assert_eq!(calls[i].1, json!({"code":0}));
    }
    for i in [0, 2, 3, 7] {
        let body = &mut calls[i].1;
        assert_eq!(body["msg_type"], 2);
        let action = body
            .pointer_mut("/keyboard/content/rows/0/buttons/0/action")
            .unwrap();
        assert_eq!(
            action["permission"],
            json!({"type":0,"specify_user_ids":["reader"]})
        );
        assert!(action.get("enter").is_none());
        action["data"] = json!("normalized-token");
    }
    calls
}

type ReceiptSummary = (String, ActionStatus, bool, Option<String>);

fn checked_receipts(receipts: &[ActionCompleted]) -> Vec<ReceiptSummary> {
    let statuses: Vec<_> = receipts
        .iter()
        .map(|receipt| {
            (
                receipt.kind.clone(),
                receipt.status,
                receipt.retryable,
                receipt.error_message.clone(),
            )
        })
        .collect();
    assert_eq!(statuses.len(), 8);
    assert_eq!(
        (&*statuses[1].0, statuses[1].1),
        (QQ_BUTTON_ACK, ActionStatus::Succeeded)
    );
    assert_eq!(
        (&*statuses[2].0, statuses[2].1),
        (QQ_MARKDOWN_SEND, ActionStatus::Succeeded)
    );
    assert_eq!(
        (&*statuses[4].0, statuses[4].1),
        (QQ_BUTTON_ACK, ActionStatus::Failed)
    );
    assert!(statuses[4].3.as_deref().unwrap().contains("400999"));
    assert!(
        !statuses[4]
            .3
            .as_deref()
            .unwrap()
            .contains("PRIVATE_ACK_DETAIL")
    );
    assert_eq!(
        (&*statuses[5].0, statuses[5].1),
        (QQ_BUTTON_ACK, ActionStatus::Unknown)
    );
    assert_eq!(
        (&*statuses[6].0, statuses[6].1),
        (QQ_BUTTON_ACK, ActionStatus::Succeeded)
    );
    assert_eq!(
        (&*statuses[7].0, statuses[7].1),
        (QQ_MARKDOWN_SEND, ActionStatus::Failed)
    );
    assert!(statuses[7].3.as_deref().unwrap().contains("40034105"));
    assert!(statuses.iter().all(|status| !status.2));
    statuses
}

#[tokio::test]
async fn callback_issuance_requires_grant_and_old_tokens_do_not_survive_a_new_host() {
    let mut denied = Harness::new(wasm_pager().await, true).await;
    denied.command("denied", c2c()).await;
    assert_eq!(denied.requests(), []);
    denied.stop().await;

    let mut original = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
    original.command("original", c2c()).await;
    let token = original.last_token();
    original.stop().await;
    let mut restarted = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
    restarted
        .deliver(
            "after-restart",
            click(&token, "late-ack", "reader", c2c()),
            0,
        )
        .await;
    assert_eq!(restarted.requests(), []);
    restarted.stop().await;
}

#[tokio::test]
async fn successful_ack_cannot_authorize_a_followup_to_another_chat() {
    let mut run = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
    run.command("private", c2c()).await;
    let token = run.last_token();
    run.observed
        .redirect_followup
        .store(true, Ordering::Relaxed);
    run.deliver(
        "redirect-click",
        click(&token, "redirect-ack", "reader", c2c()),
        0,
    )
    .await;
    assert_eq!(run.requests().len(), 2);
    {
        let receipts = run.observed.receipts.lock().unwrap();
        assert_eq!(receipts[1].status, ActionStatus::Succeeded);
        assert_eq!(receipts[2].kind, QQ_MARKDOWN_SEND);
        assert_eq!(receipts[2].status, ActionStatus::Denied);
    }
    run.stop().await;
}

#[tokio::test]
async fn old_component_cannot_claim_the_new_callback_abi() {
    let old_package = base64::engine::general_purpose::STANDARD
        .decode(include_str!("wasm-plugins/ping/component.bkm-plugin.b64").trim())
        .unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(old_package)).unwrap();
    let mut old_component = Vec::new();
    std::io::Read::read_to_end(
        &mut archive.by_name("component.wasm").unwrap(),
        &mut old_component,
    )
    .unwrap();
    let package = wasm_package(
        include_str!("wasm-plugins/qq-markdown-pages/plugin.toml"),
        &old_component,
    );
    let error = WasmPlugin::from_package(package).await.unwrap_err();
    let plugin_host::WasmPluginError::Wasmtime(source) = error else {
        panic!("unexpected plugin error {error:?}");
    };
    assert!(
        source.to_string().contains("require the qq-handler export"),
        "{source:#}"
    );
}

#[tokio::test]
async fn callback_cannot_bypass_the_ack_and_target_policy_using_ordinary_send() {
    let mut run = Harness::new(Arc::new(QqMarkdownPagesPlugin::default()), false).await;
    run.command("private", c2c()).await;
    let token = run.last_token();
    run.observed
        .use_ordinary_send
        .store(true, Ordering::Relaxed);
    run.deliver(
        "plain-send-click",
        click(&token, "unused-ack", "reader", c2c()),
        0,
    )
    .await;
    assert_eq!(run.requests().len(), 1);
    {
        let receipts = run.observed.receipts.lock().unwrap();
        assert_eq!(receipts[1].kind, "message.send");
        assert_eq!(receipts[1].status, ActionStatus::Denied);
    }
    run.stop().await;
}
