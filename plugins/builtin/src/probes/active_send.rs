use super::{
    Disposition, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest,
    StaticPlugin, async_trait, command_argument, diagnostic_authorized, diagnostic_plugin_manifest,
    ignored, json, plugin_command, restricted,
};

/// Explicit functional probe for platform proactive-message authorization.
#[derive(Debug)]
pub struct ActiveSendProbePlugin {
    manifest: PluginManifest,
}

impl Default for ActiveSendProbePlugin {
    fn default() -> Self {
        let mut manifest = diagnostic_plugin_manifest(
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
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let command = queries.command_context();
        let Some(content) = command_argument(command.as_ref(), "active-send") else {
            return Ok(ignored());
        };
        if !diagnostic_authorized(queries) {
            return Ok(restricted(event));
        }
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
