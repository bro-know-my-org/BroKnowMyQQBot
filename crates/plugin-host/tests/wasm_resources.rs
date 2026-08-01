use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::Engine as _;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Event, EventEnvelope,
    EventId, MessageSegment, MessageTarget, RuntimeBuilder, Sender, ShutdownHandle, ShutdownSignal,
    shutdown_channel,
};
use builtin_plugins::PingPlugin;
use plugin_api::{
    HostQueries, InitContext, PluginError, PluginEventEnvelope, StateValue, StaticPlugin,
};
use plugin_host::{
    PluginInstanceState, PluginStore, StaticPluginHost, ValidatedPluginPackage, WasmPlugin,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use zip::{ZipWriter, write::SimpleFileOptions};

const DEFAULT_MANIFEST: &str = r#"
manifest_version = 1
id = "dev.bkm.wasm-faults"
version = "0.1.0"
protocol = ">=1.0,<2.0"
entry = "component.wasm"

[metadata]
default_locale = "en"

[metadata.locales.en]
name = "WASM Faults"

[runtime]
mode = "serial"
max_concurrency = 1
timeout_ms = 3000
memory_mb = 16
fuel = 10000000

[[subscriptions]]
id = "messages"
event = "message.created"
priority = 0
scopes = ["group", "private", "channel"]

[permissions]
actions = ["message.reply"]
event_extensions = []
storage = "none"
storage_quota_bytes = 0
scheduler = false
stop_propagation = false
"#;

const TIMEOUT_MANIFEST: &str = r#"
manifest_version = 1
id = "dev.bkm.wasm-timeout"
version = "0.1.0"
protocol = ">=1.0,<2.0"
entry = "component.wasm"

[metadata]
default_locale = "en"

[metadata.locales.en]
name = "WASM Timeout"

[runtime]
mode = "serial"
max_concurrency = 1
timeout_ms = 20
memory_mb = 16
fuel = 1000000000000

[[subscriptions]]
id = "messages"
event = "message.created"
priority = 0
scopes = ["group"]

[permissions]
actions = ["message.reply"]
event_extensions = []
storage = "none"
storage_quota_bytes = 0
scheduler = false
stop_propagation = false
"#;

#[derive(Debug, Default)]
struct Queries {
    config: BTreeMap<String, Value>,
    state: BTreeMap<String, StateValue>,
    grants: BTreeSet<String>,
}

impl HostQueries for Queries {
    fn config_get(&self, key: &str) -> Option<&Value> {
        self.config.get(key)
    }

    fn state_get(&self, key: &str) -> Option<&StateValue> {
        self.state.get(key)
    }

    fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)> {
        self.state
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .take(limit)
            .map(|(key, value)| (key.as_str(), value))
            .collect()
    }

    fn granted_capabilities(&self) -> &BTreeSet<String> {
        &self.grants
    }

    fn invocation_time_ms(&self) -> i64 {
        1_700_000_000_000
    }
}

fn fixture_component() -> Vec<u8> {
    let package = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
        )
        .unwrap();
    ValidatedPluginPackage::from_bytes(&package)
        .unwrap()
        .component()
        .to_vec()
}

fn package_bytes(manifest: &str) -> Vec<u8> {
    let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    archive.start_file("plugin.toml", options).unwrap();
    archive.write_all(manifest.as_bytes()).unwrap();
    archive.start_file("component.wasm", options).unwrap();
    archive.write_all(&fixture_component()).unwrap();
    archive.finish().unwrap().into_inner()
}

async fn initialized_plugin(manifest: &str) -> Arc<WasmPlugin> {
    let package = ValidatedPluginPackage::from_bytes(&package_bytes(manifest)).unwrap();
    let plugin = Arc::new(WasmPlugin::from_package(package).await.unwrap());
    let grants = BTreeSet::from(["message.reply".to_owned()]);
    plugin
        .init(InitContext {
            protocol_version: "1.0".to_owned(),
            plugin_id: plugin.manifest().id.clone(),
            instance_id: format!("{}/test", plugin.manifest().id),
            granted_capabilities: grants,
            config: BTreeMap::new(),
        })
        .await
        .unwrap();
    plugin
}

fn plugin_event(id: &str, text: &str) -> PluginEventEnvelope {
    PluginEventEnvelope {
        protocol_version: "1.0".to_owned(),
        event_id: id.to_owned(),
        delivery_id: format!("delivery-{id}"),
        invocation_id: format!("invocation-{id}"),
        occurred_at_ms: None,
        received_at_ms: 1_700_000_000_000,
        adapter_id: "mock".to_owned(),
        event_type: "message.created".to_owned(),
        trace_id: None,
        payload: serde_json::to_value(Event::Message(message(text))).unwrap(),
        extensions: Vec::new(),
    }
}

fn message(text: &str) -> CommonMessage {
    CommonMessage {
        message_id: format!("message-{text}"),
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
    }
}

