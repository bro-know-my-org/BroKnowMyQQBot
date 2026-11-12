use super::{
    HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin,
    async_trait, command_argument, ignored, message_plugin_manifest, reply,
};

#[derive(Debug)]
pub struct EchoPlugin {
    manifest: PluginManifest,
}

impl Default for EchoPlugin {
    fn default() -> Self {
        Self {
            manifest: message_plugin_manifest(
                "dev.bkm.echo",
                "Echo",
                "echo",
                "message.reply",
                false,
            ),
        }
    }
}

#[async_trait]
impl StaticPlugin for EchoPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let command = queries.command_context();
        let Some(content) = command_argument(command.as_ref(), "echo") else {
            return Ok(ignored());
        };
        Ok(reply(event, content, Vec::new()))
    }
}
