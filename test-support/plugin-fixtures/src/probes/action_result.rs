use std::collections::BTreeSet;

use async_trait::async_trait;
use plugin_api::{
    ActionCompleted, Audience, CommandDeclaration, Disposition, HandlerOutput, HostQueries,
    PluginError, PluginEventEnvelope, PluginId, PluginManifest, PluginMetadata, PluginPermissions,
    PluginRuntimeConfig, StateOp, StaticPlugin, StoragePermission, Subscription,
};

#[derive(Debug)]
pub struct ActionResultProbePlugin {
    manifest: PluginManifest,
}

impl Default for ActionResultProbePlugin {
    fn default() -> Self {
        let mut manifest = probe_manifest(
            "dev.bkm.action-result-probe",
            "Action Result Probe",
            "probe-action-result",
            true,
        );
        manifest.subscriptions.push(Subscription {
            id: "action-results".to_owned(),
            event: "action.completed".to_owned(),
            priority: 0,
            scopes: BTreeSet::new(),
        });
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for ActionResultProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "message.created"
            && message_text(event).is_some_and(|text| text.trim() == "/probe-action-result")
        {
            return Ok(reply(event, "action result probe dispatched"));
        }
        if event.event_type != "action.completed" {
            return Ok(ignored());
        }
        let completion: ActionCompleted = serde_json::from_value(event.payload.clone())
            .map_err(|error| PluginError::Permanent(error.to_string()))?;
        let key = format!(
            "results/{}",
            hex_bytes(
                format!(
                    "{}/{}",
                    completion.source_invocation_id, completion.command_id
                )
                .as_bytes()
            )
        );
        let expected_revision = queries.state_get(&key).map(|state| state.revision);
        Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: vec![StateOp::Put {
                key,
                value: serde_json::to_vec(&completion)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
                expected_revision,
            }],
            commands: Vec::new(),
            diagnostics: Vec::new(),
        })
    }
}

pub(super) fn probe_manifest(id: &str, name: &str, command: &str, storage: bool) -> PluginManifest {
    PluginManifest {
        manifest_version: 1,
        id: PluginId::new(id).expect("test fixture plugin ID must be valid"),
        metadata: PluginMetadata::single_locale("en", name, format!("Test-only {name} plugin")),
        version: "0.1.0".to_owned(),
        protocol: ">=1.0,<2.0".to_owned(),
        state_version: 1,
        entry: "component.wasm".to_owned(),
        runtime: PluginRuntimeConfig::default(),
        subscriptions: vec![Subscription {
            id: "messages".to_owned(),
            event: "message.created".to_owned(),
            priority: 0,
            scopes: ["group", "private", "channel"]
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
        }],
        commands: vec![CommandDeclaration {
            name: command.to_owned(),
            aliases: Vec::new(),
            description: format!("Run /{command}"),
            audience: Audience::DevDiagnostic,
            subcommands: Vec::new(),
        }],
        permissions: PluginPermissions {
            actions: BTreeSet::from(["message.reply".to_owned()]),
            event_extensions: BTreeSet::new(),
            http: Vec::new(),
            browser: Vec::new(),
            storage: if storage {
                StoragePermission::Private
            } else {
                StoragePermission::None
            },
            storage_quota_bytes: if storage { 1024 * 1024 } else { 0 },
            scheduler: false,
            stop_propagation: false,
        },
    }
}

pub(super) fn ignored() -> HandlerOutput {
    HandlerOutput {
        disposition: Disposition::Ignore,
        ..HandlerOutput::default()
    }
}

pub(super) fn message_text(event: &PluginEventEnvelope) -> Option<&str> {
    event
        .payload
        .pointer("/data/text")
        .and_then(serde_json::Value::as_str)
}

pub(super) fn reply(event: &PluginEventEnvelope, content: &str) -> HandlerOutput {
    HandlerOutput {
        disposition: Disposition::Continue,
        commands: vec![plugin_api::PluginCommand {
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            idempotency_key: Some(format!("{}/reply", event.event_id)),
            deadline_ms: Some(5_000),
            payload: serde_json::json!({"content": content}),
        }],
        ..HandlerOutput::default()
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
