use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use base64::Engine as _;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Event, EventEnvelope,
    EventId, MessageSegment, MessageTarget, RuntimeBuilder, Sender, ShutdownHandle, ShutdownSignal,
    shutdown_channel,
};
use builtin_plugins::PingPlugin;
use plugin_api::StaticPlugin;
use plugin_host::{PluginStore, StaticPluginHost, ValidatedPluginPackage, WasmPlugin};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

#[derive(Debug)]
struct MockAdapter {
    id: AdapterId,
    event: Mutex<Option<EventEnvelope>>,
    actions: mpsc::Sender<Action>,
    shutdown: ShutdownHandle,
}

#[async_trait]
impl Adapter for MockAdapter {
    fn id(&self) -> &AdapterId {
        &self.id
    }

    fn platform(&self) -> &'static str {
        "mock"
    }

    async fn run(
        &self,
        events: bot_core::EventSender,
        mut shutdown: ShutdownSignal,
    ) -> Result<(), AdapterError> {
        events.mark_ready();
        if let Some(event) = self.event.lock().await.take() {
            events
                .send(event)
                .await
                .map_err(|_| AdapterError::EventQueueClosed)?;
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        self.actions
            .send(action)
            .await
            .map_err(|_| AdapterError::Action("action observer closed".to_owned()))?;
        self.shutdown.shutdown();
        Ok(ActionResult {
            message_id: Some("wasm-ping-reply".to_owned()),
            raw: Value::Null,
        })
    }
}

fn ping_event() -> EventEnvelope {
    EventEnvelope {
        id: EventId::new("wasm-ping-event"),
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
            text: "/ping".to_owned(),
            segments: vec![MessageSegment::Text {
                text: "/ping".to_owned(),
            }],
            reply_to: None,
        }),
        raw: Value::Null,
    }
}

async fn run_ping(plugin: Arc<dyn StaticPlugin>) -> Action {
    let mut plugins = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    let instance_id = format!("{}/equivalence", plugin.manifest().id);
    plugins
        .register_trusted(plugin, instance_id, BTreeMap::new())
        .await
        .unwrap();
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(ping_event())),
        actions: action_sender,
        shutdown: shutdown_handle,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(plugins))
        .build()
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        runtime.run(shutdown_signal),
    )
    .await
    .expect("WASM ping runtime must not hang")
    .unwrap();
    actions.recv().await.unwrap()
}

#[tokio::test]
async fn wasm_ping_matches_static_ping_through_bpp_host_and_adapter() {
    let package_bytes = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
        )
        .unwrap();
    let package = ValidatedPluginPackage::from_bytes(&package_bytes).unwrap();
    let wasm = Arc::new(WasmPlugin::from_package(package).await.unwrap());

    let static_action = run_ping(Arc::new(PingPlugin::default())).await;
    let wasm_action = run_ping(wasm).await;
    assert_eq!(static_action, wasm_action);
}
