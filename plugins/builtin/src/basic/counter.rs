use super::{
    Disposition, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest,
    StateOp, StaticPlugin, async_trait, is_command, message_plugin_manifest, message_text, reply,
};

const RECENT_EVENT_LIMIT: usize = 256;
const RECENT_EVENT_KEY: &str = "processed-recent";

#[derive(Debug)]
pub struct CounterPlugin {
    manifest: PluginManifest,
}

impl Default for CounterPlugin {
    fn default() -> Self {
        Self {
            manifest: message_plugin_manifest(
                "dev.bkm.counter",
                "Counter",
                "count",
                "message.reply",
                true,
            ),
        }
    }
}

#[async_trait]
impl StaticPlugin for CounterPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if !message_text(event).is_some_and(is_command("/count")) {
            return Ok(HandlerOutput {
                disposition: Disposition::Ignore,
                ..HandlerOutput::default()
            });
        }
        let recent_state = queries.state_get(RECENT_EVENT_KEY);
        let mut recent = recent_state.map_or_else(
            || Ok(Vec::<String>::new()),
            |state| {
                serde_json::from_slice::<Vec<String>>(&state.value).map_err(|error| {
                    PluginError::Permanent(format!("invalid processed-event state: {error}"))
                })
            },
        )?;
        if recent.iter().any(|event_id| event_id == &event.event_id) {
            return Ok(HandlerOutput {
                disposition: Disposition::Ignore,
                ..HandlerOutput::default()
            });
        }
        recent.push(event.event_id.clone());
        if recent.len() > RECENT_EVENT_LIMIT {
            recent.drain(..recent.len() - RECENT_EVENT_LIMIT);
        }
        let current = queries.state_get("counter").map_or(Ok(0), |state| {
            std::str::from_utf8(&state.value)
                .map_err(|error| {
                    PluginError::Permanent(format!("invalid counter state encoding: {error}"))
                })?
                .parse::<u64>()
                .map_err(|error| {
                    PluginError::Permanent(format!("invalid counter state value: {error}"))
                })
        })?;
        let next = current.checked_add(1).ok_or_else(|| {
            PluginError::Permanent("counter has reached its maximum value".to_owned())
        })?;
        let expected_revision = queries.state_get("counter").map(|state| state.revision);
        Ok(reply(
            event,
            &format!("count: {next}"),
            vec![
                StateOp::Put {
                    key: "counter".to_owned(),
                    value: next.to_string().into_bytes(),
                    expected_revision,
                },
                StateOp::Put {
                    key: RECENT_EVENT_KEY.to_owned(),
                    value: serde_json::to_vec(&recent)
                        .map_err(|error| PluginError::Permanent(error.to_string()))?,
                    expected_revision: recent_state.map(|state| state.revision),
                },
            ],
        ))
    }
}
