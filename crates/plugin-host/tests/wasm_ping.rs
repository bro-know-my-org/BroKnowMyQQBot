use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use base64::Engine as _;
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Event, EventEnvelope,
    EventId, MessageSegment, MessageTarget, RuntimeBuilder, Sender, ShutdownHandle, ShutdownSignal,
    shutdown_channel,
};
use builtin_plugins::{AdminPlugin, EchoPlugin, PingPlugin};
use plugin_api::{StateOp, StaticPlugin};
use plugin_host::{
    CommitOptions, PluginStore, StaticPluginHost, TrustedAuthorityProvider,
    TrustedPluginCapabilities, ValidatedPluginPackage, WasmPlugin,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug)]
struct MockAdapter {
    id: AdapterId,
    platform: &'static str,
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
        self.platform
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
    run_plugin(plugins, ping_event(), "mock").await
}

async fn run_plugin(
    plugins: StaticPluginHost,
    event: EventEnvelope,
    platform: &'static str,
) -> Action {
    let (shutdown_handle, shutdown_signal) = shutdown_channel();
    let (action_sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: event.adapter.clone(),
        platform,
        event: Mutex::new(Some(event)),
        actions: action_sender,
        shutdown: shutdown_handle,
    });
    let plugins = plugins.with_adapter(adapter.clone());
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
    // This unchanged package imports BPP 1.2; the current Host also serves it.
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

#[tokio::test]
async fn unicode_command_prefix_and_multiline_echo_match_between_static_and_wasm() {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../../test-support/wasm-plugins/ping/commands.bkm-plugin.b64").trim(),
        )
        .unwrap();
    let wasm = Arc::new(
        WasmPlugin::from_package(ValidatedPluginPackage::from_bytes(&bytes).unwrap())
            .await
            .unwrap(),
    );
    for prefix in ["!!", "喵："] {
        for plugin in [
            Arc::new(EchoPlugin::default()) as Arc<dyn StaticPlugin>,
            wasm.clone(),
        ] {
            let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap())
                .with_command_prefix(prefix)
                .unwrap();
            host.register_trusted(plugin, "echo/test", BTreeMap::new())
                .await
                .unwrap();
            let mut event = ping_event();
            let Event::Message(message) = &mut event.event else {
                unreachable!()
            };
            message.text = format!("  {prefix}echo 第一行\n\n 第二行\r\n ");
            message.segments = vec![MessageSegment::Text {
                text: message.text.clone(),
            }];
            let Action::Reply(reply) = run_plugin(host, event, "mock").await else {
                panic!("expected echo reply")
            };
            assert_eq!(reply.content, "第一行\n\n 第二行\r\n ");
        }
        // The fixture asserts that no command context survives a lifecycle call.
        wasm.health().await.unwrap();
    }
}

#[tokio::test]
async fn wasm_receives_current_platform_bound_authority_and_lifecycle_does_not_keep_it() {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../../test-support/wasm-plugins/ping/authority.bkm-plugin.b64").trim(),
        )
        .unwrap();
    let wasm = Arc::new(
        WasmPlugin::from_package(ValidatedPluginPackage::from_bytes(&bytes).unwrap())
            .await
            .unwrap(),
    );
    for (platform, sender, delegated, expected) in [
        ("qq.official", "owner-id", false, "Owner"),
        ("qq.official", "admin-id", true, "Admin"),
        ("qq.official", "admin-id", false, "User"),
        ("qq.official", "ordinary-id", false, "User"),
        ("onebot11", "owner-id", false, "User"),
        ("onebot11", "admin-id", true, "User"),
    ] {
        let store = PluginStore::in_memory().unwrap();
        let mut host = StaticPluginHost::new(store.clone());
        host.register_trusted_with_capabilities(
            Arc::new(AdminPlugin::default()),
            "dev.bkm.admin/test",
            BTreeMap::from([("owners".to_owned(), json!(["owner-id"]))]),
            TrustedPluginCapabilities::default()
                .with_authority_provider(TrustedAuthorityProvider::Administration),
        )
        .await
        .unwrap();
        if delegated {
            store
                .commit(
                    "dev.bkm.admin/test",
                    "delegate",
                    &[StateOp::Put {
                        key: "admins/61646d696e2d6964".to_owned(),
                        value: b"admin-id".to_vec(),
                        expected_revision: None,
                    }],
                    &[],
                    CommitOptions::new(1024 * 1024),
                )
                .unwrap();
        }
        host.register_trusted(wasm.clone(), "dev.bkm.wasm-ping/test", BTreeMap::new())
            .await
            .unwrap();
        let mut event = ping_event();
        event.adapter = AdapterId::new("reusable-adapter-id");
        let Event::Message(message) = &mut event.event else {
            unreachable!()
        };
        message.sender.id = sender.to_owned();
        message.text = "/caller-authority".to_owned();
        message.segments = vec![MessageSegment::Text {
            text: message.text.clone(),
        }];
        let Action::Reply(reply) = run_plugin(host, event, platform).await else {
            panic!("expected authority reply")
        };
        assert_eq!(
            reply.content,
            format!("CallerRole::{expected}:{platform}:None")
        );
        // The fixture rejects any privileged role retained by the next lifecycle call.
        wasm.health().await.unwrap();
    }
}
