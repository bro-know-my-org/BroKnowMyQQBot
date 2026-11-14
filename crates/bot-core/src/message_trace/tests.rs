use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tracing::{
    Instrument, Subscriber,
    field::{Field, Visit},
    span::Attributes,
};
use tracing_subscriber::{
    Layer,
    layer::{Context as LayerContext, SubscriberExt},
    registry::LookupSpan,
};

use crate::{
    Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Context, Event,
    EventEnvelope, EventHandler, EventSender, HandlerError, MediaAttachment, MessageSegment,
    MessageTarget, ReplyMediaAction, RuntimeBuilder, SendMediaAction, SendMessageAction, Sender,
    ShutdownHandle, ShutdownSignal, shutdown_channel,
};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<VecDeque<Fields>>>);

#[derive(Clone, Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().to_owned(), value.chars().take(4096).collect());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &tracing::span::Id,
        ctx: LayerContext<'_, S>,
    ) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        ctx.span(id).unwrap().extensions_mut().insert(fields);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "bkm::message_trace" {
            return;
        }
        let mut fields = Fields::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(parent) = span.extensions().get::<Fields>() {
                    fields.0.extend(parent.0.clone());
                }
            }
        }
        event.record(&mut fields);
        let mut records = self.0.lock().unwrap();
        if records.len() == 16 {
            records.pop_front();
        }
        records.push_back(fields);
    }
}

struct MockAdapter {
    id: AdapterId,
    fail: bool,
    shutdown: ShutdownHandle,
    acknowledgements: AtomicUsize,
    executions: AtomicUsize,
    last_content_len: AtomicUsize,
}

fn envelope(adapter: &AdapterId) -> EventEnvelope {
    EventEnvelope {
        id: "stable-event".into(),
        adapter: adapter.clone(),
        delivery_id: None,
        timestamp: None,
        event: Event::Message(CommonMessage {
            message_id: "private-message-id".to_owned(),
            target: MessageTarget::Group {
                group_id: "private-group-id".to_owned(),
            },
            sender: Sender {
                id: "private-user-id".to_owned(),
                display_name: Some("private-name".to_owned()),
            },
            text: "/ping\nbody".to_owned(),
            segments: vec![
                MessageSegment::Text {
                    text: "/ping\nbody".to_owned(),
                },
                MessageSegment::Platform {
                    kind: "mention".to_owned(),
                    data: json!({"user_id": "private-segment-id"}),
                },
            ],
            reply_to: None,
        }),
        raw: json!({"token": "private-token", "payload": "private-raw"}),
    }
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
        for _ in 0..2 {
            events
                .send(envelope(&self.id))
                .await
                .map_err(|_| AdapterError::EventQueueClosed)?;
        }
        shutdown.cancelled().await;
        Ok(())
    }

    async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        if let Action::SendMessage(message) = &action {
            self.last_content_len
                .store(message.content.len(), Ordering::SeqCst);
        }
        if self.fail {
            Err(AdapterError::Action("private-error-payload".to_owned()))
        } else {
            Ok(ActionResult {
                message_id: Some("private-result-id".to_owned()),
                raw: json!({"secret": "private-result"}),
            })
        }
    }

    async fn event_handled(&self, _event: &EventEnvelope) -> Result<(), AdapterError> {
        if self.acknowledgements.fetch_add(1, Ordering::SeqCst) == 1 {
            self.shutdown.shutdown();
        }
        Ok(())
    }
}

struct ReplyHandler {
    capture: Capture,
    fail: bool,
}

#[async_trait]
impl EventHandler for ReplyHandler {
    fn name(&self) -> &'static str {
        "reply"
    }

    async fn handle(&self, context: Context, _event: &Event) -> Result<(), HandlerError> {
        // The single inbound observation must precede the entire handler chain.
        assert_eq!(self.capture.0.lock().unwrap().len(), 1);
        let span = tracing::info_span!(
            "plugin_action",
            plugin_id = "dev.bkm.ping",
            instance_id = "ping/default",
            invocation_id = "invocation-1"
        );
        let result = context.reply("pong\nreply").instrument(span).await;
        assert_eq!(result.is_err(), self.fail);
        Ok(())
    }
}