#[tokio::test]
async fn trap_fuel_and_memory_limits_are_classified() {
    for (command, expected) in [
        ("/wasm-trap", "guest_trap"),
        ("/wasm-fuel", "resource_exhausted"),
        ("/wasm-memory", "resource_exhausted"),
    ] {
        let plugin = initialized_plugin(DEFAULT_MANIFEST).await;
        let queries = Queries::default();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            plugin.on_event(&plugin_event(command, command), &queries),
        )
        .await
        .expect("fault fixture must be interrupted")
        .unwrap_err();
        match (expected, error) {
            ("guest_trap", PluginError::GuestTrap(_))
            | ("resource_exhausted", PluginError::ResourceExhaustedTrap(_)) => {}
            (_, error) => panic!("unexpected error for {command}: {error}"),
        }
    }
}

#[tokio::test]
async fn epoch_deadline_interrupts_compute_before_high_fuel_budget() {
    let plugin = initialized_plugin(TIMEOUT_MANIFEST).await;
    let queries = Queries::default();
    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        plugin.on_event(&plugin_event("timeout", "/wasm-timeout"), &queries),
    )
    .await
    .expect("epoch deadline must interrupt pure compute")
    .unwrap_err();
    assert!(matches!(error, PluginError::ResourceExhaustedTrap(_)));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[derive(Debug)]
struct FaultSequenceAdapter {
    id: AdapterId,
    events: Mutex<Vec<EventEnvelope>>,
    actions: mpsc::Sender<Action>,
    shutdown: ShutdownHandle,
}

#[async_trait]
impl Adapter for FaultSequenceAdapter {
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
        for event in std::mem::take(&mut *self.events.lock().await) {
            events
                .send(event)
                .await
                .map_err(|_| AdapterError::EventQueueClosed)?;
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        let is_recovery = matches!(&action, Action::Reply(reply) if reply.content == "pong");
        self.actions
            .send(action)
            .await
            .map_err(|_| AdapterError::Action("action observer closed".to_owned()))?;
        if is_recovery {
            self.shutdown.shutdown();
        }
        Ok(ActionResult {
            message_id: Some("fixture-reply".to_owned()),
            raw: Value::Null,
        })
    }
}

fn runtime_event(index: usize, text: &str) -> EventEnvelope {
    EventEnvelope {
        id: EventId::new(format!("fault-{index}")),
        adapter: AdapterId::new("mock"),
        delivery_id: None,
        timestamp: None,
        event: Event::Message(message(text)),
        raw: Value::Null,
    }
}

async fn run_fault_then_static_ping(manifest: &str, fault: &str) -> (Action, PluginInstanceState) {
    let plugin = initialized_plugin(manifest).await;
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    let instance_id = format!("dev.bkm.wasm-faults/{}", fault.trim_start_matches('/'));
    host.register(
        plugin,
        instance_id.clone(),
        BTreeMap::new(),
        BTreeSet::from(["message.reply".to_owned()]),
    )
    .await
    .unwrap();
    host.register_trusted(
        Arc::new(PingPlugin::default()),
        format!("dev.bkm.ping/{fault}"),
        BTreeMap::new(),
    )
    .await
    .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(8);
    let adapter = Arc::new(FaultSequenceAdapter {
        id: AdapterId::new("mock"),
        events: Mutex::new(vec![runtime_event(1, fault), runtime_event(2, "/ping")]),
        actions: action_sender,
        shutdown: shutdown_handle,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(host.clone()))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), runtime.run(shutdown_signal))
        .await
        .expect("WASM fault must not stop later plugins")
        .unwrap();
    (
        actions.recv().await.unwrap(),
        host.instance_state(&instance_id).unwrap().unwrap(),
    )
}

#[tokio::test]
async fn trapped_or_exhausted_wasm_does_not_stop_later_static_plugin() {
    for (manifest, fault) in [
        (DEFAULT_MANIFEST, "/wasm-trap"),
        (DEFAULT_MANIFEST, "/wasm-fuel"),
        (DEFAULT_MANIFEST, "/wasm-memory"),
        (TIMEOUT_MANIFEST, "/wasm-timeout"),
    ] {
        let (action, state) = run_fault_then_static_ping(manifest, fault).await;
        assert!(matches!(action, Action::Reply(reply) if reply.content == "pong"));
        assert_eq!(state, PluginInstanceState::Disabled);
    }
}

#[tokio::test]
async fn invalid_or_oversized_wasm_outputs_do_not_reach_adapter_or_stop_runtime() {
    let plugin = initialized_plugin(DEFAULT_MANIFEST).await;
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register(
        plugin,
        "dev.bkm.wasm-faults/runtime",
        BTreeMap::new(),
        BTreeSet::from(["message.reply".to_owned()]),
    )
    .await
    .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(8);
    let adapter = Arc::new(FaultSequenceAdapter {
        id: AdapterId::new("mock"),
        events: Mutex::new(vec![
            runtime_event(1, "/wasm-output"),
            runtime_event(2, "/ping"),
        ]),
        actions: action_sender,
        shutdown: shutdown_handle,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(host))
        .build()
        .unwrap();

    tokio::time::timeout(Duration::from_secs(10), runtime.run(shutdown_signal))
        .await
        .expect("runtime must survive oversized WASM output")
        .unwrap();
    let action = actions.recv().await.unwrap();
    assert!(matches!(action, Action::Reply(reply) if reply.content == "pong"));
    assert!(actions.try_recv().is_err());
}
