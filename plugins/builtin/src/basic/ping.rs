use super::{
    Disposition, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest,
    StaticPlugin, async_trait, is_command, message_plugin_manifest, reply,
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
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if is_command(queries.command_context().as_ref(), "ping") {
            return Ok(reply(event, "pong", Vec::new()));
        }
        Ok(HandlerOutput {
            disposition: Disposition::Ignore,
            ..HandlerOutput::default()
        })
    }
}
