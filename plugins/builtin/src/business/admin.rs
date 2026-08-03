use super::super::{
    BTreeMap, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest,
    StateOp, StaticPlugin, Value, async_trait, command_tail, hex_bytes, ignored, json,
    message_plugin_manifest, message_text, reply,
};
use sha2::{Digest as _, Sha256};

const ADMIN_PREFIX: &str = "admins/";
const AUDIT_KEY: &str = "audit/recent";
const MAX_ADMINS: usize = 256;
const MAX_AUDIT_RECORDS: usize = 256;

#[derive(Debug)]
pub struct AdminPlugin {
    manifest: PluginManifest,
    config_schema: Value,
}

impl Default for AdminPlugin {
    fn default() -> Self {
        let manifest = message_plugin_manifest(
            "dev.bkm.admin",
            "Administration",
            "admin",
            "message.reply",
            true,
        );
        Self {
            manifest,
            config_schema: json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{
                    "owners":{
                        "type":"array",
                        "minItems":1,
                        "maxItems":32,
                        "uniqueItems":true,
                        "items":{"type":"string","minLength":1,"maxLength":124}
                    }
                },
                "required":["owners"],
                "additionalProperties":false
            }),
        }
    }
}

#[async_trait]
impl StaticPlugin for AdminPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn config_schema(&self) -> Option<&Value> {
        Some(&self.config_schema)
    }

    async fn validate_config(&self, config: &BTreeMap<String, Value>) -> Result<(), PluginError> {
        let owners = config
            .get("owners")
            .and_then(Value::as_array)
            .ok_or_else(|| PluginError::InvalidConfig("owners must be an array".to_owned()))?;
        if owners.is_empty()
            || owners.len() > 32
            || owners
                .iter()
                .any(|owner| owner.as_str().is_none_or(|owner| !valid_identity(owner)))
        {
            return Err(PluginError::InvalidConfig(
                "owners must contain 1..32 unique non-empty sender IDs".to_owned(),
            ));
        }
        Ok(())
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let Some(text) = message_text(event) else {
            return Ok(ignored());
        };
        let Some(arguments) = command_tail(text, "/admin") else {
            return Ok(ignored());
        };
        let sender = sender_id(event)?;
        if !valid_identity(sender) {
            return Err(PluginError::Permanent(
                "message sender ID is invalid".to_owned(),
            ));
        }
        let authorized = is_admin(queries, sender);
        if arguments == "whoami" {
            return Ok(reply(
                event,
                if authorized {
                    "administrator"
                } else {
                    "not an administrator"
                },
                Vec::new(),
            ));
        }
        if !authorized {
            return Ok(reply(
                event,
                "administrator permission required",
                Vec::new(),
            ));
        }
        if arguments == "list" {
            return Ok(reply(event, &admin_list(queries), Vec::new()));
        }
        let mut parts = arguments.split_whitespace();
        let operation = parts.next();
        let subject = parts.next();
        if parts.next().is_some() || subject.is_none_or(|value| !valid_identity(value)) {
            return Ok(usage(event));
        }
        let subject = subject.expect("validated subject is present");
        if already_audited(queries, &event.event_id)? {
            return Ok(ignored());
        }
        match operation {
            Some("grant") => grant(event, queries, sender, subject),
            Some("revoke") => revoke(event, queries, sender, subject),
            _ => Ok(usage(event)),
        }
    }
}

fn already_audited(queries: &dyn HostQueries, event_id: &str) -> Result<bool, PluginError> {
    let Some(state) = queries.state_get(AUDIT_KEY) else {
        return Ok(false);
    };
    let records: Vec<Value> = serde_json::from_slice(&state.value).map_err(|error| {
        PluginError::Permanent(format!("invalid administration audit state: {error}"))
    })?;
    let event_id = audit_event_id(event_id);
    Ok(records
        .iter()
        .any(|record| record.get("event_id").and_then(Value::as_str) == Some(event_id.as_str())))
}

