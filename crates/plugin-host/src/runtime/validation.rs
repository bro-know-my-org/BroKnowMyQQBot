//! Validation, capability, event-extension, and output policy.

use super::{
    Arc, BTreeMap, BTreeSet, Context, Disposition, Event, ExtensionPayload, HandlerOutput, HashSet,
    MAX_COMMANDS, MAX_CONFIG_SCHEMA_BYTES, MAX_EVENT_EXTENSION_BYTES, MAX_OUTPUT_BYTES,
    MAX_STATE_OPS, PluginHostError, PluginManifest, RegisteredPlugin, StaticPlugin, Value,
};

pub(super) fn event_extensions(
    plugin: &RegisteredPlugin,
    context: &Context,
) -> Result<Vec<ExtensionPayload>, PluginHostError> {
    let namespace = format!("{}.raw", context.platform());
    let capability = format!("event.extension.{namespace}");
    if !plugin
        .manifest
        .permissions
        .event_extensions
        .contains(&namespace)
        || !plugin.granted_capabilities.contains(&capability)
    {
        return Ok(Vec::new());
    }
    let mut sanitized = context.raw_event().clone();
    redact_sensitive_fields(&mut sanitized);
    let data = serde_json::to_vec(&sanitized).map_err(|error| PluginHostError::Invocation {
        plugin_id: plugin.manifest.id.to_string(),
        message: format!("failed to encode `{namespace}` extension: {error}"),
    })?;
    if data.len() > MAX_EVENT_EXTENSION_BYTES {
        return Err(PluginHostError::Invocation {
            plugin_id: plugin.manifest.id.to_string(),
            message: format!(
                "`{namespace}` extension size {} exceeds the Host limit {MAX_EVENT_EXTENSION_BYTES}",
                data.len()
            ),
        });
    }
    Ok(vec![ExtensionPayload {
        namespace,
        schema_version: "1.0".to_owned(),
        content_type: "application/json".to_owned(),
        data,
    }])
}

pub(super) fn redact_sensitive_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if is_sensitive_field(key) {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_sensitive_fields(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_sensitive_fields(value);
            }
        }
        _ => {}
    }
}

fn is_sensitive_field(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "accesstoken"
            | "apikey"
            | "authorization"
            | "bottoken"
            | "clientsecret"
            | "cookie"
            | "password"
            | "secret"
            | "setcookie"
            | "signature"
            | "token"
            | "xapikey"
    )
}

pub(super) fn validate_config_schema(
    plugin: &dyn StaticPlugin,
    plugin_id: &str,
    config: &BTreeMap<String, Value>,
) -> Result<(), PluginHostError> {
    let Some(schema) = plugin.config_schema() else {
        return Ok(());
    };
    let schema_bytes =
        serde_json::to_vec(schema).map_err(|error| PluginHostError::InvalidConfigSchema {
            plugin_id: plugin_id.to_owned(),
            message: error.to_string(),
        })?;
    if schema_bytes.len() > MAX_CONFIG_SCHEMA_BYTES {
        return Err(PluginHostError::InvalidConfigSchema {
            plugin_id: plugin_id.to_owned(),
            message: format!(
                "schema size {} exceeds the Host limit {MAX_CONFIG_SCHEMA_BYTES}",
                schema_bytes.len()
            ),
        });
    }
    if let Some(reference) = external_schema_reference(schema) {
        return Err(PluginHostError::InvalidConfigSchema {
            plugin_id: plugin_id.to_owned(),
            message: format!("external schema reference `{reference}` is not allowed"),
        });
    }
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        PluginHostError::InvalidConfigSchema {
            plugin_id: plugin_id.to_owned(),
            message: error.to_string(),
        }
    })?;
    let instance = Value::Object(config.clone().into_iter().collect());
    validator
        .validate(&instance)
        .map_err(|error| PluginHostError::InvalidConfig {
            plugin_id: plugin_id.to_owned(),
            message: format!("JSON Schema validation failed: {error}"),
        })
}

