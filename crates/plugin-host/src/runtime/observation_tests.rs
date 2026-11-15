use super::*;
use bot_core::{
    ActionResult, AdapterId, CommonMessage, EventEnvelope, EventId, EventSender, RuntimeBuilder,
    Sender, ShutdownHandle, ShutdownSignal, shutdown_channel,
};
use builtin_plugins::PingPlugin;
use plugin_fixtures::PanicFixturePlugin;
use serde_json::json;
use tracing::{
    Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{
    Layer,
    layer::{Context as LayerContext, SubscriberExt},
    registry::LookupSpan,
};

#[derive(Clone, Default, Debug)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<StdMutex<Vec<Fields>>>);

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        ctx.span(id).unwrap().extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: LayerContext<'_, S>) {
        let span = ctx.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        values.record(extensions.get_mut::<Fields>().unwrap());
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: LayerContext<'_, S>) {
        let mut fields = Fields::default();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(parent) = span.extensions().get::<Fields>() {
                    fields.0.extend(parent.0.clone());
                }
            }
        }
        event.record(&mut fields);
        if event.metadata().target() == "bkm::message_trace" {
            fields.0.remove("content");
        }
        fields
            .0
            .insert("level".to_owned(), event.metadata().level().to_string());
        fields
            .0
            .insert("target".to_owned(), event.metadata().target().to_owned());
        self.0.lock().unwrap().push(fields);
    }
}

impl Capture {
    fn records(&self, kind: &str) -> Vec<Fields> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|fields| {
                fields
                    .0
                    .get("record_kind")
                    .is_some_and(|value| value == kind)
            })
            .cloned()
            .collect()
    }
}

struct TraceAdapter {
    shutdown: ShutdownHandle,
    events: Mutex<Vec<EventEnvelope>>,
    replies: StdMutex<Vec<String>>,
    fail: bool,
}

#[async_trait]
impl Adapter for TraceAdapter {
    fn id(&self) -> &AdapterId {
        static ID: std::sync::OnceLock<AdapterId> = std::sync::OnceLock::new();
        ID.get_or_init(|| AdapterId::new("trace-mock"))
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
        tokio::task::yield_now().await;
        if self.fail {
            return Err(AdapterError::Action("PRIVATE_FAILURE_PAYLOAD".to_owned()));
        }
        if let Action::Reply(reply) = action {
            assert_eq!(
                reply.target,
                MessageTarget::Group {
                    group_id: "group".to_owned()
                }
            );
            assert_eq!(reply.source_message_id, "source");
            self.replies.lock().unwrap().push(reply.content);
        }
        Ok(ActionResult {
            message_id: Some("reply-id".to_owned()),
            raw: Value::Null,
        })
    }

    async fn event_handled(&self, _: &EventEnvelope) -> Result<(), AdapterError> {
        self.shutdown.shutdown();
        Ok(())
    }
}

fn message(text: &str) -> EventEnvelope {
    EventEnvelope {
        id: EventId::new("trace-event"),
        adapter: AdapterId::new("trace-mock"),
        delivery_id: None,
        timestamp: None,
        event: Event::Message(CommonMessage {
            message_id: "source".to_owned(),
            target: MessageTarget::Group {
                group_id: "group".to_owned(),
            },
            sender: Sender {
                id: "sender".to_owned(),
                display_name: None,
            },
            text: text.to_owned(),
            segments: vec![],
            reply_to: None,
        }),
        raw: json!({"secret": "PRIVATE_RAW_PAYLOAD"}),
    }
}

async fn dispatch(
    plugin: Arc<dyn StaticPlugin>,
    instance: &str,
    text: &str,
    fail: bool,
) -> (Capture, PluginStore) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let store = PluginStore::in_memory().unwrap();
    let mut host = StaticPluginHost::new(store.clone());
    host.register_trusted(plugin, instance, BTreeMap::new())
        .await
        .unwrap();
    let (shutdown, signal) = shutdown_channel();
    let adapter = Arc::new(TraceAdapter {
        shutdown,
        events: Mutex::new(vec![message(text)]),
        replies: StdMutex::new(vec![]),
        fail,
    });
    let runtime = RuntimeBuilder::new()
        .adapter(adapter.clone())
        .handler(Arc::new(host))
        .build()
        .unwrap();
    timeout(Duration::from_secs(5), runtime.run(signal))
        .await
        .unwrap()
        .unwrap();
    if text == "/ping" && !fail && instance.starts_with("dev.bkm.ping/") {
        assert_eq!(*adapter.replies.lock().unwrap(), ["pong"]);
    }
    (capture, store)
}

