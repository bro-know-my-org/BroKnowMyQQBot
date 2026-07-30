use super::{
    Disposition, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest,
    StaticPlugin, async_trait, is_command, message_plugin_manifest, message_text, reply,
};

#[derive(Debug)]
pub struct PingPlugin {
    manifest: PluginManifest,
}

impl Default for PingPlugin {
    fn default() -> Self {
        Self {
            manifest: message_plugin_manifest(
                "dev.bkm.ping",
                "Ping",
                "ping",
                "message.reply",
                false,
            ),
        }
    }
}

#[async_trait]
impl StaticPlugin for PingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if message_text(event).is_some_and(is_command("/ping")) {
            return Ok(reply(event, "pong", Vec::new()));
        }
        Ok(HandlerOutput {
            disposition: Disposition::Ignore,
            ..HandlerOutput::default()
        })
    }
}
