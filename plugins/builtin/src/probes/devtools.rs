#[cfg(test)]
use super::CallerRole;
use super::{
    ActionCompleted, ActionStatus, Audience, BTreeSet, Disposition, HandlerOutput, HostQueries,
    PluginCommand, PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin, Subscription,
    Value, async_trait, command_argument, command_declaration, diagnostic_authorized,
    diagnostic_plugin_manifest, ignored, is_command, json, reply, restricted,
};
use std::collections::BTreeMap;

const API_PERMISSIONS_ACTION: &str = "qq.guild.api-permission.list";
const MAX_PERMISSION_REPLY_BYTES: usize = 1_800;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionView {
    Summary,
    Channels,
    All,
}

impl PermissionView {
    fn command_id(self) -> &'static str {
        match self {
            Self::Summary => "api-permissions",
            Self::Channels => "api-permissions-channels",
            Self::All => "api-permissions-all",
        }
    }

    fn from_command_id(command_id: &str) -> Option<Self> {
        match command_id {
            "api-permissions" => Some(Self::Summary),
            "api-permissions-channels" => Some(Self::Channels),
            "api-permissions-all" => Some(Self::All),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct PermissionEntry<'a> {
    method: &'a str,
    path: &'a str,
    description: &'a str,
    status: i64,
}

/// Explicitly enabled development and runtime-diagnostic commands.
#[derive(Debug)]
pub struct DevToolsPlugin {
    manifest: PluginManifest,
    config_schema: Value,
}

impl Default for DevToolsPlugin {
    fn default() -> Self {
        let mut manifest = diagnostic_plugin_manifest(
            "dev.bkm.devtools",
            "Developer Tools",
            "whoami",
            "message.reply",
            false,
        );
        "Show the current sender and message target"
            .clone_into(&mut manifest.commands[0].description);
        manifest.commands.push(command_declaration(
            "api-permissions",
            "List QQ guild API permissions",
        ));
        manifest.commands[1].audience = Audience::DevDiagnostic;
        ">=1.1,<2.0".clone_into(&mut manifest.protocol);
        manifest.subscriptions.push(Subscription {
            id: "platform-query-results".to_owned(),
            event: "action.completed".to_owned(),
            priority: 0,
            scopes: BTreeSet::new(),
        });
        manifest
            .permissions
            .event_extensions
            .insert("qq.official.raw".to_owned());
        Self {
            manifest,
            config_schema: json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{},
                "additionalProperties":false
            }),
        }
    }
}

#[async_trait]
impl StaticPlugin for DevToolsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn config_schema(&self) -> Option<&Value> {
        Some(&self.config_schema)
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "action.completed" {
            return handle_action_completed(event, &queries.command_prefix());
        }
        let command = queries.command_context();
        if is_command(command.as_ref(), "whoami") {
            return if diagnostic_authorized(queries) {
                Ok(whoami(event))
            } else {
                Ok(restricted(event))
            };
        }
        let view = if is_command(command.as_ref(), "api-permissions") {
            Some(PermissionView::Summary)
        } else {
            match command_argument(command.as_ref(), "api-permissions").map(str::trim) {
                Some("") => Some(PermissionView::Summary),
                Some("channels") => Some(PermissionView::Channels),
                Some("all") => Some(PermissionView::All),
                Some(_) => None,
                None => return Ok(ignored()),
            }
        };
        if !diagnostic_authorized(queries) {
            return Ok(restricted(event));
        }
        let Some(view) = view else {
            return Ok(reply(
                event,
                &format!(
                    "Usage: {}api-permissions [channels|all]",
                    queries.command_prefix()
                ),
                Vec::new(),
            ));
        };
        let Some(guild_id) = qq_guild_id(event)? else {
            return Ok(reply(
                event,
                &format!(
                    "QQ guild context is unavailable; run {}api-permissions in a QQ channel",
                    queries.command_prefix()
                ),
                Vec::new(),
            ));
        };
        Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: Vec::new(),
            commands: vec![PluginCommand {
                command_id: view.command_id().to_owned(),
                kind: API_PERMISSIONS_ACTION.to_owned(),
                idempotency_key: Some(format!("{}/{}", event.event_id, view.command_id())),
                deadline_ms: Some(10_000),
                payload: json!({"guild_id":guild_id}),
            }],
            diagnostics: Vec::new(),
        })
    }
}

