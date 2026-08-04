use super::{
    ActionCompleted, ActionStatus, BTreeMap, BTreeSet, BrowserPermission, BrowserRun,
    BrowserRunResult, BrowserScreenshotFormat, BrowserStep, BrowserViewport, BrowserWaitUntil,
    Disposition, HandlerOutput, HostQueries, MediaReply, PluginCommand, PluginError,
    PluginEventEnvelope, PluginManifest, StaticPlugin, Subscription, async_trait, command_argument,
    ignored, message_plugin_manifest, message_text,
};

#[derive(Debug)]
pub struct BrowserProbePlugin {
    manifest: PluginManifest,
}

impl Default for BrowserProbePlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.browser-probe",
            "Browser Probe",
            "screenshot",
            "media.reply",
            false,
        );
        ">=1.1,<2.0".clone_into(&mut manifest.protocol);
        manifest.subscriptions.push(Subscription {
            id: "browser-results".to_owned(),
            event: "action.completed".to_owned(),
            priority: 0,
            scopes: BTreeSet::new(),
        });
        manifest.permissions.browser.push(BrowserPermission {
            scheme: "https".to_owned(),
            host: "example.com".to_owned(),
            port: 443,
            path_prefixes: BTreeSet::from(["/".to_owned()]),
            capabilities: BTreeSet::from(["navigate".to_owned(), "screenshot".to_owned()]),
        });
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for BrowserProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "message.created" {
            let Some(url) =
                message_text(event).and_then(|text| command_argument(text, "/screenshot"))
            else {
                return Ok(ignored());
            };
            let request = BrowserRun {
                steps: vec![
                    BrowserStep::Navigate {
                        url: url.to_owned(),
                        wait_until: BrowserWaitUntil::DomContentLoaded,
                        timeout_ms: 10_000,
                    },
                    BrowserStep::Wait { duration_ms: 3_000 },
                    BrowserStep::Screenshot {
                        selector: None,
                        full_page: true,
                        format: BrowserScreenshotFormat::Png,
                        quality: None,
                    },
                ],
                viewport: BrowserViewport::default(),
                user_agent: None,
                locale: Some("zh-CN".to_owned()),
                timezone: Some("Asia/Shanghai".to_owned()),
                color_scheme: None,
                extra_headers: BTreeMap::new(),
            };
            return Ok(HandlerOutput {
                disposition: Disposition::Continue,
                state_ops: Vec::new(),
                commands: vec![PluginCommand {
                    command_id: "browser".to_owned(),
                    kind: "browser.run".to_owned(),
                    idempotency_key: Some(format!("{}/browser", event.event_id)),
                    deadline_ms: Some(30_000),
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
        if completion.kind != "browser.run" || completion.status != ActionStatus::Succeeded {
            return Ok(ignored());
        }
        let result: BrowserRunResult = serde_json::from_value(
            completion
                .result
                .ok_or_else(|| PluginError::Permanent("browser result is missing".to_owned()))?,
        )
        .map_err(|error| PluginError::Permanent(error.to_string()))?;
        let asset = result
            .assets
            .first()
            .ok_or_else(|| PluginError::Permanent("browser screenshot is missing".to_owned()))?;
        Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: Vec::new(),
            commands: vec![PluginCommand {
                command_id: "reply-screenshot".to_owned(),
                kind: "media.reply".to_owned(),
                idempotency_key: Some(format!("{}/media", completion.source_event_id)),
                deadline_ms: Some(15_000),
                payload: serde_json::to_value(MediaReply {
                    asset_id: asset.asset_id.clone(),
                    caption: None,
                    consume: true,
                })
                .map_err(|error| PluginError::Permanent(error.to_string()))?,
            }],
            diagnostics: Vec::new(),
        })
    }
}
