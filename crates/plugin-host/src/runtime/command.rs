//! Host command execution and action-completion mapping.

use super::{
    Action, ActionCompleted, ActionStatus, Adapter, AdapterError, Arc, AssetDigest, AssetStore,
    BTreeMap, BTreeSet, BrowserExecutionError, BrowserExecutor, BrowserRun, BrowserStep,
    ContextError, ExecutionOrigin, HttpExecutionError, HttpExecutor, HttpRequest, MediaReply,
    MediaSend, PluginCommand, PluginError, PluginManifest, SendMessageAction, Value,
};
use bot_core::{MediaAttachment, ReplyMediaAction, SendMediaAction};
use plugin_api::{PluginMessageTarget, url_path_matches_prefix};
use reqwest::header::{HeaderName, HeaderValue};

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn execute_context_command(
    origin: ExecutionOrigin<'_>,
    command: &PluginCommand,
    http_executor: &dyn HttpExecutor,
    browser_executor: &dyn BrowserExecutor,
    assets: &AssetStore,
    instance_id: &str,
    manifest: &PluginManifest,
    granted_capabilities: &BTreeSet<String>,
    adapters: &BTreeMap<String, Arc<dyn Adapter>>,
) -> Result<Value, PluginError> {
    match command.kind.as_str() {
        "message.reply" => {
            let message_text = command
                .payload
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    PluginError::Permanent("message.reply requires content".to_owned())
                })?;
            let result = match origin {
                ExecutionOrigin::Event(context) => context
                    .reply(message_text)
                    .await
                    .map_err(|error| context_command_error(&error))?,
                ExecutionOrigin::Recovered(origin) => {
                    let target = origin.reply_target.clone().ok_or_else(|| {
                        PluginError::Permanent(
                            "recovered event does not provide a reply target".to_owned(),
                        )
                    })?;
                    let source_message_id = origin.source_message_id.clone().ok_or_else(|| {
                        PluginError::Permanent(
                            "recovered event does not provide a source message ID".to_owned(),
                        )
                    })?;
                    adapters
                        .get(&origin.adapter_id)
                        .ok_or_else(|| {
                            PluginError::Permanent(format!(
                                "recovered event adapter `{}` is unavailable",
                                origin.adapter_id
                            ))
                        })?
                        .execute(Action::Reply(bot_core::ReplyAction {
                            target,
                            source_message_id,
                            content: message_text.to_owned(),
                        }))
                        .await
                        .map_err(|error| adapter_command_error(&error))?
                }
                ExecutionOrigin::Scheduled { .. } => {
                    return Err(PluginError::Permanent(
                        "message.reply is unavailable for scheduled events; use message.send"
                            .to_owned(),
                    ));
                }
            };
            serde_json::to_value(result).map_err(|error| PluginError::Permanent(error.to_string()))
        }
        "message.send" => {
            let action: SendMessageAction = serde_json::from_value(command.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            let result = match origin {
                ExecutionOrigin::Event(context) => context
                    .execute(Action::SendMessage(action))
                    .await
                    .map_err(|error| context_command_error(&error))?,
                ExecutionOrigin::Scheduled { adapter_id } => adapters
                    .get(adapter_id)
                    .ok_or_else(|| {
                        PluginError::Permanent(format!(
                            "scheduled event adapter `{adapter_id}` is unavailable"
                        ))
                    })?
                    .execute(Action::SendMessage(action))
                    .await
                    .map_err(|error| adapter_command_error(&error))?,
                ExecutionOrigin::Recovered(origin) => adapters
                    .get(&origin.adapter_id)
                    .ok_or_else(|| {
                        PluginError::Permanent(format!(
                            "recovered event adapter `{}` is unavailable",
                            origin.adapter_id
                        ))
                    })?
                    .execute(Action::SendMessage(action))
                    .await
                    .map_err(|error| adapter_command_error(&error))?,
            };
            serde_json::to_value(result).map_err(|error| PluginError::Permanent(error.to_string()))
        }
        "http.request" => {
            let request: HttpRequest = serde_json::from_value(command.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            let response = http_executor
                .execute(&manifest.permissions.http, granted_capabilities, &request)
                .await
                .map_err(http_plugin_error)?;
            serde_json::to_value(response)
                .map_err(|error| PluginError::Permanent(error.to_string()))
        }
        "browser.run" => {
            let request: BrowserRun = serde_json::from_value(command.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            validate_browser_request(manifest, granted_capabilities, &request)?;
            let execution = browser_executor
                .execute(
                    &manifest.permissions.browser,
                    granted_capabilities,
                    &request,
                )
                .await
                .map_err(browser_plugin_error)?;
            let (final_url, title, extracted_text, artifacts) = execution.into_parts();
            let mut references = Vec::with_capacity(artifacts.len());
            for artifact in artifacts {
                let (mime_type, data) = artifact.into_parts();
                assets
                    .preflight(instance_id, &mime_type, &data)
                    .map_err(|error| PluginError::ResourceExhausted(error.to_string()))?;
                let (data, digest) = tokio::task::spawn_blocking(move || {
                    let digest = AssetDigest::from_data(&data);
                    (data, digest)
                })
                .await
                .map_err(|error| {
                    PluginError::Permanent(format!("asset hashing task failed: {error}"))
                })?;
                references.push(
                    assets
                        .insert_prehashed(instance_id, mime_type, data, digest)
                        .map_err(|error| PluginError::ResourceExhausted(error.to_string()))?,
                );
            }
            serde_json::to_value(plugin_api::BrowserRunResult {
                final_url,
                title,
                extracted_text,
                assets: references,
            })
            .map_err(|error| PluginError::Permanent(error.to_string()))
        }
        "media.reply" => {
            let request: MediaReply = serde_json::from_value(command.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            let asset = assets
                .get(instance_id, &request.asset_id)
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            let result = match origin {
                ExecutionOrigin::Event(context) => context
                    .execute(Action::ReplyMedia(ReplyMediaAction {
                        target: context.reply_target().cloned().ok_or_else(|| {
                            PluginError::Permanent(
                                "event does not provide a reply target".to_owned(),
                            )
                        })?,
                        source_message_id: context
                            .source_message_id()
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                PluginError::Permanent(
                                    "event does not provide a source message ID".to_owned(),
                                )
                            })?,
                        attachment: MediaAttachment::image(
                            asset.mime_type,
                            None,
                            asset.data.to_vec(),
                        )
                        .map_err(|error| PluginError::Permanent(error.to_owned()))?,
                        caption: request.caption,
                    }))
                    .await
                    .map_err(|error| context_command_error(&error))?,
                ExecutionOrigin::Recovered(origin) => {
                    let target = origin.reply_target.clone().ok_or_else(|| {
                        PluginError::Permanent(
                            "recovered event does not provide a reply target".to_owned(),
                        )
                    })?;
                    let source_message_id = origin.source_message_id.clone().ok_or_else(|| {
                        PluginError::Permanent(
                            "recovered event does not provide a source message ID".to_owned(),
                        )
                    })?;
                    adapters
                        .get(&origin.adapter_id)
                        .ok_or_else(|| {
                            PluginError::Permanent(format!(
                                "recovered event adapter `{}` is unavailable",
                                origin.adapter_id
                            ))
                        })?
                        .execute(Action::ReplyMedia(ReplyMediaAction {
                            target,
                            source_message_id,
                            attachment: MediaAttachment::image(
                                asset.mime_type,
                                None,
                                asset.data.to_vec(),
                            )
                            .map_err(|error| PluginError::Permanent(error.to_owned()))?,
                            caption: request.caption,
                        }))
                        .await
                        .map_err(|error| adapter_command_error(&error))?
                }
                ExecutionOrigin::Scheduled { .. } => {
                    return Err(PluginError::Permanent(
                        "media.reply is unavailable for scheduled events; use media.send"
                            .to_owned(),
                    ));
                }
            };
            if request.consume {
                assets
                    .remove(instance_id, &request.asset_id)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
            }
            serde_json::to_value(result).map_err(|error| PluginError::Permanent(error.to_string()))
        }
        "media.send" => {
            let request: MediaSend = serde_json::from_value(command.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            let asset = assets
                .get(instance_id, &request.asset_id)
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            let adapter_id = match origin {
                ExecutionOrigin::Event(context) => context.adapter_id().to_string(),
                ExecutionOrigin::Recovered(origin) => origin.adapter_id.clone(),
                ExecutionOrigin::Scheduled { adapter_id } => adapter_id.to_owned(),
            };
            let result = adapters
                .get(&adapter_id)
                .ok_or_else(|| {
                    PluginError::Permanent(format!("adapter `{adapter_id}` is unavailable"))
                })?
                .execute(Action::SendMedia(SendMediaAction {
                    target: message_target(request.target),
                    attachment: MediaAttachment::image(asset.mime_type, None, asset.data.to_vec())
                        .map_err(|error| PluginError::Permanent(error.to_owned()))?,
                    caption: request.caption,
                }))
                .await
                .map_err(|error| adapter_command_error(&error))?;
            if request.consume {
                assets
                    .remove(instance_id, &request.asset_id)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?;
            }
            serde_json::to_value(result).map_err(|error| PluginError::Permanent(error.to_string()))
        }
        other => Err(PluginError::Permanent(format!(
            "command executor `{other}` is not implemented"
        ))),
    }
}

#[allow(clippy::too_many_lines)]
fn validate_browser_request(
    manifest: &PluginManifest,
    grants: &BTreeSet<String>,
    request: &BrowserRun,
) -> Result<(), PluginError> {
    if request.steps.is_empty() || request.steps.len() > 32 {
        return Err(PluginError::Permanent(
            "browser.run requires between 1 and 32 steps".to_owned(),
        ));
    }
    if request.viewport.width < 320
        || request.viewport.width > 3840
        || request.viewport.height < 240
        || request.viewport.height > 2160
        || !(1..=3).contains(&request.viewport.device_scale_factor)
    {
        return Err(PluginError::Permanent(
            "browser viewport exceeds Host limits".to_owned(),
        ));
    }
    if request
        .user_agent
        .as_ref()
        .is_some_and(|value| value.len() > 1024)
        || request
            .locale
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 64)
        || request
            .timezone
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 128)
    {
        return Err(PluginError::Permanent(
            "browser user-agent, locale, or timezone exceeds Host limits".to_owned(),
        ));
    }
    let mut normalized_header_names = BTreeSet::new();
    if request.extra_headers.len() > 32
        || request.extra_headers.iter().any(|(name, value)| {
            let normalized = name.to_ascii_lowercase();
            name.is_empty()
                || name.len() > 128
                || value.len() > 4096
                || HeaderName::from_bytes(name.as_bytes()).is_err()
                || HeaderValue::from_bytes(value.as_bytes()).is_err()
                || !normalized_header_names.insert(normalized.clone())
                || matches!(
                    normalized.as_str(),
                    "authorization" | "cookie" | "host" | "proxy-authorization"
                )
        })
    {
        return Err(PluginError::Permanent(
            "browser headers exceed limits or contain a forbidden name".to_owned(),
        ));
    }
    let mut current_url = None;
    let mut fixed_wait_ms = 0_u64;
    for step in &request.steps {
        let (capability, selector, timeout_ms) = match step {
            BrowserStep::Navigate {
                url, timeout_ms, ..
            } => {
                if url.len() > 2048 {
                    return Err(PluginError::Permanent(
                        "browser URL exceeds the 2048 byte limit".to_owned(),
                    ));
                }
                authorize_browser_url(manifest, grants, url, "navigate")?;
                current_url = Some(url.as_str());
                (None, None, Some(*timeout_ms))
            }
            BrowserStep::Click {
                selector,
                timeout_ms,
            }
            | BrowserStep::WaitFor {
                selector,
                timeout_ms,
            } => (Some("interact"), Some(selector.as_str()), Some(*timeout_ms)),
            BrowserStep::Fill {
                selector,
                value,
                timeout_ms,
            } => {
                if value.len() > 64 * 1024 {
                    return Err(PluginError::Permanent(
                        "browser fill value is too large".to_owned(),
                    ));
                }
                (Some("interact"), Some(selector.as_str()), Some(*timeout_ms))
            }
            BrowserStep::WaitForIdle { timeout_ms } => (None, None, Some(*timeout_ms)),
            BrowserStep::Wait { duration_ms } => {
                if *duration_ms == 0 || *duration_ms > 10_000 {
                    return Err(PluginError::Permanent(
                        "browser wait must be between 1 and 10000 ms".to_owned(),
                    ));
                }
                fixed_wait_ms = fixed_wait_ms.saturating_add(*duration_ms);
                (None, None, None)
            }
            BrowserStep::ExtractText {
                selector,
                timeout_ms,
            } => (
                Some("extract_text"),
                Some(selector.as_str()),
                Some(*timeout_ms),
            ),
            BrowserStep::Screenshot {
                selector,
                format,
                quality,
                ..
            } => {
                if quality.is_some_and(|quality| quality == 0 || quality > 100)
                    || matches!(format, plugin_api::BrowserScreenshotFormat::Png)
                        && quality.is_some()
                {
                    return Err(PluginError::Permanent(
                        "browser screenshot quality is only valid for JPEG and must be 1..=100"
                            .to_owned(),
                    ));
                }
                (Some("screenshot"), selector.as_deref(), None)
            }
        };
        if selector.is_some_and(|selector| selector.is_empty() || selector.len() > 1024) {
            return Err(PluginError::Permanent(
                "browser selector must contain 1 to 1024 bytes".to_owned(),
            ));
        }
        if timeout_ms.is_some_and(|timeout| timeout == 0 || timeout > 15_000) {
            return Err(PluginError::Permanent(
                "browser step timeout must be between 1 and 15000 ms".to_owned(),
            ));
        }
        if let Some(capability) = capability {
            let url = current_url.ok_or_else(|| {
                PluginError::Permanent(
                    "browser interaction requires a preceding navigate step".to_owned(),
                )
            })?;
            authorize_browser_url(manifest, grants, url, capability)?;
        }
    }
    if fixed_wait_ms > 15_000 {
        return Err(PluginError::Permanent(
            "browser fixed waits exceed the 15 second task limit".to_owned(),
        ));
    }
    Ok(())
}