fn grant(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    actor: &str,
    subject: &str,
) -> Result<HandlerOutput, PluginError> {
    if is_admin(queries, subject) {
        return Ok(reply(
            event,
            "sender is already an administrator",
            Vec::new(),
        ));
    }
    if queries.state_scan(ADMIN_PREFIX, MAX_ADMINS + 1).len() >= MAX_ADMINS {
        return Ok(reply(event, "administrator limit reached", Vec::new()));
    }
    let mut operations = vec![StateOp::Put {
        key: admin_key(subject),
        value: subject.as_bytes().to_vec(),
        expected_revision: None,
    }];
    operations.push(audit_operation(event, queries, actor, "grant", subject)?);
    Ok(reply(event, "administrator granted", operations))
}

fn revoke(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    actor: &str,
    subject: &str,
) -> Result<HandlerOutput, PluginError> {
    if configured_owner(queries, subject) {
        return Ok(reply(
            event,
            "configured owners cannot be revoked",
            Vec::new(),
        ));
    }
    let key = admin_key(subject);
    let Some(state) = queries.state_get(&key) else {
        return Ok(reply(
            event,
            "sender is not a delegated administrator",
            Vec::new(),
        ));
    };
    let operations = vec![
        StateOp::Delete {
            key,
            expected_revision: Some(state.revision),
        },
        audit_operation(event, queries, actor, "revoke", subject)?,
    ];
    Ok(reply(event, "administrator revoked", operations))
}

fn audit_operation(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    actor: &str,
    action: &str,
    subject: &str,
) -> Result<StateOp, PluginError> {
    let current = queries.state_get(AUDIT_KEY);
    let mut records = current.map_or_else(
        || Ok(Vec::<Value>::new()),
        |state| {
            serde_json::from_slice(&state.value).map_err(|error| {
                PluginError::Permanent(format!("invalid administration audit state: {error}"))
            })
        },
    )?;
    let event_id = audit_event_id(&event.event_id);
    if records
        .iter()
        .any(|record| record.get("event_id").and_then(Value::as_str) == Some(event_id.as_str()))
    {
        return Err(PluginError::Permanent(
            "administration event was already applied".to_owned(),
        ));
    }
    records.push(json!({
        "event_id":event_id,
        "actor":actor,
        "action":action,
        "subject":subject,
        "at_ms":event.received_at_ms
    }));
    if records.len() > MAX_AUDIT_RECORDS {
        records.drain(..records.len() - MAX_AUDIT_RECORDS);
    }
    Ok(StateOp::Put {
        key: AUDIT_KEY.to_owned(),
        value: serde_json::to_vec(&records)
            .map_err(|error| PluginError::Permanent(error.to_string()))?,
        expected_revision: current.map(|state| state.revision),
    })
}

fn admin_list(queries: &dyn HostQueries) -> String {
    let mut admins = configured_owners(queries);
    admins.extend(
        queries
            .state_scan(ADMIN_PREFIX, MAX_ADMINS)
            .into_iter()
            .filter_map(|(_, value)| String::from_utf8(value.value.clone()).ok()),
    );
    admins.sort();
    admins.dedup();
    format!("administrators: {}", admins.join(", "))
}

fn configured_owners(queries: &dyn HostQueries) -> Vec<String> {
    queries
        .config_get("owners")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn configured_owner(queries: &dyn HostQueries, sender: &str) -> bool {
    configured_owners(queries)
        .iter()
        .any(|owner| owner == sender)
}

fn is_admin(queries: &dyn HostQueries, sender: &str) -> bool {
    configured_owner(queries, sender) || queries.state_get(&admin_key(sender)).is_some()
}

fn sender_id(event: &PluginEventEnvelope) -> Result<&str, PluginError> {
    event
        .payload
        .pointer("/data/sender/id")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Permanent("message sender is missing".to_owned()))
}

fn admin_key(sender: &str) -> String {
    format!("{ADMIN_PREFIX}{}", hex_bytes(sender.as_bytes()))
}

fn audit_event_id(event_id: &str) -> String {
    hex_bytes(&Sha256::digest(event_id.as_bytes()))
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 124 && !value.chars().any(char::is_control)
}

fn usage(event: &PluginEventEnvelope) -> HandlerOutput {
    reply(
        event,
        "usage: /admin whoami|list|grant <sender-id>|revoke <sender-id>",
        Vec::new(),
    )
}