fn external_schema_reference(schema: &Value) -> Option<String> {
    let mut pending = vec![schema];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
                    if let Some(reference) = object.get(keyword).and_then(Value::as_str) {
                        if !reference.starts_with('#') {
                            return Some(reference.to_owned());
                        }
                    }
                }
                pending.extend(object.values());
            }
            Value::Array(values) => pending.extend(values),
            _ => {}
        }
    }
    None
}
pub(super) fn validate_upgrade_commands(
    plugins: &[Arc<RegisteredPlugin>],
    replaced_index: usize,
    manifest: &PluginManifest,
) -> Result<(), PluginHostError> {
    let existing = plugins
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != replaced_index)
        .flat_map(|(_, plugin)| {
            plugin
                .manifest
                .commands
                .iter()
                .flat_map(|command| std::iter::once(&command.name).chain(&command.aliases))
        })
        .collect::<BTreeSet<_>>();
    for command in &manifest.commands {
        for name in std::iter::once(&command.name).chain(&command.aliases) {
            if existing.contains(name) {
                return Err(PluginHostError::DuplicateCommand(name.clone()));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_output(
    plugin: &RegisteredPlugin,
    output: &HandlerOutput,
) -> Result<(), PluginHostError> {
    let invalid = |message: String| PluginHostError::InvalidOutput {
        plugin_id: plugin.manifest.id.to_string(),
        message,
    };
    if output.disposition == Disposition::Ignore
        && (!output.state_ops.is_empty() || !output.commands.is_empty())
    {
        return Err(invalid(
            "Ignore disposition cannot contain state operations or commands".to_owned(),
        ));
    }
    if output.disposition == Disposition::Stop
        && !plugin
            .granted_capabilities
            .contains("event.stop_propagation")
    {
        return Err(invalid("event.stop_propagation is not granted".to_owned()));
    }
    if output.state_ops.len() > MAX_STATE_OPS {
        return Err(invalid("too many state operations".to_owned()));
    }
    if !output.state_ops.is_empty() && !plugin.granted_capabilities.contains("storage.private") {
        return Err(invalid("storage.private is not granted".to_owned()));
    }
    if output.commands.len() > MAX_COMMANDS {
        return Err(invalid("too many commands".to_owned()));
    }
    let mut command_ids = HashSet::new();
    for command in &output.commands {
        if !matches!(
            command.kind.as_str(),
            "message.reply"
                | "message.send"
                | "media.reply"
                | "media.send"
                | "http.request"
                | "browser.run"
                | "schedule.create"
                | "schedule.cancel"
        ) {
            return Err(invalid(format!(
                "command kind `{}` is not part of the supported BPP baseline",
                command.kind
            )));
        }
        if command.command_id.is_empty()
            || command.command_id.len() > 128
            || !command_ids.insert(&command.command_id)
        {
            return Err(invalid(format!(
                "invalid or duplicate command ID `{}`",
                command.command_id
            )));
        }
        if !plugin.granted_capabilities.contains(&command.kind) {
            return Err(invalid(format!(
                "capability `{}` is not granted",
                command.kind
            )));
        }
        if command
            .deadline_ms
            .is_some_and(|deadline| deadline == 0 || deadline > super::MAX_COMMAND_DEADLINE_MS)
        {
            return Err(invalid(format!(
                "command `{}` deadline must be between 1 and {} milliseconds",
                command.command_id,
                super::MAX_COMMAND_DEADLINE_MS
            )));
        }
    }
    let output_size = serde_json::to_vec(output)
        .map_err(|error| invalid(error.to_string()))?
        .len();
    if output_size > MAX_OUTPUT_BYTES {
        return Err(invalid("output exceeds one MiB".to_owned()));
    }
    Ok(())
}
pub(super) fn requested_capabilities(manifest: &PluginManifest) -> BTreeSet<String> {
    manifest.requested_capabilities()
}

pub(super) const fn event_type(event: &Event) -> &'static str {
    match event {
        Event::Message(_) => "message.created",
        Event::Notice(_) => "notice.received",
        Event::Request(_) => "request.received",
        Event::Lifecycle(_) => "lifecycle.changed",
        Event::Platform { .. } => "platform.event",
    }
}

pub(super) fn event_scope(event: &Event) -> Option<&'static str> {
    let Event::Message(message) = event else {
        return None;
    };
    Some(match message.scope() {
        bot_core::MessageScope::Group => "group",
        bot_core::MessageScope::Private => "private",
        bot_core::MessageScope::Channel => "channel",
    })
}
