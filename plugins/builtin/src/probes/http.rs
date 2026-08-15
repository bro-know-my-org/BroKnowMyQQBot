use super::{
    ActionCompleted, ActionStatus, BTreeMap, BTreeSet, Disposition, HandlerOutput, HostQueries,
    HttpPermission, HttpRequest, HttpResponse, PluginCommand, PluginError, PluginEventEnvelope,
    PluginManifest, StateOp, StaticPlugin, Subscription, Value, async_trait, ignored, is_command,
    json, message_plugin_manifest, message_text, reply,
};

#[derive(Debug)]
pub struct HttpProbePlugin {
    manifest: PluginManifest,
}

impl Default for HttpProbePlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.http-probe",
            "HTTP Probe",
            "http-probe",
            "message.reply",
            true,
        );
        manifest.subscriptions.push(Subscription {
            id: "http-results".to_owned(),
            event: "action.completed".to_owned(),
            priority: 0,
            scopes: BTreeSet::new(),
        });
        manifest.permissions.http.push(HttpPermission {
            host: "example.com".to_owned(),
            port: 443,
            methods: BTreeSet::from(["GET".to_owned()]),
            path_prefixes: BTreeSet::from(["/".to_owned()]),
            credential: None,
        });
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for HttpProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "message.created"
            && message_text(event).is_some_and(is_command("/http-probe"))
        {
            let url = queries
                .config_get("url")
                .and_then(Value::as_str)
                .unwrap_or("https://example.com/");
            let request = HttpRequest {
                method: "GET".to_owned(),
                url: url.to_owned(),
                headers: BTreeMap::new(),
                body: None,
                timeout_ms: 5_000,
                max_response_bytes: 64 * 1024,
            };
            return Ok(HandlerOutput {
                disposition: Disposition::Continue,
                state_ops: Vec::new(),
                commands: vec![PluginCommand {
                    command_id: "http".to_owned(),
                    kind: "http.request".to_owned(),
                    idempotency_key: Some(format!("{}/http", event.event_id)),
                    deadline_ms: Some(10_000),
                    payload: serde_json::to_value(request)
                        .map_err(|error| PluginError::Permanent(error.to_string()))?,
                }],
                diagnostics: Vec::new(),
            });
        }
        if event.event_type != "action.completed" {
            return Ok(ignored());
        }
        let completion: ActionCompleted = serde_json::from_value(event.payload.clone())
            .map_err(|error| PluginError::Permanent(error.to_string()))?;
        if completion.kind != "http.request" {
            return Ok(ignored());
        }
        let content =
            if completion.status == ActionStatus::Succeeded {
                let response: HttpResponse =
                    serde_json::from_value(completion.result.clone().ok_or_else(|| {
                        PluginError::Permanent("HTTP result is missing".to_owned())
                    })?)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
                format!("HTTP probe succeeded: {}", response.status)
            } else {
                format!(
                    "HTTP probe {:?}: {}",
                    completion.status,
                    completion.error_code.as_deref().unwrap_or("unknown")
                )
            };
        let summary = json!({
            "status": completion.status,
            "error_code": completion.error_code,
        });
        Ok(reply(
            event,
            &content,
            vec![StateOp::Put {
                key: "results/http".to_owned(),
                value: serde_json::to_vec(&summary)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
                expected_revision: None,
            }],
        ))
    }
}
