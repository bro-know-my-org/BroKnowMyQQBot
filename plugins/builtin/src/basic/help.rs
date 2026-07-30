use super::{
    CommandDeclaration, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope,
    PluginManifest, StaticPlugin, async_trait, command_declaration, ignored, is_command,
    message_plugin_manifest, message_text, reply,
};

#[derive(Debug)]
pub struct HelpPlugin {
    manifest: PluginManifest,
    help_text: String,
}

impl Default for HelpPlugin {
    fn default() -> Self {
        Self::with_commands([
            command_declaration("ping", "Run /ping"),
            command_declaration("count", "Run /count"),
            command_declaration("echo", "Run /echo <text>"),
        ])
    }
}

impl HelpPlugin {
    /// Builds a help plugin from the declarations of plugins actually loaded
    /// by the Host. Its own `/help` declaration is appended automatically.
    pub fn with_commands(commands: impl IntoIterator<Item = CommandDeclaration>) -> Self {
        let manifest =
            message_plugin_manifest("dev.bkm.help", "Help", "help", "message.reply", false);
        let mut commands = commands.into_iter().collect::<Vec<_>>();
        commands.extend(manifest.commands.iter().cloned());
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        commands.dedup_by(|left, right| left.name == right.name);
        let help_text = commands
            .into_iter()
            .map(|command| {
                let name = command.name.escape_debug();
                let description = command.description.escape_debug();
                if command.description.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} — {description}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            manifest,
            help_text: format!("commands:\n{help_text}"),
        }
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
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if message_text(event).is_some_and(is_command("/help")) {
            return Ok(reply(event, &self.help_text, Vec::new()));
        }
        Ok(ignored())
    }
}