fn authorize_browser_url(
    manifest: &PluginManifest,
    grants: &BTreeSet<String>,
    value: &str,
    operation: &str,
) -> Result<(), PluginError> {
    let url = url::Url::parse(value)
        .map_err(|_| PluginError::Permanent("browser URL must be absolute".to_owned()))?;
    if !matches!(url.scheme(), "http" | "https") || url.username() != "" || url.password().is_some()
    {
        return Err(PluginError::PermissionDenied(
            "browser URL scheme or credentials are not allowed".to_owned(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| PluginError::Permanent("browser URL is missing a host".to_owned()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| PluginError::Permanent("browser URL is missing a port".to_owned()))?;
    let capability = format!("browser.origin.{}.{host}:{port}.{operation}", url.scheme());
    let allowed = manifest.permissions.browser.iter().any(|permission| {
        permission.scheme == url.scheme()
            && permission.host == host
            && permission.port == port
            && permission.capabilities.contains(operation)
            && permission
                .path_prefixes
                .iter()
                .any(|prefix| url_path_matches_prefix(url.path(), prefix))
    });
    if !allowed || !grants.contains(&capability) {
        return Err(PluginError::PermissionDenied(format!(
            "browser URL is not granted for {operation}"
        )));
    }
    Ok(())
}

fn message_target(target: PluginMessageTarget) -> bot_core::MessageTarget {
    match target {
        PluginMessageTarget::Group { group_id } => bot_core::MessageTarget::Group { group_id },
        PluginMessageTarget::Private { user_id } => bot_core::MessageTarget::Private { user_id },
        PluginMessageTarget::Channel { channel_id } => {
            bot_core::MessageTarget::Channel { channel_id }
        }
        PluginMessageTarget::GuildDirect { guild_id } => {
            bot_core::MessageTarget::GuildDirect { guild_id }
        }
    }
}

fn browser_plugin_error(error: BrowserExecutionError) -> PluginError {
    match error {
        BrowserExecutionError::Unavailable(message) | BrowserExecutionError::Worker(message) => {
            PluginError::Transient(message)
        }
        BrowserExecutionError::Denied(message) => PluginError::PermissionDenied(message),
        BrowserExecutionError::InvalidRequest(message) => PluginError::Permanent(message),
        BrowserExecutionError::ResourceExhausted(message) => {
            PluginError::ResourceExhausted(message)
        }
    }
}

fn adapter_command_error(error: &AdapterError) -> PluginError {
    match error {
        AdapterError::ActionUnknown(_) | AdapterError::Transport(_) => {
            PluginError::Transient(error.to_string())
        }
        AdapterError::Configuration(_)
        | AdapterError::Action(_)
        | AdapterError::EventAdapterMismatch { .. }
        | AdapterError::EventQueueClosed => PluginError::Permanent(error.to_string()),
    }
}

fn http_plugin_error(error: HttpExecutionError) -> PluginError {
    match error {
        HttpExecutionError::Denied(message) => PluginError::PermissionDenied(message),
        HttpExecutionError::InvalidRequest(message) => PluginError::Permanent(message),
        HttpExecutionError::ResponseTooLarge => PluginError::ResourceExhausted(error.to_string()),
        HttpExecutionError::Dns(_) | HttpExecutionError::NonPublicAddress => {
            PluginError::PermissionDenied(error.to_string())
        }
        HttpExecutionError::Transport(_) => PluginError::Transient(error.to_string()),
    }
}

fn context_command_error(error: &ContextError) -> PluginError {
    match error {
        ContextError::Adapter(AdapterError::ActionUnknown(_) | AdapterError::Transport(_)) => {
            PluginError::Transient(error.to_string())
        }
        ContextError::MissingReplyTarget
        | ContextError::Adapter(
            AdapterError::Configuration(_)
            | AdapterError::Action(_)
            | AdapterError::EventAdapterMismatch { .. }
            | AdapterError::EventQueueClosed,
        ) => PluginError::Permanent(error.to_string()),
    }
}

pub(super) fn failed_completion(
    source_event_id: &str,
    invocation_id: &str,
    command: &PluginCommand,
    error: &PluginError,
) -> ActionCompleted {
    let (status, retryable, code) = match error {
        PluginError::PermissionDenied(_) => (ActionStatus::Denied, false, "permission_denied"),
        PluginError::ResourceExhausted(_) => (ActionStatus::Failed, false, "resource_exhausted"),
        PluginError::ResourceExhaustedTrap(_) => {
            (ActionStatus::Failed, false, "resource_exhausted")
        }
        PluginError::GuestTrap(_) => (ActionStatus::Failed, false, "guest_trap"),
        PluginError::Transient(_) => (ActionStatus::Unknown, false, "result_unknown"),
        PluginError::InvalidConfig(_) => (ActionStatus::Failed, false, "invalid_config"),
        PluginError::Permanent(_) => (ActionStatus::Failed, false, "permanent"),
    };
    ActionCompleted {
        source_event_id: source_event_id.to_owned(),
        source_invocation_id: invocation_id.to_owned(),
        command_id: command.command_id.clone(),
        kind: command.kind.clone(),
        status,
        retryable,
        result: None,
        error_code: Some(code.to_owned()),
        error_message: Some(error.to_string()),
    }
}

pub(super) const fn action_status_name(status: ActionStatus) -> &'static str {
    match status {
        ActionStatus::Succeeded => "succeeded",
        ActionStatus::Failed => "failed",
        ActionStatus::Denied => "denied",
        ActionStatus::TimedOut => "timed_out",
        ActionStatus::Unknown => "unknown",
        ActionStatus::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use plugin_api::{PluginError, PluginManifest};

    use super::authorize_browser_url;

    #[test]
    fn browser_authorization_matches_the_exact_origin_and_path_boundary() {
        let manifest = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.browser-auth"
                version = "1.0.0"
                protocol = ">=1.1,<2.0"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "Browser Auth"

                [[permissions.browser]]
                scheme = "https"
                host = "example.com"
                port = 443
                path_prefixes = ["/allowed"]
                capabilities = ["navigate"]
            "#,
        )
        .unwrap();
        let grants = manifest.requested_capabilities();

        authorize_browser_url(
            &manifest,
            &grants,
            "https://example.com/allowed/page",
            "navigate",
        )
        .unwrap();
        for denied in [
            "http://example.com:443/allowed/page",
            "https://example.com/allowed-private",
        ] {
            assert!(matches!(
                authorize_browser_url(&manifest, &grants, denied, "navigate"),
                Err(PluginError::PermissionDenied(_))
            ));
        }
    }
}
