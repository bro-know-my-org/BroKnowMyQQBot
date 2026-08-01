use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Event, EventEnvelope,
    EventId, MessageSegment, MessageTarget, RuntimeBuilder, Sender, ShutdownHandle, ShutdownSignal,
    shutdown_channel,
};
use builtin_plugins::{
    ActionResultProbePlugin, ConfigProbePlugin, HttpProbePlugin, PingPlugin,
    QqExtensionProbePlugin, SchedulerProbePlugin,
};
use plugin_api::{
    ActionCompleted, ActionStatus, HandlerOutput, HostQueries, HttpPermission, HttpRequest,
    HttpResponse, PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin,
};
use plugin_fixtures::{
    MigrationFixturePlugin, PanicFixturePlugin, PartitionFixturePlugin, QuotaFixturePlugin,
    TimeoutFixturePlugin,
};
use plugin_host::{
    CommitOptions, HttpExecutionError, HttpExecutor, PluginHostError, PluginInstanceState,
    PluginStore, SecureHttpExecutor, StaticPluginHost,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug)]
struct MockHttpExecutor;

#[derive(Debug)]
struct ShutdownProbePlugin {
    inner: PingPlugin,
    calls: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct TransientProbePlugin {
    inner: PingPlugin,
    calls: Arc<AtomicUsize>,
    attempt_ids: Arc<StdMutex<Vec<(String, String)>>>,
    failures: usize,
}

fn assert_unique_attempt_ids(attempt_ids: &Arc<StdMutex<Vec<(String, String)>>>, expected: usize) {
    let attempt_ids = attempt_ids.lock().unwrap();
    assert_eq!(attempt_ids.len(), expected);
    assert_eq!(
        attempt_ids
            .iter()
            .map(|(delivery_id, _)| delivery_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected
    );
    assert_eq!(
        attempt_ids
            .iter()
            .map(|(_, invocation_id)| invocation_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected
    );
}

#[async_trait]
impl StaticPlugin for TransientProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        self.inner.manifest()
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        self.attempt_ids
            .lock()
            .unwrap()
            .push((event.delivery_id.clone(), event.invocation_id.clone()));
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.failures {
            return Err(PluginError::Transient("try again".to_owned()));
        }
        self.inner.on_event(event, queries).await
    }
}

#[async_trait]
impl StaticPlugin for ShutdownProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        self.inner.manifest()
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        self.inner.on_event(event, queries).await
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl HttpExecutor for MockHttpExecutor {
    async fn execute(
        &self,
        permissions: &[HttpPermission],
        granted_capabilities: &std::collections::BTreeSet<String>,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpExecutionError> {
        assert_eq!(permissions[0].host, "example.com");
        assert!(granted_capabilities.contains("http.request"));
        assert_eq!(request.url, "https://example.com/");
        Ok(HttpResponse {
            status: 204,
            final_url: request.url.clone(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        })
    }
}

#[tokio::test]
async fn host_shutdown_drains_plugins_and_invokes_lifecycle_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register_trusted(
        Arc::new(ShutdownProbePlugin {
            inner: PingPlugin::default(),
            calls: calls.clone(),
        }),
        "dev.bkm.ping/shutdown",
        BTreeMap::new(),
    )
    .await
    .unwrap();

    host.shutdown().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.instance_state("dev.bkm.ping/shutdown").unwrap(),
        Some(PluginInstanceState::Stopped)
    );

    host.shutdown().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[derive(Debug)]
struct MockAdapter {
    id: AdapterId,
    event: Mutex<Option<EventEnvelope>>,
    actions: mpsc::Sender<Action>,
    shutdown: ShutdownHandle,
    outcome: ActionOutcome,
    shutdown_after_send: bool,
}

#[derive(Debug)]
struct MultiEventAdapter {
    id: AdapterId,
    events: Mutex<Vec<EventEnvelope>>,
    handled: AtomicUsize,
    expected: usize,
    shutdown: ShutdownHandle,
}

#[async_trait]
impl Adapter for MultiEventAdapter {
    fn id(&self) -> &AdapterId {
        &self.id
    }

    fn platform(&self) -> &'static str {
        "mock"
    }

    async fn run(
        &self,
        events: mpsc::Sender<EventEnvelope>,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        let pending = std::mem::take(&mut *self.events.lock().await);
        for event in pending {
            events
                .send(event)
                .await
                .map_err(|_| AdapterError::EventQueueClosed)?;
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn execute(&self, _action: Action) -> Result<ActionResult, AdapterError> {
        Err(AdapterError::Action(
            "partition fixture must not execute actions".to_owned(),
        ))
    }

    async fn event_handled(&self, _event: &EventEnvelope) -> Result<(), AdapterError> {
        if self.handled.fetch_add(1, Ordering::SeqCst) + 1 == self.expected {
            self.shutdown.shutdown();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum ActionOutcome {
    Succeeded,
    Failed,
    Unknown,
}

#[async_trait]
impl Adapter for MockAdapter {
    fn id(&self) -> &AdapterId {
        &self.id
    }

    fn platform(&self) -> &'static str {
        "qq.official"
    }

    async fn run(
        &self,
        events: mpsc::Sender<EventEnvelope>,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        if let Some(event) = self.event.lock().await.take() {
            events
                .send(event)
                .await
                .map_err(|_| AdapterError::EventQueueClosed)?;
        }
        if self.shutdown_after_send {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.shutdown.shutdown();
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        self.actions
            .send(action)
            .await
            .map_err(|_| AdapterError::Action("action receiver closed".to_owned()))?;
        self.shutdown.shutdown();
        match self.outcome {
            ActionOutcome::Succeeded => Ok(ActionResult {
                message_id: Some("plugin-reply".to_owned()),
                raw: Value::Null,
            }),
            ActionOutcome::Failed => Err(AdapterError::Action("definitive rejection".to_owned())),
            ActionOutcome::Unknown => Err(AdapterError::ActionUnknown(
                "connection closed after request write".to_owned(),
            )),
        }
    }
}

fn message_event(text: &str) -> EventEnvelope {
    EventEnvelope {
        id: EventId::new("plugin-event"),
        adapter: AdapterId::new("mock"),
        delivery_id: None,
        timestamp: None,
        event: Event::Message(CommonMessage {
            message_id: "source-message".to_owned(),
            target: MessageTarget::Group {
                group_id: "group".to_owned(),
            },
            sender: Sender {
                id: "user".to_owned(),
                display_name: None,
            },
            text: text.to_owned(),
            segments: vec![MessageSegment::Text {
                text: text.to_owned(),
            }],
            reply_to: None,
        }),
        raw: json!({"test":true}),
    }
}

fn group_event(id: &str, group_id: &str, text: &str) -> EventEnvelope {
    let mut event = message_event(text);
    event.id = EventId::new(id);
    let Event::Message(message) = &mut event.event else {
        unreachable!("message_event always creates a message")
    };
    message.message_id = format!("message-{id}");
    message.target = MessageTarget::Group {
        group_id: group_id.to_owned(),
    };
    event
}

#[tokio::test]
async fn static_ping_plugin_runs_through_runtime_and_original_adapter() {
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    plugins
        .register_trusted(
            Arc::new(PingPlugin::default()),
            "dev.bkm.ping/test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/ping"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("expected plugin reply");
    };
    assert_eq!(reply.content, "pong");
    assert_eq!(reply.source_message_id, "source-message");
}

async fn run_qq_extension_probe(grant_raw_extension: bool) -> String {
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    let mut grants = std::collections::BTreeSet::from(["message.reply".to_owned()]);
    if grant_raw_extension {
        grants.insert("event.extension.qq.official.raw".to_owned());
    }
    plugins
        .register(
            Arc::new(QqExtensionProbePlugin::default()),
            "dev.bkm.qq-extension-probe/test",
            BTreeMap::new(),
            grants,
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let mut event = message_event("/qq-extension");
    event.raw = json!({
        "op": 0,
        "t": "GROUP_AT_MESSAGE_CREATE",
        "d": {
            "content": "/qq-extension",
            "token": "must-not-reach-plugin"
        }
    });
    event.adapter = AdapterId::new("qq-test-instance");
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("qq-test-instance"),
        event: Mutex::new(Some(event)),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("expected QQ extension probe reply");
    };
    reply.content
}

#[tokio::test]
async fn qq_raw_extension_requires_manifest_and_administrator_grant() {
    assert_eq!(
        run_qq_extension_probe(true).await,
        "qq extension: GROUP_AT_MESSAGE_CREATE"
    );
    assert_eq!(
        run_qq_extension_probe(false).await,
        "qq extension unavailable"
    );
}

async fn run_action_result_probe(outcome: ActionOutcome) -> ActionCompleted {
    let store = PluginStore::in_memory().unwrap();
    let mut plugins = StaticPluginHost::new(store.clone());
    plugins
        .register_trusted(
            Arc::new(ActionResultProbePlugin::default()),
            "dev.bkm.action-result-probe/test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/probe-action-result"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    assert!(matches!(actions.recv().await, Some(Action::Reply(_))));
    let state = store.snapshot("dev.bkm.action-result-probe/test").unwrap();
    assert_eq!(state.len(), 1);
    serde_json::from_slice(&state.values().next().unwrap().value).unwrap()
}

#[tokio::test]
async fn action_completed_reports_success_failure_and_unknown() {
    let succeeded = run_action_result_probe(ActionOutcome::Succeeded).await;
    assert_eq!(succeeded.status, ActionStatus::Succeeded);
    assert!(!succeeded.retryable);
    assert_eq!(succeeded.result.unwrap()["message_id"], "plugin-reply");

    let failed = run_action_result_probe(ActionOutcome::Failed).await;
    assert_eq!(failed.status, ActionStatus::Failed);
    assert!(!failed.retryable);
    assert_eq!(failed.error_code.as_deref(), Some("permanent"));

    let unknown = run_action_result_probe(ActionOutcome::Unknown).await;
    assert_eq!(unknown.status, ActionStatus::Unknown);
    assert!(!unknown.retryable);
    assert_eq!(unknown.error_code.as_deref(), Some("result_unknown"));
}

async fn run_http_probe(
    executor: Arc<dyn HttpExecutor>,
    config: BTreeMap<String, Value>,
) -> (Action, BTreeMap<String, plugin_api::StateValue>) {
    let store = PluginStore::in_memory().unwrap();
    let mut plugins = StaticPluginHost::new(store.clone()).with_http_executor(executor);
    plugins
        .register_trusted(
            Arc::new(HttpProbePlugin::default()),
            "dev.bkm.http-probe/test",
            config,
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/http-probe"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    let action = actions.recv().await.unwrap();
    let state = store.snapshot("dev.bkm.http-probe/test").unwrap();
    (action, state)
}

#[tokio::test]
async fn http_probe_uses_host_executor_and_receives_success() {
    let (action, state) = run_http_probe(Arc::new(MockHttpExecutor), BTreeMap::new()).await;
    let Action::Reply(reply) = action else {
        panic!("expected HTTP result reply");
    };
    assert_eq!(reply.content, "HTTP probe succeeded: 204");
    let summary: Value = serde_json::from_slice(&state["results/http"].value).unwrap();
    assert_eq!(summary["status"], "succeeded");
}

#[tokio::test]
async fn http_probe_rejects_ip_literal_before_network_access() {
    let (action, state) = run_http_probe(
        Arc::new(SecureHttpExecutor),
        BTreeMap::from([(
            "url".to_owned(),
            Value::String("https://169.254.169.254/latest/meta-data/".to_owned()),
        )]),
    )
    .await;
    let Action::Reply(reply) = action else {
        panic!("expected denied HTTP result reply");
    };
    assert!(reply.content.contains("Denied"));
    let summary: Value = serde_json::from_slice(&state["results/http"].value).unwrap();
    assert_eq!(summary["status"], "denied");
    assert_eq!(summary["error_code"], "permission_denied");
}

#[tokio::test]
async fn timeout_and_panic_are_isolated_from_later_plugins() {
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    for (plugin, instance_id) in [
        (
            Arc::new(TimeoutFixturePlugin::default()) as Arc<dyn plugin_api::StaticPlugin>,
            "dev.bkm.timeout-fixture/test",
        ),
        (
            Arc::new(PanicFixturePlugin::default()) as Arc<dyn plugin_api::StaticPlugin>,
            "dev.bkm.panic-fixture/test",
        ),
        (
            Arc::new(PingPlugin::default()) as Arc<dyn plugin_api::StaticPlugin>,
            "dev.bkm.ping/isolation-test",
        ),
    ] {
        plugins
            .register_trusted(plugin, instance_id, BTreeMap::new())
            .await
            .unwrap();
    }
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/ping"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("later ping plugin should still execute");
    };
    assert_eq!(reply.content, "pong");
}

#[tokio::test]
async fn transient_delivery_retries_with_new_attempts_and_succeeds() {
    let calls = Arc::new(AtomicUsize::new(0));
    let attempt_ids = Arc::new(StdMutex::new(Vec::new()));
    let store = PluginStore::in_memory().unwrap();
    let mut plugins = StaticPluginHost::new(store.clone());
    plugins
        .register_trusted(
            Arc::new(TransientProbePlugin {
                inner: PingPlugin::default(),
                calls: calls.clone(),
                attempt_ids: attempt_ids.clone(),
                failures: 2,
            }),
            "dev.bkm.ping/retry-test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/ping"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_unique_attempt_ids(&attempt_ids, 3);
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("retrying plugin should eventually reply");
    };
    assert_eq!(reply.content, "pong");
    assert!(
        store
            .dead_letters(Some("dev.bkm.ping/retry-test"))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn dead_letter_recovery_replays_after_host_restart() {
    let calls = Arc::new(AtomicUsize::new(0));
    let attempt_ids = Arc::new(StdMutex::new(Vec::new()));
    let store = PluginStore::in_memory().unwrap();
    let plugin = Arc::new(TransientProbePlugin {
        inner: PingPlugin::default(),
        calls: calls.clone(),
        attempt_ids: attempt_ids.clone(),
        failures: 3,
    });
    let mut first_host = StaticPluginHost::new(store.clone());
    first_host
        .register_trusted(
            plugin.clone(),
            "dev.bkm.ping/recovery-test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, _actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/ping"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: true,
    });
    RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(first_host))
        .build()
        .unwrap()
        .run(shutdown_signal)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        store
            .dead_letters(Some("dev.bkm.ping/recovery-test"))
            .unwrap()
            .len(),
        1
    );
    let mut circuit_restart = StaticPluginHost::new(store.clone());
    circuit_restart
        .register_trusted(
            plugin.clone(),
            "dev.bkm.ping/recovery-test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        circuit_restart
            .instance_state("dev.bkm.ping/recovery-test")
            .unwrap(),
        Some(PluginInstanceState::Disabled)
    );
    assert_eq!(
        store
            .recover_dead_letters("dev.bkm.ping/recovery-test", 0)
            .unwrap(),
        1
    );

    let (shutdown_handle, _shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(None),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let mut restarted = StaticPluginHost::new(store.clone()).with_adapter(adapter);
    restarted
        .register_trusted(plugin, "dev.bkm.ping/recovery-test", BTreeMap::new())
        .await
        .unwrap();
    let action = tokio::time::timeout(std::time::Duration::from_secs(2), actions.recv())
        .await
        .expect("recovered delivery should emit an action before the timeout")
        .expect("action channel should remain open");
    let Action::Reply(reply) = action else {
        panic!("recovered delivery should execute its original reply");
    };
    assert_eq!(reply.content, "pong");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_unique_attempt_ids(&attempt_ids, 4);
    assert!(
        store
            .dead_letters(Some("dev.bkm.ping/recovery-test"))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn quota_failure_rolls_back_state_and_command() {
    let store = PluginStore::in_memory().unwrap();
    let mut plugins = StaticPluginHost::new(store.clone());
    plugins
        .register_trusted(
            Arc::new(QuotaFixturePlugin::default()),
            "dev.bkm.quota-fixture/test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/quota-fixture"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: true,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    assert!(
        store
            .snapshot("dev.bkm.quota-fixture/test")
            .unwrap()
            .is_empty()
    );
    assert!(actions.try_recv().is_err());
}

async fn persist_schedule_then_stop() -> PluginStore {
    let store = PluginStore::in_memory().unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(4);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/schedule 1"))),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let routed_adapter: Arc<dyn Adapter> = adapter.clone();
    let mut plugins = StaticPluginHost::new(store.clone()).with_adapter(routed_adapter);
    plugins
        .register_trusted(
            Arc::new(SchedulerProbePlugin::default()),
            "dev.bkm.scheduler-probe/test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let plugins = Arc::new(plugins);
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(plugins.clone())
        .build()
        .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.run(shutdown_signal),
    )
    .await
    .expect("schedule creation runtime should stop after confirmation")
    .unwrap();
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("schedule creation should receive an immediate confirmation reply");
    };
    assert!(reply.content.contains("scheduled task probe-"));
    assert!(reply.content.ends_with("in 1s"));
    plugins.stop_schedulers().unwrap();
    store
}

#[tokio::test]
async fn persisted_schedule_recovers_and_fires_after_host_restart() {
    let store = persist_schedule_then_stop().await;
    let (shutdown_handle, _shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(4);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(None),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let routed_adapter: Arc<dyn Adapter> = adapter;
    let mut plugins = StaticPluginHost::new(store).with_adapter(routed_adapter);
    plugins
        .register_trusted(
            Arc::new(SchedulerProbePlugin::default()),
            "dev.bkm.scheduler-probe/test",
            BTreeMap::new(),
        )
        .await
        .unwrap();

    let action = tokio::time::timeout(std::time::Duration::from_secs(2), actions.recv())
        .await
        .expect("recovered schedule should fire")
        .unwrap();
    let Action::SendMessage(message) = action else {
        panic!("scheduled trigger should actively send a message");
    };
    assert_eq!(message.content, "scheduled task fired");
    assert_eq!(
        message.target,
        MessageTarget::Group {
            group_id: "group".to_owned()
        }
    );
    plugins.stop_schedulers().unwrap();
}

#[tokio::test]
async fn recovered_schedule_can_be_cancelled_before_it_fires() {
    let store = persist_schedule_then_stop().await;
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(4);
    let task_id = "probe-706c7567696e2d6576656e74";
    let mut cancel_event = message_event(&format!("/schedule-cancel {task_id}"));
    cancel_event.id = EventId::new("schedule-cancel-event");
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(cancel_event)),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let routed_adapter: Arc<dyn Adapter> = adapter.clone();
    let mut plugins = StaticPluginHost::new(store).with_adapter(routed_adapter);
    plugins
        .register_trusted(
            Arc::new(SchedulerProbePlugin::default()),
            "dev.bkm.scheduler-probe/test",
            BTreeMap::new(),
        )
        .await
        .unwrap();
    let plugins = Arc::new(plugins);
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(plugins.clone())
        .build()
        .unwrap();

    let run = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runtime.run(shutdown_signal),
    )
    .await;
    assert!(
        run.is_ok(),
        "schedule cancellation runtime should stop after result reply; pending action: {:?}",
        actions.try_recv()
    );
    run.unwrap().unwrap();
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("schedule cancellation should receive a result reply");
    };
    assert_eq!(reply.content, format!("cancelled task {task_id}"));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1_200), actions.recv())
            .await
            .is_err(),
        "cancelled schedule must not produce message.send"
    );
    plugins.stop_schedulers().unwrap();
}

async fn run_config_probe(host: Arc<StaticPluginHost>) -> String {
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let mut event = message_event("/config-probe");
    event.id = EventId::new(format!("config-{}", uuid::Uuid::new_v4()));
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(event)),
        actions: action_sender,
        shutdown: shutdown_handle,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: false,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(host)
        .build()
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.run(shutdown_signal),
    )
    .await
    .expect("config probe runtime should shut down")
    .unwrap();
    let Action::Reply(reply) = actions.recv().await.unwrap() else {
        panic!("expected config probe reply");
    };
    reply.content
}

#[tokio::test]
async fn config_update_validates_switches_and_rolls_back_failed_init() {
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register_trusted(
        Arc::new(ConfigProbePlugin::default()),
        "dev.bkm.config-probe/test",
        BTreeMap::from([("prefix".to_owned(), Value::String("old".to_owned()))]),
    )
    .await
    .unwrap();
    let host = Arc::new(host);
    assert_eq!(run_config_probe(host.clone()).await, "old: ok");

    host.update_config(
        "dev.bkm.config-probe/test",
        BTreeMap::from([("prefix".to_owned(), Value::String("new".to_owned()))]),
    )
    .await
    .unwrap();
    assert_eq!(run_config_probe(host.clone()).await, "new: ok");

    let schema_error = host
        .update_config("dev.bkm.config-probe/test", BTreeMap::new())
        .await
        .unwrap_err();
    assert!(matches!(
        schema_error,
        PluginHostError::InvalidConfig { message, .. }
            if message.contains("JSON Schema validation failed")
    ));
    assert_eq!(run_config_probe(host.clone()).await, "new: ok");

    assert!(
        host.update_config(
            "dev.bkm.config-probe/test",
            BTreeMap::from([
                ("prefix".to_owned(), Value::String("broken".to_owned())),
                ("fail_init".to_owned(), Value::Bool(true)),
            ]),
        )
        .await
        .is_err()
    );
    assert_eq!(
        host.instance_state("dev.bkm.config-probe/test").unwrap(),
        Some(PluginInstanceState::Ready)
    );
    assert_eq!(run_config_probe(host).await, "new: ok");
}

#[tokio::test]
async fn config_update_drains_inflight_invocation_before_switching() {
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register_trusted(
        Arc::new(ConfigProbePlugin::default()),
        "dev.bkm.config-probe/draining",
        BTreeMap::from([
            ("prefix".to_owned(), Value::String("old".to_owned())),
            ("delay_ms".to_owned(), Value::from(250)),
        ]),
    )
    .await
    .unwrap();
    let host = Arc::new(host);
    let run = tokio::spawn(run_config_probe(host.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let update_host = host.clone();
    let update = tokio::spawn(async move {
        update_host
            .update_config(
                "dev.bkm.config-probe/draining",
                BTreeMap::from([("prefix".to_owned(), Value::String("new".to_owned()))]),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        host.instance_state("dev.bkm.config-probe/draining")
            .unwrap(),
        Some(PluginInstanceState::Draining)
    );
    assert_eq!(run.await.unwrap(), "old: ok");
    update.await.unwrap().unwrap();
    assert_eq!(run_config_probe(host).await, "new: ok");
}

#[tokio::test]
async fn queued_old_generation_is_not_run_after_config_switch() {
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register_trusted(
        Arc::new(ConfigProbePlugin::default()),
        "dev.bkm.config-probe/generation",
        BTreeMap::from([
            ("prefix".to_owned(), Value::String("old".to_owned())),
            ("delay_ms".to_owned(), Value::from(250)),
        ]),
    )
    .await
    .unwrap();
    let host = Arc::new(host);
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let adapter = Arc::new(MultiEventAdapter {
        id: AdapterId::new("mock"),
        events: Mutex::new(vec![
            group_event("config-a", "group-a", "/config-probe"),
            group_event("config-b", "group-b", "/config-probe"),
        ]),
        handled: AtomicUsize::new(0),
        expected: usize::MAX,
        shutdown: shutdown_handle.clone(),
    });
    let runtime = RuntimeBuilder::new()
        .event_concurrency(2)
        .adapter(adapter.clone())
        .handler(host.clone())
        .build()
        .unwrap();
    let run = tokio::spawn(runtime.run(shutdown_signal));
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    host.update_config(
        "dev.bkm.config-probe/generation",
        BTreeMap::from([("prefix".to_owned(), Value::String("new".to_owned()))]),
    )
    .await
    .unwrap();
    shutdown_handle.shutdown();
    run.await.unwrap().unwrap();
    assert_eq!(
        adapter.handled.load(Ordering::SeqCst),
        2,
        "an isolated lifecycle race must not force a platform replay"
    );
}

async fn migration_host() -> (StaticPluginHost, PluginStore) {
    let store = PluginStore::in_memory().unwrap();
    let instance_id = "dev.bkm.migration-fixture/test";
    store
        .commit(
            instance_id,
            "seed",
            &[plugin_api::StateOp::Put {
                key: "value".to_owned(),
                value: b"v1".to_vec(),
                expected_revision: None,
            }],
            &[],
            CommitOptions::new(1024),
        )
        .unwrap();
    let mut host = StaticPluginHost::new(store.clone());
    host.register_trusted(
        Arc::new(MigrationFixturePlugin::version_one()),
        instance_id,
        BTreeMap::new(),
    )
    .await
    .unwrap();
    (host, store)
}

#[tokio::test]
async fn plugin_upgrade_migrates_state_and_switches_manifest() {
    let (mut host, store) = migration_host().await;
    host.upgrade_trusted(
        "dev.bkm.migration-fixture/test",
        Arc::new(MigrationFixturePlugin::version_two()),
        BTreeMap::new(),
    )
    .await
    .unwrap();
    let state = store.snapshot("dev.bkm.migration-fixture/test").unwrap();
    assert_eq!(state["value"].value, b"v2");
    assert_eq!(state["value"].revision, 2);
    assert_eq!(
        host.instance_manifest("dev.bkm.migration-fixture/test")
            .unwrap()
            .state_version,
        2
    );
}

#[tokio::test]
async fn failed_migration_or_new_init_restores_old_state_and_plugin() {
    for candidate in [
        MigrationFixturePlugin::failing_migration(),
        MigrationFixturePlugin::failing_init(),
    ] {
        let (mut host, store) = migration_host().await;
        assert!(
            host.upgrade_trusted(
                "dev.bkm.migration-fixture/test",
                Arc::new(candidate),
                BTreeMap::new(),
            )
            .await
            .is_err()
        );
        let state = store.snapshot("dev.bkm.migration-fixture/test").unwrap();
        assert_eq!(state["value"].value, b"v1");
        assert_eq!(state["value"].revision, 1);
        assert_eq!(
            host.instance_manifest("dev.bkm.migration-fixture/test")
                .unwrap()
                .state_version,
            1
        );
        assert_eq!(
            host.instance_state("dev.bkm.migration-fixture/test")
                .unwrap(),
            Some(PluginInstanceState::Ready)
        );
    }
}

#[tokio::test]
async fn partitioned_plugin_preserves_group_order_and_runs_groups_concurrently() {
    let store = PluginStore::in_memory().unwrap();
    let (plugin, metrics) = PartitionFixturePlugin::instrumented();
    let mut host = StaticPluginHost::new(store.clone());
    host.register_trusted(
        Arc::new(plugin),
        "dev.bkm.partition-fixture/test",
        BTreeMap::new(),
    )
    .await
    .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let adapter = Arc::new(MultiEventAdapter {
        id: AdapterId::new("mock"),
        events: Mutex::new(vec![
            group_event("a-1", "group-a", "1"),
            group_event("a-2", "group-a", "2"),
            group_event("b-1", "group-b", "1"),
        ]),
        handled: AtomicUsize::new(0),
        expected: 3,
        shutdown: shutdown_handle,
    });
    let runtime = RuntimeBuilder::new()
        .event_concurrency(3)
        .adapter(adapter)
        .handler(Arc::new(host))
        .build()
        .unwrap();

    runtime.run(shutdown_signal).await.unwrap();
    assert_eq!(metrics.max_active(), 2);
    let records = metrics.records();
    let finish_a1 = records
        .iter()
        .position(|record| record == "finish:group-a:1")
        .unwrap();
    let start_a2 = records
        .iter()
        .position(|record| record == "start:group-a:2")
        .unwrap();
    assert!(finish_a1 < start_a2, "same partition must remain ordered");

    let state = store.snapshot("dev.bkm.partition-fixture/test").unwrap();
    assert_eq!(state["partition/group-a"].value, b"2");
    assert_eq!(state["partition/group-a"].revision, 2);
    assert_eq!(state["partition/group-b"].value, b"1");
}
