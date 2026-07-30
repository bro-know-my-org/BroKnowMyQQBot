use super::{
    ActionCompleted, BTreeSet, Disposition, HandlerOutput, HostQueries, PluginError,
    PluginEventEnvelope, PluginManifest, StateOp, StaticPlugin, Subscription, async_trait,
    hex_bytes, ignored, is_command, message_plugin_manifest, message_text, reply,
};

#[derive(Debug)]
pub struct ActionResultProbePlugin {
    manifest: PluginManifest,
}

impl Default for ActionResultProbePlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.action-result-probe",
            "Action Result Probe",
            "probe-action-result",
            "message.reply",
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
            && message_text(event).is_some_and(is_command("/probe-action-result"))
        {
            return Ok(reply(event, "action result probe dispatched", Vec::new()));
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
