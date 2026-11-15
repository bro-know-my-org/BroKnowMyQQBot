//! Built-in plugins distributed with `BroKnowMyQQBot`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use plugin_api::{
    ActionCompleted, ActionStatus, Audience, CallerAuthority, CallerRole, CommandContext,
    CommandDeclaration, Disposition, HandlerOutput, HostQueries, PluginCommand, PluginError,
    PluginEventEnvelope, PluginId, PluginManifest, PluginMetadata, PluginPermissions,
    PluginRuntimeConfig, ScheduleCancel, ScheduleCreate, ScheduleTriggered, StateOp, StaticPlugin,
    StoragePermission, SubcommandDeclaration, Subscription,
};
#[cfg(feature = "coverage-probes")]
use plugin_api::{
    BrowserPermission, BrowserRun, BrowserRunResult, BrowserScreenshotFormat, BrowserStep,
    BrowserViewport, BrowserWaitUntil, HttpPermission, HttpRequest, HttpResponse, MediaReply,
};
use serde_json::{Value, json};

#[cfg(test)]
mod test_queries;

mod basic;
mod business;
#[cfg(feature = "coverage-probes")]
mod probes;

pub use basic::{CounterPlugin, EchoPlugin, HelpPlugin, PingPlugin};
pub use business::{AdminPlugin, ReminderPlugin};
#[cfg(feature = "coverage-probes")]
pub use probes::{
    ActiveSendProbePlugin, BrowserProbePlugin, DevToolsPlugin, HttpProbePlugin,
    QqExtensionProbePlugin, SchedulerProbePlugin,
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
        protocol: ">=1.4,<2.0".to_owned(),
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
            description: command_description(command),
            audience: Audience::Public,
            subcommands: Vec::new(),
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

#[cfg(feature = "coverage-probes")]
fn diagnostic_plugin_manifest(
    id: &str,
    name: &str,
    command: &str,
    action: &str,
    storage: bool,
) -> PluginManifest {
    let mut manifest = message_plugin_manifest(id, name, command, action, storage);
    manifest.commands[0].audience = Audience::DevDiagnostic;
    manifest
}

fn command_declaration(name: &str, description: &str) -> CommandDeclaration {
    CommandDeclaration {
        name: name.to_owned(),
        aliases: Vec::new(),
        description: description.to_owned(),
        audience: Audience::Public,
        subcommands: Vec::new(),
    }
}

fn command_description(command: &str) -> String {
    match command {
        "ping" => "测试机器人".to_owned(),
        "count" => "查看计数".to_owned(),
        "echo" => "原样回显".to_owned(),
        "help" => "查看本帮助".to_owned(),
        "remind" => "创建提醒".to_owned(),
        _ => format!("Run {command}"),
    }
}

fn is_command(command: Option<&CommandContext>, name: &str) -> bool {
    command_tail(command, name).is_some_and(|arguments| arguments.trim().is_empty())
}

fn command_argument<'a>(command: Option<&'a CommandContext>, name: &str) -> Option<&'a str> {
    command_tail(command, name).filter(|argument| !argument.trim().is_empty())
}

fn command_tail<'a>(command: Option<&'a CommandContext>, name: &str) -> Option<&'a str> {
    let command = command?;
    (command.name == name).then_some(command.arguments.as_str())
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

#[cfg(test)]
mod tests {
    use crate::test_queries::TestPlugin as _;
    use std::collections::{BTreeMap, BTreeSet};

    use plugin_api::{
        ActionCompleted, ActionStatus, Audience, CallerAuthority, CallerRole, CommandDeclaration,
        HostQueries, PluginEventEnvelope, StateOp, StateValue, StaticPlugin,
    };
    use serde_json::{Value, json};

    #[cfg(feature = "coverage-probes")]
    use super::{
        ActiveSendProbePlugin, BrowserProbePlugin, DevToolsPlugin, HttpProbePlugin,
        QqExtensionProbePlugin, SchedulerProbePlugin,
    };
    use super::{AdminPlugin, CounterPlugin, EchoPlugin, HelpPlugin, PingPlugin, ReminderPlugin};

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