#[tokio::test]
async fn ping_reply_inherits_declared_command_and_each_instance_identity() {
    for instance in ["dev.bkm.ping/first", "dev.bkm.ping/second"] {
        let (capture, _) =
            dispatch(Arc::new(PingPlugin::default()), instance, "/ping", false).await;
        let completed = capture.records("plugin.completed");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0["command_name"], "ping");
        assert_eq!(completed[0].0["level"], "INFO");
        assert_eq!(completed[0].0["command_count"], "1");
        assert_eq!(completed[0].0["status"], "output_committed");
        let invocation = &completed[0].0["invocation_id"];
        for kind in ["plugin.invoked", "plugin.action.completed", "message.sent"] {
            let records = capture.records(kind);
            assert_eq!(records.len(), 1, "{kind}");
            assert_eq!(records[0].0["instance_id"], instance);
            assert_eq!(records[0].0["plugin_id"], "dev.bkm.ping");
            assert_eq!(records[0].0["event_id"], "trace-event");
            assert_eq!(&records[0].0["invocation_id"], invocation);
        }
        let action = capture.records("plugin.action.completed");
        assert_eq!(action[0].0["status"], "succeeded");
        assert_eq!(action[0].0["action_type"], "message.reply");
        assert_ne!(action[0].0["command_id"], "");
        assert_eq!(capture.records("plugin.invoked")[0].0["level"], "DEBUG");
        assert!(
            !capture.records("message.received")[0]
                .0
                .contains_key("instance_id")
        );
    }
}

#[tokio::test]
async fn unrelated_command_broadcast_has_only_debug_no_output_observation() {
    let (capture, _) = dispatch(
        Arc::new(PingPlugin::default()),
        "ping/unrelated",
        "/PRIVATE_ARGUMENT other body",
        false,
    )
    .await;
    let completed = capture.records("plugin.completed");
    assert_eq!(completed.len(), 1);
    let fields = &completed[0].0;
    assert_eq!(fields["level"], "DEBUG");
    assert_eq!(fields["status"], "no_output");
    assert_eq!(fields["command_count"], "0");
    assert_eq!(fields["state_op_count"], "0");
    assert!(!fields.contains_key("command_name"));
    assert!(capture.records("plugin.action.completed").is_empty());
    assert!(!format!("{:?}", capture.0.lock().unwrap()).contains("PRIVATE_"));
}

#[tokio::test]
async fn declared_alias_is_labeled_without_claiming_the_plugin_produced_output() {
    struct AliasProbe(PluginManifest);
    #[async_trait]
    impl StaticPlugin for AliasProbe {
        fn manifest(&self) -> &PluginManifest {
            &self.0
        }
        async fn on_event(
            &self,
            _: &PluginEventEnvelope,
            _: &dyn HostQueries,
        ) -> Result<HandlerOutput, PluginError> {
            Ok(HandlerOutput::default())
        }
    }
    let mut manifest = PingPlugin::default().manifest().clone();
    manifest.commands[0].aliases = vec!["p".to_owned()];
    let (capture, _) = dispatch(Arc::new(AliasProbe(manifest)), "ping-alias", "/p", false).await;
    let records = capture.records("plugin.completed");
    assert_eq!(records[0].0["command_name"], "p");
    assert_eq!(records[0].0["status"], "no_output");
    assert_eq!(records[0].0["command_count"], "0");
    assert_eq!(records[0].0["level"], "INFO");
}

#[tokio::test]
async fn completion_reply_uses_derived_event_identity_and_original_reply_target() {
    struct CompletionReply(PluginManifest);
    #[async_trait]
    impl StaticPlugin for CompletionReply {
        fn manifest(&self) -> &PluginManifest {
            &self.0
        }
        async fn on_event(
            &self,
            event: &PluginEventEnvelope,
            _: &dyn HostQueries,
        ) -> Result<HandlerOutput, PluginError> {
            let command_id = if event.event_type == "message.created" {
                "first"
            } else if event.payload["command_id"] == "first" {
                "followup"
            } else {
                return Ok(HandlerOutput::default());
            };
            Ok(HandlerOutput {
                disposition: Disposition::Continue,
                commands: vec![PluginCommand {
                    command_id: command_id.to_owned(),
                    kind: "message.reply".to_owned(),
                    idempotency_key: None,
                    deadline_ms: None,
                    payload: json!({"content": "PRIVATE_FOLLOWUP_BODY"}),
                }],
                ..HandlerOutput::default()
            })
        }
    }
    let mut manifest = PingPlugin::default().manifest().clone();
    manifest.subscriptions.push(plugin_api::Subscription {
        id: "completions".to_owned(),
        event: "action.completed".to_owned(),
        priority: 0,
        scopes: BTreeSet::new(),
    });
    let (capture, _) = dispatch(
        Arc::new(CompletionReply(manifest)),
        "ping/completion",
        "/ping",
        false,
    )
    .await;
    let actions = capture.records("plugin.action.completed");
    assert_eq!(actions.len(), 2);
    let derived_event = format!(
        "action/{}/{}",
        actions[0].0["invocation_id"], actions[0].0["command_id"]
    );
    assert_eq!(actions[1].0["event_id"], derived_event);
    assert_ne!(actions[0].0["invocation_id"], actions[1].0["invocation_id"]);
    for kind in [
        "plugin.invoked",
        "plugin.completed",
        "message.sending",
        "message.sent",
    ] {
        let records = capture.records(kind);
        let followup = records
            .iter()
            .find(|record| record.0["event_id"] == derived_event)
            .unwrap_or_else(|| panic!("missing completion reply record {kind}"));
        assert_eq!(followup.0["invocation_id"], actions[1].0["invocation_id"]);
        assert_eq!(followup.0["instance_id"], "ping/completion");
        if kind.starts_with("message.") {
            assert_eq!(records.len(), 2);
            assert_eq!(followup.0["command_id"], "followup");
        }
    }
    assert!(!format!("{:?}", capture.0.lock().unwrap()).contains("PRIVATE_"));
}

