use super::{
    HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin,
    Value, async_trait, ignored, is_command, message_plugin_manifest, message_text, reply,
};

#[derive(Debug)]
pub struct QqExtensionProbePlugin {
    manifest: PluginManifest,
}

impl Default for QqExtensionProbePlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.qq-extension-probe",
            "QQ Extension Probe",
            "qq-extension",
            "message.reply",
            false,
        );
        manifest
            .permissions
            .event_extensions
            .insert("qq.official.raw".to_owned());
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for QqExtensionProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if !message_text(event).is_some_and(is_command("/qq-extension")) {
            return Ok(ignored());
        }
        let event_type = event
            .extensions
            .iter()
            .find(|extension| extension.namespace == "qq.official.raw")
            .map(|extension| serde_json::from_slice::<Value>(&extension.data))
            .transpose()
            .map_err(|error| PluginError::Permanent(error.to_string()))?
            .and_then(|raw| raw.get("t").and_then(Value::as_str).map(ToOwned::to_owned));
        let content = event_type.map_or_else(
            || "qq extension unavailable".to_owned(),
            |event_type| format!("qq extension: {event_type}"),
        );
        Ok(reply(event, &content, Vec::new()))
    }
}