fn whoami(event: &PluginEventEnvelope) -> HandlerOutput {
    let sender = event
        .payload
        .pointer("/data/sender/id")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let target = event
        .payload
        .pointer("/data/target")
        .map_or_else(|| "unavailable".to_owned(), Value::to_string);
    reply(
        event,
        &format!(
            "adapter: {}\nsender_id: {sender}\ntarget: {target}",
            event.adapter_id
        ),
        Vec::new(),
    )
}

fn handle_action_completed(
    event: &PluginEventEnvelope,
    prefix: &str,
) -> Result<HandlerOutput, PluginError> {
    let completion: ActionCompleted = serde_json::from_value(event.payload.clone())
        .map_err(|error| PluginError::Permanent(error.to_string()))?;
    if completion.kind != API_PERMISSIONS_ACTION {
        return Ok(ignored());
    }
    let Some(view) = PermissionView::from_command_id(&completion.command_id) else {
        return Ok(ignored());
    };
    let content = if completion.status == ActionStatus::Succeeded {
        let result = completion
            .result
            .as_ref()
            .ok_or_else(|| PluginError::Permanent("API permission result is missing".to_owned()))?;
        match view {
            PermissionView::Summary => format_api_permission_summary(result, prefix)?,
            PermissionView::Channels => format_channel_permissions(result)?,
            PermissionView::All => format_all_api_permissions(result)?,
        }
    } else {
        format!(
            "API permission query {:?}: {}{}",
            completion.status,
            completion.error_code.as_deref().unwrap_or("unknown"),
            completion
                .error_message
                .as_deref()
                .map(|message| format!(" ({message})"))
                .unwrap_or_default()
        )
    };
    Ok(reply(
        event,
        &truncate_permission_reply(content),
        Vec::new(),
    ))
}

fn api_permission_entries(result: &Value) -> Result<Vec<PermissionEntry<'_>>, PluginError> {
    let apis = result
        .pointer("/raw/apis")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::Permanent("API permission list is invalid".to_owned()))?;
    let mut entries = apis
        .iter()
        .map(|api| {
            let path = required_string(api, "path")?;
            let method = required_string(api, "method")?;
            let description = required_string(api, "desc")?;
            let status = api
                .get("auth_status")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    PluginError::Permanent("API permission auth_status is invalid".to_owned())
                })?;
            Ok(PermissionEntry {
                method,
                path,
                description,
                status,
            })
        })
        .collect::<Result<Vec<_>, PluginError>>()?;
    entries.sort_by(|left, right| (left.method, left.path).cmp(&(right.method, right.path)));
    Ok(entries)
}

fn format_api_permission_summary(result: &Value, prefix: &str) -> Result<String, PluginError> {
    let entries = api_permission_entries(result)?;
    let mut status_counts = BTreeMap::<i64, usize>::new();
    for entry in &entries {
        *status_counts.entry(entry.status).or_default() += 1;
    }
    let counts = if status_counts.is_empty() {
        "none".to_owned()
    } else {
        status_counts
            .into_iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let create_channel = entries
        .iter()
        .find(|entry| entry.method == "POST" && entry.path == "/guilds/{guild_id}/channels");
    let create_channel_line = create_channel.map_or_else(
        || "[not reported] POST /guilds/{guild_id}/channels".to_owned(),
        |entry| format_permission_entry(entry),
    );
    Ok(format!(
        "API permissions: {} total\nauth_status: {counts}\ncreate channel:\n{create_channel_line}\nUse {prefix}api-permissions channels for Channel CRUD; {prefix}api-permissions all for the full list.",
        entries.len()
    ))
}

fn format_channel_permissions(result: &Value) -> Result<String, PluginError> {
    let entries = api_permission_entries(result)?;
    let entries = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.path,
                "/guilds/{guild_id}/channels" | "/channels/{channel_id}"
            )
        })
        .collect::<Vec<_>>();
    Ok(format_permission_entries(
        "Channel API permissions",
        &entries,
    ))
}

