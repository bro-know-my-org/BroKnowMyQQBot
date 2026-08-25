//! Built-in plugins distributed with `BroKnowMyQQBot`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use plugin_api::{
    ActionCompleted, ActionStatus, BrowserPermission, BrowserRun, BrowserRunResult,
    BrowserScreenshotFormat, BrowserStep, BrowserViewport, BrowserWaitUntil, CommandDeclaration,
    Disposition, HandlerOutput, HostQueries, HttpPermission, HttpRequest, HttpResponse, MediaReply,
    PluginCommand, PluginError, PluginEventEnvelope, PluginId, PluginManifest, PluginMetadata,
    PluginPermissions, PluginRuntimeConfig, ScheduleCancel, ScheduleCreate, ScheduleTriggered,
    StateOp, StaticPlugin, StoragePermission, Subscription,
};
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

mod basic;
mod business;
mod devtools;
mod probes;

pub use basic::{CounterPlugin, EchoPlugin, HelpPlugin, PingPlugin};
pub use business::{AdminPlugin, ReminderPlugin};
pub use devtools::DevToolsPlugin;
pub use probes::{
    ActionResultProbePlugin, ActiveSendProbePlugin, BrowserProbePlugin, ConfigProbePlugin,
    HttpProbePlugin, QqExtensionProbePlugin, SchedulerProbePlugin,
};

fn message_plugin_manifest(
    id: &str,
    name: &str,
    command: &str,
    action: &str,
    storage: bool,
) -> PluginManifest {
    let mut actions = BTreeSet::new();
    actions.insert(action.to_owned());
    PluginManifest {
        manifest_version: 1,
        id: PluginId::new(id).expect("built-in plugin ID must be valid"),
        metadata: PluginMetadata::single_locale("en", name, format!("Built-in {name} plugin")),
        version: "0.1.0".to_owned(),
        protocol: ">=1.0,<2.0".to_owned(),
        state_version: 1,
        entry: "component.wasm".to_owned(),
        runtime: PluginRuntimeConfig::default(),
        subscriptions: vec![Subscription {
            id: "messages".to_owned(),
            event: "message.created".to_owned(),
            priority: 0,
            scopes: ["group", "private", "channel"]
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        }],
        commands: vec![CommandDeclaration {
            name: command.to_owned(),
            aliases: Vec::new(),
            description: format!("Run /{command}"),
        }],
        permissions: PluginPermissions {
            actions,
            event_extensions: BTreeSet::new(),
            http: Vec::new(),
            browser: Vec::new(),
            storage: if storage {
                StoragePermission::Private
            } else {
                StoragePermission::None
            },
            storage_quota_bytes: if storage { 1024 * 1024 } else { 0 },
            scheduler: false,
            stop_propagation: false,
        },
    }
}

fn command_declaration(name: &str, description: &str) -> CommandDeclaration {
    CommandDeclaration {
        name: name.to_owned(),
        aliases: Vec::new(),
        description: description.to_owned(),
    }
}

fn message_text(event: &PluginEventEnvelope) -> Option<&str> {
    event.payload.pointer("/data/text").and_then(Value::as_str)
}

fn is_command(command: &'static str) -> impl FnOnce(&str) -> bool {
    move |content| content.trim() == command
}

fn command_argument<'a>(content: &'a str, command: &str) -> Option<&'a str> {
    let argument = command_tail(content, command)?;
    (!argument.is_empty()).then_some(argument)
}

fn command_tail<'a>(content: &'a str, command: &str) -> Option<&'a str> {
    let content = content.trim_start();
    let tail = content.strip_prefix(command)?;
    tail.chars()
        .next()
        .is_none_or(char::is_whitespace)
        .then(|| tail.trim())
}

fn ignored() -> HandlerOutput {
    HandlerOutput {
        disposition: Disposition::Ignore,
        ..HandlerOutput::default()
    }
}

