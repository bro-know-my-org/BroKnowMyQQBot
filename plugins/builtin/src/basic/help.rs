use std::fmt::Write as _;

use super::{
    Audience, CallerAuthority, CallerRole, CommandDeclaration, HandlerOutput, HostQueries,
    PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin, async_trait,
    command_declaration, ignored, is_command, message_plugin_manifest, reply,
};

const MAX_HELP_COLUMN_WIDTH: usize = 32;
pub(super) const MAX_HELP_OUTPUT_BYTES: usize = 4_096;
const HELP_TRUNCATION_RESERVE_BYTES: usize = 64;

#[derive(Debug, Clone)]
struct HelpEntry {
    invocation: String,
    description: String,
    audience: Audience,
}

#[derive(Debug)]
pub struct HelpPlugin {
    manifest: PluginManifest,
    entries: Vec<HelpEntry>,
}

impl Default for HelpPlugin {
    fn default() -> Self {
        Self::with_commands([
            command_declaration("ping", "测试机器人"),
            command_declaration("count", "查看计数"),
            command_declaration("echo", "原样回显"),
        ])
    }
}

impl HelpPlugin {
    /// Builds a help plugin from declarations of Ready plugins. Aliases are
    /// intentionally absent because Manifest primary names are canonical.
    pub fn with_commands(commands: impl IntoIterator<Item = CommandDeclaration>) -> Self {
        let manifest =
            message_plugin_manifest("dev.bkm.help", "Help", "help", "message.reply", false);
        let mut commands = commands.into_iter().collect::<Vec<_>>();
        commands.extend(manifest.commands.iter().cloned());
        let mut entries = commands
            .into_iter()
            .flat_map(|command| {
                if command.subcommands.is_empty() {
                    vec![HelpEntry {
                        invocation: command.name.escape_debug().to_string(),
                        description: command.description.escape_debug().to_string(),
                        audience: command.audience,
                    }]
                } else {
                    // A declaration with children is a command namespace in
                    // the user-facing catalog; list only concrete children.
                    command
                        .subcommands
                        .into_iter()
                        .map(|subcommand| HelpEntry {
                            invocation: format!(
                                "{} {}",
                                command.name.escape_debug(),
                                subcommand.name.escape_debug()
                            ),
                            description: subcommand.description.escape_debug().to_string(),
                            audience: subcommand.audience,
                        })
                        .collect()
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.invocation.cmp(&right.invocation));
        entries.dedup_by(|left, right| left.invocation == right.invocation);
        Self { manifest, entries }
    }

    fn format_help(&self, caller: &CallerAuthority, prefix: &str) -> String {
        let entries = self
            .entries
            .iter()
            .filter(|entry| visible_to(entry.audience, caller.role))
            .map(|entry| HelpEntry {
                invocation: format!("{prefix}{}", entry.invocation),
                ..entry.clone()
            })
            .collect::<Vec<_>>();
        let visible = entries.iter().collect::<Vec<_>>();
        let width = visible
            .iter()
            .map(|entry| entry.invocation.chars().count())
            .max()
            .unwrap_or_default()
            .min(MAX_HELP_COLUMN_WIDTH);
        let (available, elevated): (Vec<_>, Vec<_>) = visible
            .into_iter()
            .partition(|entry| permitted(entry.audience, caller.role));

        let mut output = format!(
            "当前身份：{}\n当前平台：{}\n\n可用命令",
            role_name(caller.role),
            bounded_platform_name(&caller.platform)
        );
        let mut omitted = append_entries(&mut output, &available, width);
        if !elevated.is_empty() {
            output.push_str("\n\n需要更高权限");
            omitted += append_entries(&mut output, &elevated, width);
        }
        if omitted > 0 {
            write!(output, "\n\n另有 {omitted} 条命令未显示")
                .expect("writing to a String cannot fail");
        }
        truncate_utf8(&mut output, MAX_HELP_OUTPUT_BYTES);
        output
    }
}

#[async_trait]
impl StaticPlugin for HelpPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if is_command(queries.command_context().as_ref(), "help") {
            let current = queries.command_declarations().map(Self::with_commands);
            let catalog = current.as_ref().unwrap_or(self);
            return Ok(reply(
                event,
                &catalog.format_help(&queries.caller_authority(), &queries.command_prefix()),
                Vec::new(),
            ));
        }
        Ok(ignored())
    }
}

fn append_entries(output: &mut String, entries: &[&HelpEntry], width: usize) -> usize {
    let mut omitted = 0;
    for entry in entries {
        let padding = width.saturating_sub(entry.invocation.chars().count()) + 2;
        let audience = audience_name(entry.audience);
        let description_bytes = if entry.description.is_empty() {
            0
        } else {
            1 + entry.description.len()
        };
        let line_bytes =
            1 + entry.invocation.len() + padding + 2 + audience.len() + description_bytes;
        if output
            .len()
            .saturating_add(line_bytes)
            .saturating_add(HELP_TRUNCATION_RESERVE_BYTES)
            <= MAX_HELP_OUTPUT_BYTES
        {
            write!(output, "\n{}{:padding$}[{audience}]", entry.invocation, "")
                .expect("writing to a String cannot fail");
            if !entry.description.is_empty() {
                output.push(' ');
                output.push_str(&entry.description);
            }
        } else {
            omitted += 1;
        }
    }
    omitted
}

const fn audience_name(audience: Audience) -> &'static str {
    match audience {
        Audience::Public => "公开",
        Audience::Admin => "管理员",
        Audience::Owner => "所有者",
        Audience::DevDiagnostic => "开发诊断",
        Audience::Hidden => "隐藏",
    }
}

const fn permitted(audience: Audience, role: CallerRole) -> bool {
    match audience {
        Audience::Public => true,
        Audience::Admin => matches!(role, CallerRole::Admin | CallerRole::Owner),
        Audience::Owner => matches!(role, CallerRole::Owner),
        Audience::DevDiagnostic => matches!(role, CallerRole::Admin | CallerRole::Owner),
        Audience::Hidden => false,
    }
}

const fn visible_to(audience: Audience, role: CallerRole) -> bool {
    match audience {
        Audience::Hidden => false,
        Audience::DevDiagnostic => matches!(role, CallerRole::Admin | CallerRole::Owner),
        _ => true,
    }
}

const fn role_name(role: CallerRole) -> &'static str {
    match role {
        CallerRole::User => "普通用户",
        CallerRole::Admin => "管理员",
        CallerRole::Owner => "所有者",
    }
}

fn platform_name(platform: &str) -> &str {
    match platform {
        "qq.official" => "QQ 官方",
        "onebot11" => "OneBot 11",
        "unknown" | "" => "未知平台",
        other => other,
    }
}

fn bounded_platform_name(platform: &str) -> String {
    const MAX_PLATFORM_BYTES: usize = 64;
    let mut escaped = platform_name(platform).escape_debug().to_string();
    truncate_utf8(&mut escaped, MAX_PLATFORM_BYTES);
    escaped
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}
