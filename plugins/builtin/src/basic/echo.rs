use super::{
    HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin,
    async_trait, command_argument, ignored, message_plugin_manifest, message_text, reply,
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
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let Some(text) = message_text(event) else {
            return Ok(ignored());
        };
        let Some(content) = command_argument(text, "/echo") else {
            return Ok(ignored());
        };
        Ok(reply(event, content, Vec::new()))
    }
}