struct SecondHandler;

#[async_trait]
impl EventHandler for SecondHandler {
    fn name(&self) -> &'static str {
        "second"
    }
    async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_to_reply_observes_body_identity_and_outcome_once() {
    for adapter_id in ["qq-mock", "onebot-mock"] {
        for fail in [false, true] {
            let capture = Capture::default();
            let subscriber = tracing_subscriber::registry().with(capture.clone());
            let _guard = tracing::subscriber::set_default(subscriber);
            let (shutdown, signal) = shutdown_channel();
            let adapter = Arc::new(MockAdapter {
                id: adapter_id.into(),
                fail,
                shutdown,
                acknowledgements: AtomicUsize::new(0),
                executions: AtomicUsize::new(0),
                last_content_len: AtomicUsize::new(0),
            });
            let runtime = RuntimeBuilder::new()
                .adapter(adapter.clone())
                .handler(Arc::new(ReplyHandler {
                    capture: capture.clone(),
                    fail,
                }))
                .handler(Arc::new(SecondHandler))
                .build()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), runtime.run(signal))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(adapter.executions.load(Ordering::SeqCst), 1);
            let records = capture.0.lock().unwrap();
            assert_eq!(records.len(), 3);
            let incoming = &records[0].0;
            let sending = &records[1].0;
            let outgoing = &records[2].0;
            assert_eq!(incoming["record_kind"], "message.received");
            assert_eq!(incoming["direction"], "inbound");
            assert_eq!(incoming["status"], "received");
            assert_eq!(incoming["content"], "/ping\nbody");
            assert!(!incoming.contains_key("instance_id"));
            assert_eq!(
                outgoing["record_kind"],
                if fail {
                    "message.failed"
                } else {
                    "message.sent"
                }
            );
            assert_eq!(
                outgoing["status"],
                if fail { "failed" } else { "succeeded" }
            );
            assert_eq!(outgoing["direction"], "outbound");
            assert_eq!(outgoing["action_type"], "message.reply");
            assert_eq!(sending["record_kind"], "message.sending");
            assert_eq!(sending["content"], "pong\nreply");
            assert!(!outgoing.contains_key("content"));
            assert_eq!(outgoing["plugin_id"], "dev.bkm.ping");
            assert_eq!(outgoing["instance_id"], "ping/default");
            assert_eq!(outgoing["invocation_id"], "invocation-1");
            for record in &*records {
                assert_eq!(record.0["message_scope"], "group");
                assert_eq!(record.0["adapter_id"], adapter_id);
                assert_eq!(record.0["event_id"], "stable-event");
                assert!(!format!("{:?}", record.0).contains("private-"));
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shared_action_executor_records_sends_and_media_captions_without_payloads() {
    let capture = Capture::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (shutdown, _) = shutdown_channel();
    let adapter = MockAdapter {
        id: "mock".into(),
        fail: false,
        shutdown,
        acknowledgements: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        last_content_len: AtomicUsize::new(0),
    };
    let target = MessageTarget::Private {
        user_id: "private-target".to_owned(),
    };
    let attachment = MediaAttachment::image(
        "image/png",
        Some("private-filename".to_owned()),
        b"\x89PNG\r\n\x1a\nprivate-image".to_vec(),
    )
    .unwrap();
    let actions = [
        (
            Action::SendMessage(SendMessageAction {
                target: target.clone(),
                content: "send body".to_owned(),
            }),
            "message.send",
            "send body",
        ),
        (
            Action::ReplyMedia(ReplyMediaAction {
                target: target.clone(),
                source_message_id: "private-source".to_owned(),
                attachment: attachment.clone(),
                caption: Some("caption".to_owned()),
            }),
            "media.reply",
            "caption",
        ),
        (
            Action::SendMedia(SendMediaAction {
                target: target.clone(),
                attachment,
                caption: None,
            }),
            "media.send",
            "",
        ),
    ];
    for (action, kind, content) in actions {
        super::execute_traced_action(&adapter, "schedule:stable", action)
            .await
            .unwrap();
        let records = capture.0.lock().unwrap();
        let fields = &records[records.len() - 2].0;
        assert_eq!(fields["action_type"], kind);
        assert_eq!(fields["message_scope"], "private");
        assert_eq!(records.back().unwrap().0["message_scope"], "private");
        assert_eq!(fields["content"], content);
        assert_eq!(fields["event_id"], "schedule:stable");
        assert!(!format!("{fields:?}").contains("private-"));
    }
    let count = capture.0.lock().unwrap().len();
    for action in [
        Action::Recall {
            target,
            message_id: "private-recall".to_owned(),
            hide_tip: false,
        },
        Action::Platform {
            name: "send_unknown".to_owned(),
            payload: json!({"content": "private-payload"}),
        },
    ] {
        super::execute_traced_action(&adapter, "stable", action)
            .await
            .unwrap();
    }
    assert_eq!(capture.0.lock().unwrap().len(), count);
    assert_eq!(adapter.executions.load(Ordering::SeqCst), 5);
}

#[tokio::test(flavor = "current_thread")]
async fn scope_is_portable_target_metadata_without_platform_identity() {
    let capture = Capture::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (shutdown, _) = shutdown_channel();
    let mut adapter = MockAdapter {
        id: "mock".into(),
        fail: false,
        shutdown,
        acknowledgements: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        last_content_len: AtomicUsize::new(0),
    };
    for (target, expected) in [
        (
            MessageTarget::Private {
                user_id: "private-user".into(),
            },
            "private",
        ),
        (
            MessageTarget::GuildDirect {
                guild_id: "private-guild".into(),
            },
            "private",
        ),
        (
            MessageTarget::Group {
                group_id: "private-group".into(),
            },
            "group",
        ),
        (
            MessageTarget::Channel {
                channel_id: "private-channel".into(),
            },
            "channel",
        ),
    ] {
        let mut event = envelope(&adapter.id);
        if let Event::Message(message) = &mut event.event {
            message.target = target.clone();
        }
        for fail in [false, true] {
            adapter.fail = fail;
            super::received(&event);
            let result = super::execute_traced_action(
                &adapter,
                "stable-event",
                Action::SendMessage(SendMessageAction {
                    target: target.clone(),
                    content: "body".into(),
                }),
            )
            .await;
            assert_eq!(result.is_err(), fail);
            let mut records = capture.0.lock().unwrap();
            assert_eq!(records.len(), 3);
            for record in records.drain(..) {
                assert_eq!(record.0["message_scope"], expected);
                assert!(!format!("{:?}", record.0).contains("private-"));
            }
        }
    }
}

#[tokio::test]
async fn long_message_is_sent_intact_without_retaining_a_body_in_the_result() {
    let capture = Capture::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (shutdown, _) = shutdown_channel();
    let adapter = MockAdapter {
        id: "mock".into(),
        fail: false,
        shutdown,
        acknowledgements: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        last_content_len: AtomicUsize::new(0),
    };
    let content = "界".repeat(20_000);
    let original_len = content.len();
    super::execute_traced_action(
        &adapter,
        "large-event",
        Action::SendMessage(SendMessageAction {
            target: MessageTarget::Private {
                user_id: "private-target".to_owned(),
            },
            content,
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        adapter.last_content_len.load(Ordering::SeqCst),
        original_len
    );
    let records = capture.0.lock().unwrap();
    assert_eq!(records.front().unwrap().0["record_kind"], "message.sending");
    assert!(!records.back().unwrap().0.contains_key("content"));
}