#[tokio::test]
async fn invocation_and_action_failures_keep_safe_correlation_and_persisted_status() {
    let (capture, _) = dispatch(
        Arc::new(PanicFixturePlugin::default()),
        "panic/second",
        "/ping",
        false,
    )
    .await;
    let failed = capture.records("plugin.failed");
    assert_eq!(failed.len(), 3);
    assert_eq!(failed[0].0["level"], "WARN");
    assert_eq!(failed[0].0["error_class"], "guest_trap");
    assert_eq!(failed[0].0["instance_id"], "panic/second");
    assert_eq!(failed[0].0["event_id"], "trace-event");
    assert!(capture.records("plugin.completed").is_empty());
    let (capture, store) = dispatch(
        Arc::new(PingPlugin::default()),
        "ping/failure",
        "/ping",
        true,
    )
    .await;
    let action = capture.records("plugin.action.completed");
    assert_eq!(action.len(), 1);
    assert_eq!(action[0].0["status"], "failed");
    assert_eq!(action[0].0["level"], "WARN");
    assert_eq!(
        capture.records("message.failed")[0].0["instance_id"],
        "ping/failure"
    );
    assert!(store.pending_commands("ping/failure").unwrap().is_empty());
    assert_eq!(
        store
            .command_status(
                "ping/failure",
                &action[0].0["invocation_id"],
                &action[0].0["command_id"]
            )
            .unwrap()
            .as_deref(),
        Some("failed")
    );
    assert!(!format!("{:?}", capture.0.lock().unwrap()).contains("PRIVATE_"));
}

#[tokio::test]
async fn scheduled_and_recovered_direct_sends_keep_source_event_and_action_identity() {
    let capture = Capture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (shutdown, _) = shutdown_channel();
    let adapter = Arc::new(TraceAdapter {
        shutdown,
        events: Mutex::new(vec![]),
        replies: StdMutex::new(vec![]),
        fail: false,
    });
    let store = PluginStore::in_memory().unwrap();
    let mut host = StaticPluginHost::new(store.clone()).with_adapter(adapter);
    host.register_trusted(
        Arc::new(PingPlugin::default()),
        "ping/background",
        BTreeMap::new(),
    )
    .await
    .unwrap();
    let recovered = OutboxOrigin {
        source_event_id: "background-event".to_owned(),
        adapter_id: "trace-mock".to_owned(),
        platform: Some("mock".to_owned()),
        reply_target: None,
        source_message_id: None,
    };
    for (invocation, origin) in [
        (
            "scheduled-invocation",
            ExecutionOrigin::Scheduled {
                adapter_id: "trace-mock",
            },
        ),
        (
            "recovered-invocation",
            ExecutionOrigin::Recovered(&recovered),
        ),
    ] {
        let command = PluginCommand {
            command_id: "send".to_owned(),
            kind: "message.send".to_owned(),
            idempotency_key: None,
            deadline_ms: None,
            payload: serde_json::to_value(SendMessageAction {
                target: MessageTarget::Group {
                    group_id: "group".to_owned(),
                },
                content: "PRIVATE_SEND_BODY".to_owned(),
            })
            .unwrap(),
        };
        store
            .commit(
                "ping/background",
                invocation,
                &[],
                std::slice::from_ref(&command),
                CommitOptions::new(1024).with_origin(&recovered),
            )
            .unwrap();
        host.execute_command(
            &host.plugins[0],
            origin,
            "background-event",
            invocation,
            command,
        )
        .await
        .unwrap();
        assert_eq!(
            store
                .command_status("ping/background", invocation, "send")
                .unwrap()
                .as_deref(),
            Some("succeeded")
        );
    }
    let sent = capture.records("message.sent");
    assert_eq!(sent.len(), 2);
    for (record, invocation) in sent
        .iter()
        .zip(["scheduled-invocation", "recovered-invocation"])
    {
        assert_eq!(record.0["event_id"], "background-event");
        assert_eq!(record.0["instance_id"], "ping/background");
        assert_eq!(record.0["invocation_id"], invocation);
        assert_eq!(record.0["command_id"], "send");
        assert_eq!(record.0["action_type"], "message.send");
    }
    assert_eq!(capture.records("plugin.action.completed").len(), 2);
    assert!(!format!("{:?}", capture.0.lock().unwrap()).contains("PRIVATE_"));
}