fn reply(event: &PluginEventEnvelope, content: &str, state_ops: Vec<StateOp>) -> HandlerOutput {
    HandlerOutput {
        disposition: Disposition::Continue,
        state_ops,
        commands: vec![PluginCommand {
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            idempotency_key: Some(format!("{}/reply", event.event_id)),
            deadline_ms: Some(5_000),
            payload: json!({"content":content}),
        }],
        diagnostics: Vec::new(),
    }
}

fn plugin_command(
    event: &PluginEventEnvelope,
    command_id: &str,
    kind: &str,
    payload: Value,
) -> PluginCommand {
    PluginCommand {
        command_id: command_id.to_owned(),
        kind: kind.to_owned(),
        idempotency_key: Some(format!("{}/{command_id}", event.event_id)),
        deadline_ms: Some(10_000),
        payload,
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

pub fn default_plugin_configs() -> BTreeMap<String, BTreeMap<String, Value>> {
    BTreeMap::from([
        ("dev.bkm.ping".to_owned(), BTreeMap::new()),
        ("dev.bkm.counter".to_owned(), BTreeMap::new()),
        ("dev.bkm.help".to_owned(), BTreeMap::new()),
        ("dev.bkm.echo".to_owned(), BTreeMap::new()),
        ("dev.bkm.reminder".to_owned(), BTreeMap::new()),
    ])
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use plugin_api::{
        ActionCompleted, ActionStatus, HostQueries, PluginEventEnvelope, StateOp, StateValue,
        StaticPlugin,
    };
    use serde_json::{Value, json};

    use super::{
        ActionResultProbePlugin, ActiveSendProbePlugin, AdminPlugin, ConfigProbePlugin,
        CounterPlugin, EchoPlugin, HelpPlugin, HttpProbePlugin, PingPlugin, ReminderPlugin,
        SchedulerProbePlugin,
    };

    struct Queries {
        state: BTreeMap<String, StateValue>,
        capabilities: BTreeSet<String>,
    }

    impl HostQueries for Queries {
        fn config_get(&self, _key: &str) -> Option<&Value> {
            None
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
            &self.capabilities
        }

        fn invocation_time_ms(&self) -> i64 {
            0
        }
    }

    fn event(text: &str) -> PluginEventEnvelope {
        PluginEventEnvelope {
            protocol_version: "1.0.0".to_owned(),
            event_id: "event-1".to_owned(),
            delivery_id: "delivery-1".to_owned(),
            invocation_id: "invocation-1".to_owned(),
            occurred_at_ms: None,
            received_at_ms: 0,
            adapter_id: "test".to_owned(),
            event_type: "message.created".to_owned(),
            trace_id: None,
            payload: json!({
                "type":"message",
                "data":{
                    "text":text,
                    "sender":{"id":"owner-id"},
                    "target":{"scope":"group","group_id":"group-id"}
                }
            }),
            extensions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn ping_returns_reply_command() {
        let output = PingPlugin::default()
            .on_event(
                &event("/ping"),
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].payload["content"], "pong");
    }

    #[tokio::test]
    async fn help_lists_the_supplied_manifest_commands() {
        let plugin = HelpPlugin::with_commands([
            super::command_declaration("ping", "Ping command"),
            super::command_declaration("http-probe", "HTTP command"),
        ]);
        let output = plugin
            .on_event(
                &event("/help"),
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let content = output.commands[0].payload["content"].as_str().unwrap();
        assert!(content.contains("/help"));
        assert!(content.contains("/http-probe"));
        assert!(content.contains("/ping"));
        assert!(content.find("/help").unwrap() < content.find("/ping").unwrap());
    }

    #[tokio::test]
    async fn counter_uses_revision_for_state_update() {
        let output = CounterPlugin::default()
            .on_event(
                &event("/count"),
                &Queries {
                    state: BTreeMap::from([(
                        "counter".to_owned(),
                        StateValue {
                            value: b"4".to_vec(),
                            revision: 7,
                        },
                    )]),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].payload["content"], "count: 5");
        assert_eq!(
            serde_json::to_value(&output.state_ops[0]).unwrap()["expected_revision"],
            7
        );
    }

    #[tokio::test]
    async fn echo_uses_passive_reply() {
        let output = EchoPlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    payload: json!({
                        "type":"message",
                        "data":{
                            "text":"/echo hello world",
                            "target":{"scope":"group","group_id":"group-id"}
                        }
                    }),
                    ..event("ignored")
                },
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "message.reply");
        assert_eq!(output.commands[0].payload["content"], "hello world");
    }

    #[tokio::test]
    async fn active_send_probe_confirms_then_uses_explicit_target() {
        let output = ActiveSendProbePlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    payload: json!({
                        "type":"message",
                        "data":{
                            "text":"/active-send hello world",
                            "target":{"scope":"group","group_id":"group-id"}
                        }
                    }),
                    ..event("ignored")
                },
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "message.reply");
        assert_eq!(output.commands[1].kind, "message.send");
        assert_eq!(output.commands[1].payload["content"], "hello world");
        assert_eq!(output.commands[1].payload["target"]["group_id"], "group-id");
    }

    #[tokio::test]
    async fn action_result_probe_persists_completion() {
        let completion = ActionCompleted {
            source_event_id: "source-event".to_owned(),
            source_invocation_id: "source-invocation".to_owned(),
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            status: ActionStatus::Succeeded,
            retryable: false,
            result: Some(json!({"message_id":"sent"})),
            error_code: None,
            error_message: None,
        };
        let output = ActionResultProbePlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    event_type: "action.completed".to_owned(),
                    payload: serde_json::to_value(completion).unwrap(),
                    ..event("ignored")
                },
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.state_ops.len(), 1);
        assert!(output.commands.is_empty());
    }

    #[tokio::test]
    async fn http_probe_emits_only_host_http_command() {
        let output = HttpProbePlugin::default()
            .on_event(
                &event("/http-probe"),
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "http.request");
        assert_eq!(output.commands[0].payload["method"], "GET");
        assert_eq!(output.commands[0].payload["url"], "https://example.com/");
    }

    #[tokio::test]
    async fn scheduler_probe_emits_persistent_schedule_command() {
        let output = SchedulerProbePlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    payload: json!({
                        "type":"message",
                        "data":{
                            "text":"/schedule 30",
                            "target":{"scope":"group","group_id":"group-id"},
                            "sender":{"id":"user-id"}
                        }
                    }),
                    ..event("ignored")
                },
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "schedule.create");
        assert_eq!(output.commands[0].payload["run_at_ms"], 30_000);
        assert_eq!(
            output.commands[0].payload["payload"]["target"]["group_id"],
            "group-id"
        );
        assert_eq!(output.commands.len(), 1);
        assert_eq!(output.state_ops.len(), 1);

        let cancel = SchedulerProbePlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    payload: json!({
                        "type":"message",
                        "data":{
                            "text":"/schedule-cancel probe-6576656e742d31",
                            "target":{"scope":"group","group_id":"group-id"},
                            "sender":{"id":"user-id"}
                        }
                    }),
                    ..event("ignored")
                },
                &Queries {
                    state: BTreeMap::from([(
                        "tasks/probe-6576656e742d31".to_owned(),
                        StateValue {
                            value: serde_json::to_vec(&json!({
                                "sender":"user-id",
                                "target":{"scope":"group","group_id":"group-id"}
                            }))
                            .unwrap(),
                            revision: 1,
                        },
                    )]),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(cancel.commands[0].kind, "schedule.cancel");
        assert_eq!(cancel.commands.len(), 1);

        for (cancelled, expected) in [
            (true, "cancelled task task-1"),
            (false, "task not found or no longer pending: task-1"),
        ] {
            let completion = ActionCompleted {
                source_event_id: "source".to_owned(),
                source_invocation_id: "invocation".to_owned(),
                command_id: "cancel".to_owned(),
                kind: "schedule.cancel".to_owned(),
                status: ActionStatus::Succeeded,
                retryable: false,
                result: Some(json!({"cancelled":cancelled,"task_id":"task-1"})),
                error_code: None,
                error_message: None,
            };
            let output = SchedulerProbePlugin::default()
                .on_event(
                    &PluginEventEnvelope {
                        event_type: "action.completed".to_owned(),
                        payload: serde_json::to_value(completion).unwrap(),
                        ..event("ignored")
                    },
                    &Queries {
                        state: BTreeMap::new(),
                        capabilities: BTreeSet::new(),
                    },
                )
                .await
                .unwrap();
            assert_eq!(output.commands[0].payload["content"], expected);
        }
    }

    #[tokio::test]
    async fn config_probe_rejects_missing_or_oversized_config() {
        let plugin = ConfigProbePlugin::default();
        assert!(plugin.validate_config(&BTreeMap::new()).await.is_err());
        assert!(
            plugin
                .validate_config(&BTreeMap::from([(
                    "prefix".to_owned(),
                    Value::String("x".repeat(65)),
                )]))
                .await
                .is_err()
        );
        plugin
            .validate_config(&BTreeMap::from([(
                "prefix".to_owned(),
                Value::String("configured".to_owned()),
            )]))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn admin_plugin_enforces_owner_and_persists_delegation_with_audit() {
        struct AdminQueries {
            owners: Value,
            state: BTreeMap<String, StateValue>,
            capabilities: BTreeSet<String>,
        }
        impl HostQueries for AdminQueries {
            fn config_get(&self, key: &str) -> Option<&Value> {
                (key == "owners").then_some(&self.owners)
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
                &self.capabilities
            }
            fn invocation_time_ms(&self) -> i64 {
                0
            }
        }
        let queries = AdminQueries {
            owners: json!(["owner-id"]),
            state: BTreeMap::new(),
            capabilities: BTreeSet::new(),
        };
        let output = AdminPlugin::default()
            .on_event(&event("/admin grant user-2"), &queries)
            .await
            .unwrap();
        assert_eq!(
            output.commands[0].payload["content"],
            "administrator granted"
        );
        assert_eq!(output.state_ops.len(), 2);

        let denied = AdminPlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    payload: json!({
                        "type":"message",
                        "data":{
                            "text":"/admin grant user-3",
                            "sender":{"id":"ordinary"},
                            "target":{"scope":"group","group_id":"group-id"}
                        }
                    }),
                    ..event("ignored")
                },
                &queries,
            )
            .await
            .unwrap();
        assert_eq!(
            denied.commands[0].payload["content"],
            "administrator permission required"
        );
    }

    #[tokio::test]
    async fn reminder_plugin_creates_owned_persistent_schedule() {
        let output = ReminderPlugin::default()
            .on_event(
                &event("/remind 30 hydrate"),
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(output.state_ops.len(), 2);
        assert_eq!(output.commands[0].kind, "schedule.create");
        assert_eq!(output.commands[0].payload["run_at_ms"], 30_000);
        assert_eq!(output.commands[1].kind, "message.reply");
        assert_eq!(
            output.commands[0].payload["payload"]["target"]["group_id"],
            "group-id"
        );

        let long_event = PluginEventEnvelope {
            event_id: "x".repeat(512),
            ..event("/remind 30 bounded")
        };
        let bounded = ReminderPlugin::default()
            .on_event(
                &long_event,
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let StateOp::Put { key, .. } = &bounded.state_ops[0] else {
            panic!("reminder task must be persisted")
        };
        assert!(key.len() <= 256);

        let unknown = ActionCompleted {
            source_event_id: long_event.event_id,
            source_invocation_id: "invocation".to_owned(),
            command_id: "schedule".to_owned(),
            kind: "schedule.create".to_owned(),
            status: ActionStatus::Unknown,
            retryable: false,
            result: None,
            error_code: None,
            error_message: None,
        };
        let retained = ReminderPlugin::default()
            .on_event(
                &PluginEventEnvelope {
                    event_type: "action.completed".to_owned(),
                    payload: serde_json::to_value(unknown).unwrap(),
                    ..event("ignored")
                },
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        assert!(retained.state_ops.is_empty());
    }
}
