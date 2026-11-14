//! Offline end-to-end observation: Runtime -> real Ping plugin -> mock Adapter -> HTTP.
use crate::{
    logging::live::{LogBuffer, LogCategory, LogSelection},
    plugin_management::PluginManager,
};
use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use bot_core::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Event, EventEnvelope,
    EventId, EventSender, MessageTarget, RuntimeBuilder, Sender, ShutdownSignal,
};
use plugin_host::{PluginStore, StaticPluginHost};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, Notify};
use tower::ServiceExt as _;
use tracing_subscriber::layer::SubscriberExt as _;

#[derive(Debug)]
struct MockAdapter {
    event: Mutex<Option<EventEnvelope>>,
    id: AdapterId,
    fail: bool,
    gate: Option<Arc<Notify>>,
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
        events: EventSender,
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
        assert!(matches!(action, Action::Reply(ref reply) if reply.content == "pong"));
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        if self.fail {
            Err(AdapterError::Action("mock failure".to_owned()))
        } else {
            Ok(ActionResult {
                message_id: Some("sent".to_owned()),
                raw: Value::Null,
            })
        }
    }
}

#[tokio::test]
async fn received_command_plugin_instance_and_reply_outcome_share_a_source_event() {
    for fail in [false, true] {
        let logs = LogBuffer::default();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.layer()));
        let store = PluginStore::in_memory().unwrap();
        let mut host = StaticPluginHost::new(store.clone());
        host.register_trusted(
            Arc::new(builtin_plugins::PingPlugin::default()),
            "ping-a",
            BTreeMap::new(),
        )
        .await
        .unwrap();
        let manager = PluginManager::new(
            host.clone(),
            store,
            BTreeMap::default(),
            BTreeSet::default(),
        );
        let auth = super::auth::AuthState::from_config(&crate::config::ManagementConfig {
            auth: crate::config::ManagementAuth::None,
            ..Default::default()
        })
        .unwrap();
        let app = super::plugins::router_with_logs(manager, auth, logs.clone());
        let capture = app
            .clone()
            .oneshot(
                Request::get("/api/logs/stream?category=all&include_content=true")
                    .header("host", "localhost:9090")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let adapter = Arc::new(MockAdapter {
            id: AdapterId::new("mock"),
            fail,
            gate: None,
            event: Mutex::new(Some(source_event())),
        });
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(host))
            .build()
            .unwrap();
        let (shutdown, signal) = bot_core::shutdown_channel();
        let mut updates = logs.subscribe();
        let running = tokio::spawn(runtime.run(signal));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                updates.borrow_and_update();
                let page = logs.filtered_snapshot(
                    None,
                    1000,
                    LogSelection {
                        category: LogCategory::All,
                        ..Default::default()
                    },
                );
                if page.entries.iter().any(|entry| {
                    entry
                        .fields
                        .get("record_kind")
                        .is_some_and(|kind| kind == "plugin.action.completed")
                }) {
                    break;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        shutdown.shutdown();
        running.await.unwrap().unwrap();
        let response = app
            .oneshot(
                Request::get("/api/logs?category=all&include_content=true")
                    .header("host", "localhost:9090")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let data: Value = serde_json::from_slice(&bytes).unwrap();
        assert_flow(&data, &bytes, fail);
        drop(capture);
    }
}

fn assert_flow(data: &Value, bytes: &[u8], fail: bool) {
    let entries = data["entries"].as_array().unwrap();
    let incoming = entries
        .iter()
        .find(|entry| entry["fields"]["record_kind"] == "message.received")
        .unwrap();
    assert_eq!(incoming["content"], "/ping");
    assert_eq!(incoming["fields"]["event_id"], "flow-event");
    assert!(incoming["instance_id"].is_null());
    let outgoing = entries
        .iter()
        .find(|entry| {
            entry["fields"]["record_kind"]
                == if fail {
                    "message.failed"
                } else {
                    "message.sent"
                }
        })
        .unwrap();
    assert!(outgoing.get("content").is_none());
    let sending = entries
        .iter()
        .find(|entry| entry["fields"]["record_kind"] == "message.sending")
        .unwrap();
    assert_eq!(sending["content"], "pong");
    assert_eq!(
        sending["fields"]["event_id"],
        outgoing["fields"]["event_id"]
    );
    assert_eq!(
        sending["fields"]["command_id"],
        outgoing["fields"]["command_id"]
    );
    assert_eq!(outgoing["instance_id"], "ping-a");
    assert_eq!(outgoing["fields"]["plugin_id"], "dev.bkm.ping");
    assert_eq!(outgoing["fields"]["event_id"], "flow-event");
    assert_eq!(outgoing["fields"]["command_name"], "ping");
    let action = entries
        .iter()
        .find(|entry| entry["fields"]["record_kind"] == "plugin.action.completed")
        .unwrap();
    assert_eq!(
        action["fields"]["invocation_id"],
        outgoing["fields"]["invocation_id"]
    );
    assert!(!action["fields"]["command_id"].as_str().unwrap().is_empty());
    assert!(
        !String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("raw-payload")
    );
    assert!(
        !String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("private-user")
    );
}

#[tokio::test]
async fn in_flight_sends_never_expose_old_body_to_new_capture_subscribers() {
    for initially_capturing in [false, true] {
        let logs = LogBuffer::default();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(logs.layer()));
        let old = initially_capturing.then(|| logs.capture_content());
        let gate = Arc::new(Notify::new());
        let adapter = MockAdapter {
            event: Mutex::new(None),
            id: AdapterId::new("mock"),
            fail: false,
            gate: Some(gate.clone()),
        };
        let sending = bot_core::execute_traced_action(
            &adapter,
            "pending-event",
            Action::Reply(bot_core::ReplyAction {
                target: MessageTarget::Private {
                    user_id: "private-user".to_owned(),
                },
                source_message_id: "source-message".to_owned(),
                content: "pong".to_owned(),
            }),
        );
        tokio::pin!(sending);
        assert!(futures_util::poll!(&mut sending).is_pending());
        drop(old);
        let _new = logs.capture_content();
        gate.notify_one();
        sending.await.unwrap();
        let page = logs.filtered_snapshot(
            None,
            1000,
            LogSelection {
                category: LogCategory::Message,
                include_content: true,
                ..Default::default()
            },
        );
        assert_eq!(page.entries.len(), 2);
        assert!(page.entries.iter().all(|entry| entry.content.is_none()));
        assert_eq!(page.entries[1].fields["record_kind"], "message.sent");
    }
}

fn source_event() -> EventEnvelope {
    EventEnvelope {
        id: EventId::new("flow-event"),
        adapter: AdapterId::new("mock"),
        delivery_id: None,
        timestamp: None,
        event: Event::Message(CommonMessage {
            message_id: "source-message".to_owned(),
            sender: Sender {
                id: "private-user".to_owned(),
                display_name: None,
            },
            target: MessageTarget::Private {
                user_id: "private-user".to_owned(),
            },
            text: "/ping".to_owned(),
            segments: Vec::new(),
            reply_to: None,
        }),
        raw: json!({"private":"raw-payload"}),
    }
}