        fn caller_authority(&self) -> CallerAuthority {
            CallerAuthority::default()
        }
    }

    #[cfg(feature = "coverage-probes")]
    struct AuthorizedQueries<'a>(&'a Queries);

    #[cfg(feature = "coverage-probes")]
    impl HostQueries for AuthorizedQueries<'_> {
        fn config_get(&self, key: &str) -> Option<&Value> {
            self.0.config_get(key)
        }

        fn state_get(&self, key: &str) -> Option<&StateValue> {
            self.0.state_get(key)
        }

        fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)> {
            self.0.state_scan(prefix, limit)
        }

        fn granted_capabilities(&self) -> &BTreeSet<String> {
            self.0.granted_capabilities()
        }

        fn invocation_time_ms(&self) -> i64 {
            self.0.invocation_time_ms()
        }

        fn caller_authority(&self) -> CallerAuthority {
            CallerAuthority {
                role: CallerRole::Owner,
                platform: "qq.official".to_owned(),
                subject_hint: None,
            }
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
            .on_test_event(
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
    async fn every_bundled_command_follows_multicharacter_and_unicode_prefixes() {
        let queries = Queries {
            state: BTreeMap::new(),
            capabilities: BTreeSet::new(),
        };
        let cases: Vec<(Box<dyn StaticPlugin>, &str)> = vec![
            (Box::new(PingPlugin::default()), "ping"),
            (Box::new(HelpPlugin::default()), "help"),
            (Box::new(CounterPlugin::default()), "count"),
            (Box::new(EchoPlugin::default()), "echo 第一行\n 第二行\n "),
            (Box::new(AdminPlugin::default()), "admin invalid"),
            (Box::new(ReminderPlugin::default()), "remind"),
        ];
        #[cfg(feature = "coverage-probes")]
        let queries = AuthorizedQueries(&queries);
        #[cfg(feature = "coverage-probes")]
        let cases = cases.into_iter().chain([
            (
                Box::new(DevToolsPlugin::default()) as Box<dyn StaticPlugin>,
                "whoami",
            ),
            (Box::new(HttpProbePlugin::default()), "http-probe"),
            (
                Box::new(BrowserProbePlugin::default()),
                "screenshot example.com",
            ),
            (Box::new(QqExtensionProbePlugin::default()), "qq-extension"),
            (
                Box::new(SchedulerProbePlugin::default()),
                "schedule invalid",
            ),
            (
                Box::new(ActiveSendProbePlugin::default()),
                "active-send hello",
            ),
        ]);
        for (plugin, command) in cases {
            for prefix in ["!!", "机器人："] {
                let input = event(&format!("{prefix}{command}"));
                let snapshot =
                    crate::test_queries::CommandQueries::with_prefix(&input, &queries, prefix);
                let output = plugin.on_event(&input, &snapshot).await.unwrap();
                assert!(
                    !output.commands.is_empty(),
                    "{} ignored {prefix}{command}",
                    plugin.manifest().id
                );
                let old = event(&format!("/{command}"));
                let snapshot =
                    crate::test_queries::CommandQueries::with_prefix(&old, &queries, prefix);
                let output = plugin.on_event(&old, &snapshot).await.unwrap();
                assert!(
                    output.commands.is_empty(),
                    "{} accepted old prefix",
                    plugin.manifest().id
                );
                assert_eq!(output.state_ops, []);
            }
        }
    }

    #[tokio::test]
    async fn help_lists_the_supplied_manifest_commands() {
        let plugin = HelpPlugin::with_commands([
            super::command_declaration("ping", "Ping command"),
            super::command_declaration("http-probe", "HTTP command"),
        ]);
        let output = plugin
            .on_test_event(
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
        assert!(content.contains("当前身份：普通用户"));
        assert!(content.contains("[公开]"));
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "covers the complete help role matrix"
    )]
    async fn help_partitions_admin_subcommands_and_filters_diagnostics() {
        struct AuthorityQueries {
            role: CallerRole,
            platform: String,
            capabilities: BTreeSet<String>,
        }
        impl HostQueries for AuthorityQueries {
            fn config_get(&self, _key: &str) -> Option<&Value> {
                None
            }
            fn state_get(&self, _key: &str) -> Option<&StateValue> {
                None
            }
            fn state_scan(&self, _prefix: &str, _limit: usize) -> Vec<(&str, &StateValue)> {
                Vec::new()
            }
            fn granted_capabilities(&self) -> &BTreeSet<String> {
                &self.capabilities
            }
            fn invocation_time_ms(&self) -> i64 {
                0
            }
            fn caller_authority(&self) -> CallerAuthority {
                CallerAuthority {
                    role: self.role,
                    platform: self.platform.clone(),
                    subject_hint: None,
                }
            }
        }
        let mut admin = super::command_declaration("admin", "管理员功能");
        admin.subcommands = vec![
            plugin_api::SubcommandDeclaration {
                name: "whoami".to_owned(),
                description: "查询自己的身份".to_owned(),
                audience: Audience::Public,
            },
            plugin_api::SubcommandDeclaration {
                name: "list".to_owned(),
                description: "查看管理员列表".to_owned(),
                audience: Audience::Admin,
            },
            plugin_api::SubcommandDeclaration {
                name: "grant".to_owned(),
                description: "委派管理员".to_owned(),
                audience: Audience::Owner,
            },
        ];
        let diagnostic = CommandDeclaration {
            name: "probe".to_owned(),
            aliases: vec!["p".to_owned()],
            description: "探针".to_owned(),
            audience: Audience::DevDiagnostic,
            subcommands: Vec::new(),
        };
        let hidden = CommandDeclaration {
            name: "secret".to_owned(),
            aliases: Vec::new(),
            description: "隐藏".to_owned(),
            audience: Audience::Hidden,
            subcommands: Vec::new(),
        };
        let plugin = HelpPlugin::with_commands([admin, diagnostic, hidden]);

        let output = plugin
            .on_test_event(
                &event("/help"),
                &AuthorityQueries {
                    role: CallerRole::User,
                    platform: "qq.official".to_owned(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let content = output.commands[0].payload["content"].as_str().unwrap();
        assert!(content.contains("当前平台：QQ 官方"));
        assert!(content.contains("/admin whoami"));
        assert!(content.contains("需要更高权限"));
        assert!(content.contains("/admin list"));
        assert!(content.contains("/admin grant"));
        assert!(!content.contains("/probe"));
        assert!(!content.contains("/secret"));
        assert!(!content.contains("/p "));

        let administrator = plugin
            .on_test_event(
                &event("/help"),
                &AuthorityQueries {
                    role: CallerRole::Admin,
                    platform: "qq.official".to_owned(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let administrator_content = administrator.commands[0].payload["content"]
            .as_str()
            .unwrap();
        assert!(administrator_content.contains("/probe"));
        assert!(
            administrator_content.find("/admin list").unwrap()
                < administrator_content.find("需要更高权限").unwrap()
        );
        assert!(
            administrator_content.find("需要更高权限").unwrap()
                < administrator_content.find("/admin grant").unwrap()
        );

        let owner = plugin
            .on_test_event(
                &event("/help"),
                &AuthorityQueries {
                    role: CallerRole::Owner,
                    platform: "qq.official".to_owned(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let owner_content = owner.commands[0].payload["content"].as_str().unwrap();
        assert!(owner_content.contains("/probe"));
        assert!(!owner_content.contains("需要更高权限"));

        let malicious_platform = plugin
            .on_test_event(
                &event("/help"),
                &AuthorityQueries {
                    role: CallerRole::User,
                    platform: format!("custom\n/fake{}", "界".repeat(10_000)),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let malicious_content = malicious_platform.commands[0].payload["content"]
            .as_str()
            .unwrap();
        assert!(malicious_content.contains("custom\\n/fake"));
        assert!(!malicious_content.contains("custom\n/fake"));
        assert!(malicious_content.len() <= 4_096);
    }

    #[test]
    #[cfg(feature = "coverage-probes")]
    fn every_bundled_diagnostic_command_declares_diagnostic_audience() {
        let manifests = [
            ActiveSendProbePlugin::default().manifest().clone(),
            BrowserProbePlugin::default().manifest().clone(),
            DevToolsPlugin::default().manifest().clone(),
            HttpProbePlugin::default().manifest().clone(),
            QqExtensionProbePlugin::default().manifest().clone(),
            SchedulerProbePlugin::default().manifest().clone(),
        ];

        for manifest in manifests {
            assert!(
                manifest
                    .commands
                    .iter()
                    .all(|command| command.audience == Audience::DevDiagnostic),
                "{} has a non-diagnostic command declaration",
                manifest.id
            );
        }
    }

    #[tokio::test]
    async fn help_escapes_manifest_control_characters() {
        let plugin = HelpPlugin::with_commands([CommandDeclaration {
            name: "safe\nforged".to_owned(),
            aliases: Vec::new(),
            description: "description\n需要更高权限\n/fake".to_owned(),
            audience: Audience::Public,
            subcommands: Vec::new(),
        }]);
        let output = plugin
            .on_test_event(
                &event("/help"),
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let content = output.commands[0].payload["content"].as_str().unwrap();

        assert!(content.contains("/safe\\nforged"));
        assert!(content.contains("description\\n需要更高权限\\n/fake"));
        assert_eq!(content.matches("需要更高权限").count(), 1);
    }

    #[tokio::test]
    async fn help_caps_alignment_for_long_command_names() {
        let plugin = HelpPlugin::with_commands([
            super::command_declaration("short", "Short command"),
            super::command_declaration(&"x".repeat(10_000), "Long command"),
        ]);
        let output = plugin
            .on_test_event(
                &event("/help"),
                &Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let content = output.commands[0].payload["content"].as_str().unwrap();
        let short = content.find("/short").unwrap();
        let label = content[short..].find("[公开]").unwrap();

        assert!(
            label <= 40,
            "short command alignment was unbounded: {label}"
        );
        assert!(content.len() <= 4_096);
        assert!(content.contains("另有 1 条命令未显示"));
    }

    #[tokio::test]
    async fn counter_uses_revision_for_state_update() {
        let output = CounterPlugin::default()
            .on_test_event(
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
            .on_test_event(
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
    #[cfg(feature = "coverage-probes")]
    async fn active_send_probe_confirms_then_uses_explicit_target() {
        let output = ActiveSendProbePlugin::default()
            .on_test_event(
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
                &AuthorizedQueries(&Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "message.reply");
        assert_eq!(output.commands[1].kind, "message.send");
        assert_eq!(output.commands[1].payload["content"], "hello world");
        assert_eq!(output.commands[1].payload["target"]["group_id"], "group-id");
    }

    #[tokio::test]
    #[cfg(feature = "coverage-probes")]
    async fn http_probe_emits_only_host_http_command() {
        let output = HttpProbePlugin::default()
            .on_test_event(
                &event("/http-probe"),
                &AuthorizedQueries(&Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "http.request");
        assert_eq!(output.commands[0].payload["method"], "GET");
        assert_eq!(output.commands[0].payload["url"], "https://example.com/");
    }

    #[tokio::test]
    #[cfg(feature = "coverage-probes")]
    async fn scheduler_probe_emits_persistent_schedule_command() {
        let output = SchedulerProbePlugin::default()
            .on_test_event(
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
                &AuthorizedQueries(&Queries {
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "schedule.create");
        assert_eq!(output.commands[0].payload["run_at_ms"], 30_000);
        assert_eq!(output.commands[0].payload["payload"], json!({}));
        assert_eq!(output.commands.len(), 1);
        assert_eq!(output.state_ops.len(), 1);

        let cancel = SchedulerProbePlugin::default()
            .on_test_event(
                &PluginEventEnvelope {
                    payload: json!({
                        "type":"message",
                        "data":{
                            "text":"/schedule-cancel  probe-6576656e742d31  ",
                            "target":{"scope":"group","group_id":"group-id"},
                            "sender":{"id":"user-id"}
                        }
                    }),
                    ..event("ignored")
                },
                &AuthorizedQueries(&Queries {
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
                }),
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
                .on_test_event(
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
    #[allow(
        clippy::too_many_lines,
        reason = "covers authorization, parsing, state, and audit behavior together"
    )]
    async fn admin_plugin_enforces_owner_and_persists_delegation_with_audit() {
        struct AdminQueries {
            owners: Value,
            state: BTreeMap<String, StateValue>,
            capabilities: BTreeSet<String>,
            role: CallerRole,
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
            fn caller_authority(&self) -> CallerAuthority {
                CallerAuthority {
                    role: self.role,
                    platform: "qq.official".to_owned(),
                    subject_hint: None,
                }
            }
        }
        let queries = AdminQueries {
            owners: json!(["owner-id"]),
            state: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            role: CallerRole::Owner,
        };
        let output = AdminPlugin::default()
            .on_test_event(&event("/admin grant user-2"), &queries)
            .await
            .unwrap();
        assert_eq!(
            output.commands[0].payload["content"],
            "administrator granted"
        );
        assert_eq!(output.state_ops.len(), 2);

        let administrator_queries = AdminQueries {
            owners: json!(["owner-id"]),
            state: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            role: CallerRole::Admin,
        };
        let listed = AdminPlugin::default()
            .on_test_event(&event("/admin list"), &administrator_queries)
            .await
            .unwrap();
        assert!(
            listed.commands[0].payload["content"]
                .as_str()
                .unwrap()
                .contains("owner-id")
        );
        let administrator_grant = AdminPlugin::default()
            .on_test_event(&event("/admin grant user-3"), &administrator_queries)
            .await
            .unwrap();
        assert_eq!(
            administrator_grant.commands[0].payload["content"],
            "owner permission required"
        );

        let mut anonymous_whoami = event("/admin whoami");
        anonymous_whoami.payload["data"]
            .as_object_mut()
            .unwrap()
            .remove("sender");
        let anonymous = AdminPlugin::default()
            .on_test_event(
                &anonymous_whoami,
                &AdminQueries {
                    owners: json!(["owner-id"]),
                    state: BTreeMap::new(),
                    capabilities: BTreeSet::new(),
                    role: CallerRole::User,
                },
            )
            .await
            .unwrap();
        assert_eq!(anonymous.commands[0].payload["content"], "ordinary user");

        let denied = AdminPlugin::default()
            .on_test_event(
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
                &AdminQueries {
                    role: CallerRole::User,
                    ..queries
                },
            )
            .await
            .unwrap();
        assert_eq!(
            denied.commands[0].payload["content"],
            "owner permission required"
        );

        for malformed in [
            "/admin",
            "/admin whoami extra",
            "/admin list extra",
            "/admin grant",
            "/admin revoke user-3 extra",
        ] {
            let output = AdminPlugin::default()
                .on_test_event(&event(malformed), &administrator_queries)
                .await
                .unwrap();
            assert_eq!(
                output.commands[0].payload["content"],
                "usage: /admin whoami|list|grant <sender-id>|revoke <sender-id>",
                "malformed command was not handled as usage: {malformed}"
            );
        }
    }

    #[tokio::test]
    async fn reminder_plugin_creates_owned_persistent_schedule() {
        let output = ReminderPlugin::default()
            .on_test_event(
                &event("/remind  30  hydrate  "),
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

        let state = output
            .state_ops
            .iter()
            .filter_map(|operation| match operation {
                StateOp::Put { key, value, .. } => Some((
                    key.clone(),
                    StateValue {
                        value: value.clone(),
                        revision: 1,
                    },
                )),
                StateOp::Delete { .. } => None,
            })
            .collect();
        let queries = Queries {
            state,
            capabilities: BTreeSet::new(),
        };
        let listed = ReminderPlugin::default()
            .on_test_event(&event("/reminders  "), &queries)
            .await
            .unwrap();
        assert!(
            listed.commands[0].payload["content"]
                .as_str()
                .unwrap()
                .contains("hydrate")
        );
        let task_id = output.commands[0].payload["task_id"].as_str().unwrap();
        let cancelled = ReminderPlugin::default()
            .on_test_event(&event(&format!("/remind-cancel  {task_id}  ")), &queries)
            .await
            .unwrap();
        assert_eq!(cancelled.commands[0].kind, "schedule.cancel");
        assert_eq!(cancelled.commands[0].payload["task_id"], task_id);

        let long_event = PluginEventEnvelope {
            event_id: "x".repeat(512),
            ..event("/remind 30 bounded")
        };
        let bounded = ReminderPlugin::default()
            .on_test_event(
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
            .on_test_event(
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
        assert_eq!(retained.state_ops, []);
    }
}
