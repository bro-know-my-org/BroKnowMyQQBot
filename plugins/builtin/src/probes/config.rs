use super::{
    BTreeMap, Duration, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope,
    PluginManifest, StaticPlugin, Value, async_trait, ignored, is_command, json,
    message_plugin_manifest, message_text, reply, sleep,
};

#[derive(Debug)]
pub struct ConfigProbePlugin {
    manifest: PluginManifest,
    config_schema: Value,
}

impl Default for ConfigProbePlugin {
    fn default() -> Self {
        Self {
            manifest: message_plugin_manifest(
                "dev.bkm.config-probe",
                "Config Probe",
                "config-probe",
                "message.reply",
                false,
            ),
            config_schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "prefix": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 64
                    },
                    "delay_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 1000
                    },
                    "fail_init": { "type": "boolean" }
                },
                "required": ["prefix"],
                "additionalProperties": false
            }),
        }
    }
}

#[async_trait]
impl StaticPlugin for ConfigProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn config_schema(&self) -> Option<&Value> {
        Some(&self.config_schema)
    }

    async fn validate_config(&self, config: &BTreeMap<String, Value>) -> Result<(), PluginError> {
        let prefix = config
            .get("prefix")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidConfig("prefix must be a non-empty string".to_owned())
            })?;
        if prefix.is_empty() || prefix.chars().count() > 64 {
            return Err(PluginError::InvalidConfig(
                "prefix must contain between 1 and 64 characters".to_owned(),
            ));
        }
        if config
            .get("delay_ms")
            .and_then(Value::as_u64)
            .is_some_and(|delay| delay > 1_000)
        {
            return Err(PluginError::InvalidConfig(
                "delay_ms must not exceed 1000".to_owned(),
            ));
        }
        Ok(())
    }

    async fn init(&self, context: plugin_api::InitContext) -> Result<(), PluginError> {
        if context.config.get("fail_init").and_then(Value::as_bool) == Some(true) {
            return Err(PluginError::Permanent(
                "intentional config probe init failure".to_owned(),
            ));
        }
        Ok(())
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if !message_text(event).is_some_and(is_command("/config-probe")) {
            return Ok(ignored());
        }
        if let Some(delay) = queries.config_get("delay_ms").and_then(Value::as_u64) {
            sleep(Duration::from_millis(delay)).await;
        }
        let prefix = queries
            .config_get("prefix")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::Permanent("validated prefix is missing".to_owned()))?;
        Ok(reply(event, &format!("{prefix}: ok"), Vec::new()))
    }
}