fn format_all_api_permissions(result: &Value) -> Result<String, PluginError> {
    let entries = api_permission_entries(result)?;
    let entries = entries.iter().collect::<Vec<_>>();
    Ok(format_permission_entries("API permissions", &entries))
}

fn format_permission_entries(title: &str, entries: &[&PermissionEntry<'_>]) -> String {
    if entries.is_empty() {
        return format!("{title}: none");
    }
    let total = entries.len();
    let mut output = format!("{title} ({total}):");
    let mut shown = 0;
    let mut line_boundaries = Vec::new();
    for entry in entries {
        let line = format!("\n{}", format_permission_entry(entry));
        if output.len() + line.len() > MAX_PERMISSION_REPLY_BYTES {
            break;
        }
        line_boundaries.push(output.len());
        output.push_str(&line);
        shown += 1;
    }
    if shown < total {
        let mut suffix = format!("\n… {} more omitted", total - shown);
        while output.len() + suffix.len() > MAX_PERMISSION_REPLY_BYTES && shown > 0 {
            output.truncate(line_boundaries.pop().expect("shown lines have boundaries"));
            shown -= 1;
            suffix = format!("\n… {} more omitted", total - shown);
        }
        output.push_str(&suffix);
    }
    output
}

fn truncate_permission_reply(mut content: String) -> String {
    const SUFFIX: &str = "\n… response truncated";
    if content.len() <= MAX_PERMISSION_REPLY_BYTES {
        return content;
    }
    let budget = MAX_PERMISSION_REPLY_BYTES.saturating_sub(SUFFIX.len());
    let mut end = budget.min(content.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    content.push_str(SUFFIX);
    content
}

fn format_permission_entry(entry: &PermissionEntry<'_>) -> String {
    format!(
        "[{}] {} {} — {}",
        entry.status, entry.method, entry.path, entry.description
    )
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, PluginError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Permanent(format!("API permission {field} is invalid")))
}

fn qq_guild_id(event: &PluginEventEnvelope) -> Result<Option<String>, PluginError> {
    event
        .extensions
        .iter()
        .find(|extension| extension.namespace == "qq.official.raw")
        .map(|extension| serde_json::from_slice::<Value>(&extension.data))
        .transpose()
        .map_err(|error| PluginError::Permanent(error.to_string()))
        .map(|raw| {
            raw.and_then(|raw| {
                raw.pointer("/d/guild_id")
                    .and_then(Value::as_str)
                    .filter(|guild_id| {
                        !guild_id.is_empty() && !guild_id.chars().any(char::is_control)
                    })
                    .map(ToOwned::to_owned)
            })
        })
}

#[cfg(test)]
mod tests {
    use crate::test_queries::TestPlugin as _;
    use std::collections::BTreeSet;

    use plugin_api::{
        ActionCompleted, ActionStatus, CallerAuthority, ExtensionPayload, StateValue,
    };

    use super::*;

    struct Queries {
        capabilities: BTreeSet<String>,
        role: CallerRole,
        platform: &'static str,
    }

    impl HostQueries for Queries {
        fn config_get(&self, _key: &str) -> Option<&Value> {
            panic!("DevTools authorization must use caller_authority, not plugin configuration")
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

    fn queries() -> Queries {
        Queries {
            capabilities: BTreeSet::new(),
            role: CallerRole::Owner,
            platform: "qq.official",
        }
    }

    fn event(text: &str) -> PluginEventEnvelope {
        PluginEventEnvelope {
            protocol_version: "1.2.0".to_owned(),
            event_id: "event-1".to_owned(),
            delivery_id: "delivery-1".to_owned(),
            invocation_id: "invocation-1".to_owned(),
            occurred_at_ms: None,
            received_at_ms: 0,
            adapter_id: "qq-main".to_owned(),
            event_type: "message.created".to_owned(),
            trace_id: None,
            payload: json!({
                "type":"message",
                "data":{
                    "text":text,
                    "sender":{"id":"sender-id"},
                    "target":{"scope":"channel","channel_id":"channel-id"}
                }
            }),
            extensions: vec![ExtensionPayload {
                namespace: "qq.official.raw".to_owned(),
                schema_version: "1.0".to_owned(),
                content_type: "application/json".to_owned(),
                data: serde_json::to_vec(&json!({"d":{"guild_id":"guild-id"}})).unwrap(),
            }],
        }
    }

    #[tokio::test]
    async fn whoami_reports_sender_adapter_and_target_for_admin_and_owner() {
        for role in [CallerRole::Admin, CallerRole::Owner] {
            let output = DevToolsPlugin::default()
                .on_test_event(&event("/whoami"), &Queries { role, ..queries() })
                .await
                .unwrap();
            let content = output.commands[0].payload["content"].as_str().unwrap();
            assert!(content.contains("adapter: qq-main"));
            assert!(content.contains("sender_id: sender-id"));
            assert!(content.contains("channel-id"));
        }
    }

    #[tokio::test]
    async fn api_permissions_emits_the_narrow_platform_query() {
        for role in [CallerRole::Admin, CallerRole::Owner] {
            let queries = Queries { role, ..queries() };
            for (text, command_id) in [
                ("/api-permissions", "api-permissions"),
                ("/api-permissions channels", "api-permissions-channels"),
                ("/api-permissions all", "api-permissions-all"),
                ("/api-permissions  all  ", "api-permissions-all"),
                ("/api-permissions channels  ", "api-permissions-channels"),
                ("/api-permissions  ", "api-permissions"),
            ] {
                let output = DevToolsPlugin::default()
                    .on_test_event(&event(text), &queries)
                    .await
                    .unwrap();
                assert_eq!(output.commands.len(), 1);
                assert_eq!(output.commands[0].command_id, command_id);
                assert_eq!(output.commands[0].kind, API_PERMISSIONS_ACTION);
                assert_eq!(output.commands[0].payload, json!({"guild_id":"guild-id"}));
            }
        }
    }

    #[tokio::test]
    async fn api_permissions_rejects_users() {
        let output = DevToolsPlugin::default()
            .on_test_event(
                &event("/api-permissions"),
                &Queries {
                    role: CallerRole::User,
                    ..queries()
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
                .contains("restricted to configured owners")
        );
    }

    #[tokio::test]
    async fn whoami_rejects_users() {
        let output = DevToolsPlugin::default()
            .on_test_event(
                &event("/whoami"),
                &Queries {
                    role: CallerRole::User,
                    ..queries()
                },
            )
            .await
            .unwrap();
        assert_eq!(output.commands[0].kind, "message.reply");
        assert!(
            output.commands[0].payload["content"]
                .as_str()
                .unwrap()
                .contains("restricted to configured owners")
        );
    }

    #[tokio::test]
    async fn privileged_roles_from_other_platforms_cannot_run_devtools() {
        for role in [CallerRole::Admin, CallerRole::Owner] {
            let queries = Queries {
                role,
                platform: "onebot11",
                ..queries()
            };
            for command in ["/whoami", "/api-permissions"] {
                let output = DevToolsPlugin::default()
                    .on_test_event(&event(command), &queries)
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
    }

    #[tokio::test]
    async fn api_permissions_authorizes_before_reporting_invalid_arguments() {
        let output = DevToolsPlugin::default()
            .on_test_event(
                &event("/api-permissions invalid"),
                &Queries {
                    role: CallerRole::User,
                    ..queries()
                },
            )
            .await
            .unwrap();
        assert!(
            output.commands[0].payload["content"]
                .as_str()
                .unwrap()
                .contains("restricted to configured owners")
        );
    }

    #[tokio::test]
    async fn api_permission_completion_defaults_to_concise_summary() {
        let mut completion = event("ignored");
        completion.event_type = "action.completed".to_owned();
        completion.payload = serde_json::to_value(ActionCompleted {
            source_event_id: "source".to_owned(),
            source_invocation_id: "invocation".to_owned(),
            command_id: "api-permissions".to_owned(),
            kind: API_PERMISSIONS_ACTION.to_owned(),
            status: ActionStatus::Succeeded,
            retryable: false,
            result: Some(json!({
                "raw": {
                    "apis": [
                        {"path":"/z","method":"POST","desc":"Zed","auth_status":0},
                        {"path":"/a","method":"GET","desc":"Aye","auth_status":1},
                        {"path":"/guilds/{guild_id}/channels","method":"POST","desc":"创建子频道","auth_status":1}
                    ]
                }
            })),
            error_code: None,
            error_message: None,
        })
        .unwrap();
        let output = DevToolsPlugin::default()
            .on_test_event(&completion, &queries())
            .await
            .unwrap();
        let content = output.commands[0].payload["content"].as_str().unwrap();
        assert!(content.starts_with("API permissions: 3 total"));
        assert!(content.contains("auth_status: 0=1, 1=2"));
        assert!(content.contains("[1] POST /guilds/{guild_id}/channels — 创建子频道"));
        assert!(content.contains("/api-permissions channels"));
        assert!(content.contains("/api-permissions all"));
        assert!(content.len() < 600);
    }

    #[tokio::test]
    async fn api_permission_completion_supports_channel_crud_and_all_views() {
        let result = json!({
            "raw": {
                "apis": [
                    {"path":"/channels/{channel_id}","method":"DELETE","desc":"删除子频道","auth_status":0},
                    {"path":"/guilds/{guild_id}/channels","method":"POST","desc":"创建子频道","auth_status":1},
                    {"path":"/guilds/{guild_id}","method":"GET","desc":"获取频道","auth_status":1}
                ]
            }
        });
        for (command_id, expected, omitted) in [
            (
                "api-permissions-channels",
                "[1] POST /guilds/{guild_id}/channels",
                "/guilds/{guild_id} — 获取频道",
            ),
            (
                "api-permissions-all",
                "[1] GET /guilds/{guild_id}",
                "never omitted",
            ),
        ] {
            let mut completion = event("ignored");
            completion.event_type = "action.completed".to_owned();
            completion.payload = serde_json::to_value(ActionCompleted {
                source_event_id: "source".to_owned(),
                source_invocation_id: "invocation".to_owned(),
                command_id: command_id.to_owned(),
                kind: API_PERMISSIONS_ACTION.to_owned(),
                status: ActionStatus::Succeeded,
                retryable: false,
                result: Some(result.clone()),
                error_code: None,
                error_message: None,
            })
            .unwrap();
            let output = DevToolsPlugin::default()
                .on_test_event(&completion, &queries())
                .await
                .unwrap();
            let content = output.commands[0].payload["content"].as_str().unwrap();
            assert!(content.contains(expected));
            assert!(!content.contains(omitted));
        }
    }

    #[tokio::test]
    async fn api_permission_replies_are_utf8_safely_bounded() {
        for (status, result, error_message) in [
            (
                ActionStatus::Succeeded,
                Some(json!({
                    "raw": {
                        "apis": [{
                            "path":"/guilds/{guild_id}/channels",
                            "method":"POST",
                            "desc":"界".repeat(2_000),
                            "auth_status":1
                        }]
                    }
                })),
                None,
            ),
            (ActionStatus::Failed, None, Some("界".repeat(2_000))),
        ] {
            let mut completion = event("ignored");
            completion.event_type = "action.completed".to_owned();
            completion.payload = serde_json::to_value(ActionCompleted {
                source_event_id: "source".to_owned(),
                source_invocation_id: "invocation".to_owned(),
                command_id: "api-permissions".to_owned(),
                kind: API_PERMISSIONS_ACTION.to_owned(),
                status,
                retryable: false,
                result,
                error_code: Some("platform_error".to_owned()),
                error_message,
            })
            .unwrap();
            let output = DevToolsPlugin::default()
                .on_test_event(&completion, &queries())
                .await
                .unwrap();
            let content = output.commands[0].payload["content"].as_str().unwrap();
            assert!(content.len() <= MAX_PERMISSION_REPLY_BYTES);
            assert!(content.ends_with("… response truncated"));
        }
    }

    #[test]
    fn manifest_declares_top_level_devtool_commands() {
        let plugin = DevToolsPlugin::default();
        let commands = plugin
            .manifest()
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(commands, ["whoami", "api-permissions"]);
    }
}
