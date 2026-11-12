//! Explicit functional and Host capability probes.

use super::{
    ActionCompleted, ActionStatus, Audience, BTreeMap, BTreeSet, BrowserPermission, BrowserRun,
    BrowserRunResult, BrowserScreenshotFormat, BrowserStep, BrowserViewport, BrowserWaitUntil,
    CallerRole, CommandDeclaration, Disposition, HandlerOutput, HostQueries, HttpPermission,
    HttpRequest, HttpResponse, MediaReply, PluginCommand, PluginError, PluginEventEnvelope,
    PluginManifest, ScheduleCancel, ScheduleCreate, ScheduleTriggered, StateOp, StaticPlugin,
    Subscription, Value, async_trait, command_argument, command_declaration, command_tail,
    diagnostic_plugin_manifest, hex_bytes, ignored, is_command, json, message_text, plugin_command,
    reply,
};

fn diagnostic_authorized(queries: &dyn HostQueries) -> bool {
    let authority = queries.caller_authority();
    authority.platform == "qq.official"
        && matches!(authority.role, CallerRole::Admin | CallerRole::Owner)
}

fn restricted(event: &PluginEventEnvelope) -> HandlerOutput {
    reply(
        event,
        "Developer diagnostics are restricted to configured owners and delegated administrators",
        Vec::new(),
    )
}

mod active_send;
mod browser;
mod devtools;
mod http;
mod qq_extension;
mod scheduler;

pub use active_send::ActiveSendProbePlugin;
pub use browser::BrowserProbePlugin;
pub use devtools::DevToolsPlugin;
pub use http::HttpProbePlugin;
pub use qq_extension::QqExtensionProbePlugin;
pub use scheduler::SchedulerProbePlugin;

#[cfg(test)]
mod authorization_tests {
    use plugin_api::{CallerAuthority, ExtensionPayload, StateValue};

    use super::*;

    struct OrdinaryQueries {
        capabilities: BTreeSet<String>,
        role: CallerRole,
        platform: &'static str,
    }

    impl Default for OrdinaryQueries {
        fn default() -> Self {
            Self {
                capabilities: BTreeSet::new(),
                role: CallerRole::User,
                platform: "qq.official",
            }
        }
    }

    impl HostQueries for OrdinaryQueries {
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
                platform: self.platform.to_owned(),
                subject_hint: None,
            }
        }
    }

    fn event(text: &str) -> PluginEventEnvelope {
        PluginEventEnvelope {
            protocol_version: "1.2.0".to_owned(),
            event_id: "event".to_owned(),
            delivery_id: "delivery".to_owned(),
            invocation_id: "invocation".to_owned(),
            occurred_at_ms: None,
            received_at_ms: 0,
            adapter_id: "qq".to_owned(),
            event_type: "message.created".to_owned(),
            trace_id: None,
            payload: json!({
                "type":"message",
                "data":{
                    "text":text,
                    "sender":{"id":"ordinary-user"},
                    "target":{"scope":"group","group_id":"group"}
                }
            }),
            extensions: vec![ExtensionPayload {
                namespace: "qq.official.raw".to_owned(),
                schema_version: "1.0".to_owned(),
                content_type: "application/json".to_owned(),
                data: br#"{"t":"GROUP_AT_MESSAGE_CREATE"}"#.to_vec(),
            }],
        }
    }

    #[tokio::test]
    async fn ordinary_users_cannot_trigger_any_diagnostic_probe_side_effect() {
        let cases: Vec<(std::sync::Arc<dyn StaticPlugin>, &str)> = vec![
            (
                std::sync::Arc::new(HttpProbePlugin::default()),
                "/http-probe",
            ),
            (
                std::sync::Arc::new(BrowserProbePlugin::default()),
                "/screenshot example.com",
            ),
            (
                std::sync::Arc::new(QqExtensionProbePlugin::default()),
                "/qq-extension",
            ),
            (
                std::sync::Arc::new(SchedulerProbePlugin::default()),
                "/schedule 1",
            ),
            (
                std::sync::Arc::new(SchedulerProbePlugin::default()),
                "/schedule-cancel task-id",
            ),
            (
                std::sync::Arc::new(ActiveSendProbePlugin::default()),
                "/active-send hello",
            ),
        ];
        for (plugin, command) in cases {
            let output = plugin
                .on_event(&event(command), &OrdinaryQueries::default())
                .await
                .unwrap();
            assert_eq!(output.commands.len(), 1, "unexpected output for {command}");
            assert_eq!(output.commands[0].kind, "message.reply");
            assert!(
                output.commands[0].payload["content"]
                    .as_str()
                    .unwrap()
                    .contains("restricted")
            );
        }
    }

    #[tokio::test]
    async fn privileged_roles_from_other_platforms_cannot_trigger_diagnostics() {
        let output = HttpProbePlugin::default()
            .on_event(
                &event("/http-probe"),
                &OrdinaryQueries {
                    capabilities: BTreeSet::new(),
                    role: CallerRole::Owner,
                    platform: "onebot11",
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands.len(), 1);
        assert_eq!(output.commands[0].kind, "message.reply");
        assert!(
            output.commands[0].payload["content"]
                .as_str()
                .unwrap()
                .contains("restricted")
        );
    }
}
