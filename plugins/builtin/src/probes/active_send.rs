use super::{
    Disposition, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest,
    StaticPlugin, async_trait, command_argument, ignored, json, message_plugin_manifest,
    message_text, plugin_command,
};

/// Explicit functional probe for platform proactive-message authorization.
#[derive(Debug)]
pub struct ActiveSendProbePlugin {
    manifest: PluginManifest,
}

impl Default for ActiveSendProbePlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.active-send-probe",
            "Active Send Probe",
            "active-send",
            "message.send",
            false,
        );
        manifest
            .permissions
            .actions
            .insert("message.reply".to_owned());
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for ActiveSendProbePlugin {
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
        let Some(content) = command_argument(text, "/active-send") else {
            return Ok(ignored());
        };
        let target = event
            .payload
            .pointer("/data/target")
            .cloned()
            .ok_or_else(|| PluginError::Permanent("message target is missing".to_owned()))?;
        Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: Vec::new(),
            commands: vec![
                plugin_command(
                    event,
                    "confirm",
                    "message.reply",
                    json!({"content":"attempting proactive message"}),
                ),
                plugin_command(
                    event,
                    "send",
                    "message.send",
                    json!({"target":target,"content":content}),
                ),
            ],
            diagnostics: Vec::new(),
        })
    }
}
