//! Conversion between BPP Rust types and generated WIT bindings.

use super::{
    ActionCompleted, BTreeMap, BTreeSet, BrowserColorScheme, BrowserRun, BrowserScreenshotFormat,
    BrowserStep, BrowserViewport, BrowserWaitUntil, Event, HandlerOutput, HttpRequest, MediaReply,
    MediaSend, MemoryLimitExceeded, MessageSegment, MessageTarget, PluginCommand, PluginDiagnostic,
    PluginError, PluginEventEnvelope, PluginMessageTarget, ScheduleCancel, ScheduleCreate,
    ScheduleTriggered, StateOp, StateValue, StoreData, Value, json, types,
};

const MAX_ENCODED_VALUE_BYTES: usize = 1024 * 1024;

pub(super) fn config_entries(config: &BTreeMap<String, Value>) -> Vec<types::ConfigEntry> {
    config
        .iter()
        .map(|(key, value)| types::ConfigEntry {
            key: key.clone(),
            value: StoreData::encoded_json(value),
        })
        .collect()
}

pub(super) fn state_entries(state: &BTreeMap<String, StateValue>) -> Vec<types::StateEntry> {
    state
        .iter()
        .map(|(key, value)| types::StateEntry {
            key: key.clone(),
            value: value.value.clone(),
            revision: value.revision,
        })
        .collect()
}

pub(super) fn state_op(operation: types::StateOp) -> StateOp {
    match operation {
        types::StateOp::Put(put) => StateOp::Put {
            key: put.key,
            value: put.value,
            expected_revision: put.expected_revision,
        },
        types::StateOp::Delete(delete) => StateOp::Delete {
            key: delete.key,
            expected_revision: delete.expected_revision,
        },
    }
}

pub(super) fn wit_event(event: &PluginEventEnvelope) -> Result<types::EventEnvelope, PluginError> {
    let payload = match event.event_type.as_str() {
        "message.created" => match serde_json::from_value::<Event>(event.payload.clone())
            .map_err(|error| PluginError::Permanent(error.to_string()))?
        {
            Event::Message(message) => types::EventPayload::MessageCreated(types::MessageCreated {
                message_id: message.message_id,
                target: wit_target(message.target),
                sender: types::Sender {
                    id: message.sender.id,
                    display_name: message.sender.display_name,
                },
                text: message.text,
                segments: message.segments.into_iter().map(wit_segment).collect(),
                reply_to: message.reply_to,
            }),
            _ => {
                return Err(PluginError::Permanent(
                    "message event payload mismatch".to_owned(),
                ));
            }
        },
        "action.completed" => {
            let completion: ActionCompleted = serde_json::from_value(event.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            types::EventPayload::ActionCompleted(types::ActionCompleted {
                source_event_id: completion.source_event_id,
                source_invocation_id: completion.source_invocation_id,
                command_id: completion.command_id,
                kind: completion.kind,
                status: match completion.status {
                    plugin_api::ActionStatus::Succeeded => types::ActionStatus::Succeeded,
                    plugin_api::ActionStatus::Failed => types::ActionStatus::Failed,
                    plugin_api::ActionStatus::Denied => types::ActionStatus::Denied,
                    plugin_api::ActionStatus::TimedOut => types::ActionStatus::TimedOut,
                    plugin_api::ActionStatus::Unknown => types::ActionStatus::Unknown,
                    plugin_api::ActionStatus::Cancelled => types::ActionStatus::Cancelled,
                },
                retryable: completion.retryable,
                result_data: completion.result.as_ref().map(StoreData::encoded_json),
                error_code: completion.error_code,
                error_message: completion.error_message,
            })
        }
        "schedule.triggered" => {
            let triggered: ScheduleTriggered = serde_json::from_value(event.payload.clone())
                .map_err(|error| PluginError::Permanent(error.to_string()))?;
            types::EventPayload::ScheduleTriggered(types::ScheduleTriggered {
                task_id: triggered.task_id,
                scheduled_at_ms: triggered.scheduled_at_ms,
                payload: StoreData::encoded_json(&triggered.payload),
            })
        }
        "notice.received" => types::EventPayload::Notice(StoreData::encoded_json(&event.payload)),
        "request.received" => types::EventPayload::Request(StoreData::encoded_json(&event.payload)),
        "lifecycle.changed" => {
            types::EventPayload::Lifecycle(StoreData::encoded_json(&event.payload))
        }
        _ => types::EventPayload::Opaque(StoreData::encoded_json(&event.payload)),
    };
    Ok(types::EventEnvelope {
        protocol_version: event.protocol_version.clone(),
        event_id: event.event_id.clone(),
        delivery_id: event.delivery_id.clone(),
        invocation_id: event.invocation_id.clone(),
        occurred_at_ms: event.occurred_at_ms,
        received_at_ms: event.received_at_ms,
        adapter_id: event.adapter_id.clone(),
        event_type: event.event_type.clone(),
        trace_id: event.trace_id.clone(),
        payload,
        extensions: event
            .extensions
            .iter()
            .map(|extension| types::ExtensionPayload {
                namespace: extension.namespace.clone(),
                schema_version: extension.schema_version.clone(),
                content_type: extension.content_type.clone(),
                data: extension.data.clone(),
            })
            .collect(),
    })
}

pub(super) fn wit_target(target: MessageTarget) -> types::MessageTarget {
    match target {
        MessageTarget::Group { group_id } => types::MessageTarget::Group(group_id),
        MessageTarget::Private { user_id } => types::MessageTarget::Private(user_id),
        MessageTarget::Channel { channel_id } => types::MessageTarget::Channel(channel_id),
        MessageTarget::GuildDirect { guild_id } => types::MessageTarget::GuildDirect(guild_id),
    }
}

pub(super) fn wit_segment(segment: MessageSegment) -> types::MessageSegment {
    match segment {
        MessageSegment::Text { text } => types::MessageSegment::Text(text),
        MessageSegment::Platform { kind, data } => {
            types::MessageSegment::Opaque(StoreData::encoded_json(&json!({
                "kind": kind,
                "data": data,
            })))
        }
    }
}

pub(super) fn handler_output(output: types::HandlerOutput) -> Result<HandlerOutput, PluginError> {
    if output.commands.len() > crate::runtime::MAX_COMMANDS {
        return Err(PluginError::Permanent(
            "plugin output exceeds the 32 command limit".to_owned(),
        ));
    }
    let mut browser_bytes = 0_usize;
    for command in &output.commands {
        if let types::CommandPayload::BrowserRun(request) = &command.payload {
            validate_wit_browser_request(request)?;
            browser_bytes = browser_bytes.saturating_add(wit_browser_request_size(request));
        }
    }
    if browser_bytes > MAX_ENCODED_VALUE_BYTES {
        return Err(PluginError::Permanent(
            "browser commands exceed the aggregate 1 MiB output limit".to_owned(),
        ));
    }
    Ok(HandlerOutput {
        disposition: match output.disposition {
            types::Disposition::Continue => plugin_api::Disposition::Continue,
            types::Disposition::Stop => plugin_api::Disposition::Stop,
            types::Disposition::Ignore => plugin_api::Disposition::Ignore,
        },
        state_ops: output.state_ops.into_iter().map(state_op).collect(),
        commands: output
            .commands
            .into_iter()
            .map(plugin_command)
            .collect::<Result<Vec<_>, _>>()?,
        diagnostics: output
            .diagnostics
            .into_iter()
            .map(plugin_diagnostic)
            .collect(),
    })
}

fn wit_browser_request_size(request: &types::BrowserRunCommand) -> usize {
    let mut size = 256_usize
        .saturating_add(request.steps.len().saturating_mul(64))
        .saturating_add(request.extra_headers.len().saturating_mul(64))
        .saturating_add(
            request
                .user_agent
                .as_ref()
                .map_or(0, |value| json_string_upper_bound(value)),
        )
        .saturating_add(
            request
                .locale
                .as_ref()
                .map_or(0, |value| json_string_upper_bound(value)),
        )
        .saturating_add(
            request
                .timezone
                .as_ref()
                .map_or(0, |value| json_string_upper_bound(value)),
        );
    for header in &request.extra_headers {
        size = size
            .saturating_add(json_string_upper_bound(&header.name))
            .saturating_add(json_string_upper_bound(&header.value));
    }
    for step in &request.steps {
        size = size.saturating_add(match step {
            types::BrowserStep::Navigate(step) => json_string_upper_bound(&step.url),
            types::BrowserStep::Click(step) => json_string_upper_bound(&step.selector),
            types::BrowserStep::Fill(step) => json_string_upper_bound(&step.selector)
                .saturating_add(json_string_upper_bound(&step.value)),
            types::BrowserStep::WaitFor(step) => json_string_upper_bound(&step.selector),
            types::BrowserStep::ExtractText(step) => json_string_upper_bound(&step.selector),
            types::BrowserStep::Screenshot(step) => step
                .selector
                .as_ref()
                .map_or(0, |value| json_string_upper_bound(value)),
            types::BrowserStep::WaitForIdle(_) | types::BrowserStep::Wait(_) => 0,
        });
    }
    size
}

fn json_string_upper_bound(value: &str) -> usize {
    value.len().saturating_mul(6).saturating_add(2)
}

#[allow(clippy::too_many_lines)]
pub(super) fn plugin_command(command: types::Command) -> Result<PluginCommand, PluginError> {
    let (kind, payload) = match command.payload {
        types::CommandPayload::MessageReply(reply) => (
            "message.reply".to_owned(),
            json!({"content": reply.content}),
        ),
        types::CommandPayload::MessageSend(send) => (
            "message.send".to_owned(),
            json!({
                "target": core_target(send.target),
                "content": send.content,
            }),
        ),
        types::CommandPayload::MediaReply(reply) => (
            "media.reply".to_owned(),
            serde_json::to_value(MediaReply {
                asset_id: reply.asset_id,
                caption: reply.caption,
                consume: reply.consume,
            })
            .map_err(|error| PluginError::Permanent(error.to_string()))?,
        ),
        types::CommandPayload::MediaSend(send) => (
            "media.send".to_owned(),
            serde_json::to_value(MediaSend {
                target: plugin_target(send.target),
                asset_id: send.asset_id,
                caption: send.caption,
                consume: send.consume,
            })
            .map_err(|error| PluginError::Permanent(error.to_string()))?,
        ),
        types::CommandPayload::HttpRequest(request) => {
            let request = HttpRequest {
                method: request.method,
                url: request.url,
                headers: request
                    .headers
                    .into_iter()
                    .map(|header| (header.name, header.value))
                    .collect(),
                body: request.body,
                timeout_ms: request.timeout_ms,
                max_response_bytes: request.max_response_bytes,
            };
            (
                "http.request".to_owned(),
                serde_json::to_value(request)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
            )
        }
        types::CommandPayload::BrowserRun(request) => {
            validate_wit_browser_request(&request)?;
            let mut header_names = BTreeSet::new();
            let mut extra_headers = BTreeMap::new();
            for header in request.extra_headers {
                if !header_names.insert(header.name.to_ascii_lowercase()) {
                    return Err(PluginError::Permanent(format!(
                        "duplicate browser header `{}`",
                        header.name
                    )));
                }
                extra_headers.insert(header.name, header.value);
            }
            let request = BrowserRun {
                steps: request.steps.into_iter().map(browser_step).collect(),
                viewport: BrowserViewport {
                    width: request.viewport.width,
                    height: request.viewport.height,
                    device_scale_factor: request.viewport.device_scale_factor,
                },
                user_agent: request.user_agent,
                locale: request.locale,
                timezone: request.timezone,
                color_scheme: request.color_scheme.map(|scheme| match scheme {
                    types::BrowserColorScheme::Light => BrowserColorScheme::Light,
                    types::BrowserColorScheme::Dark => BrowserColorScheme::Dark,
                    types::BrowserColorScheme::NoPreference => BrowserColorScheme::NoPreference,
                }),
                extra_headers,
            };
            (
                "browser.run".to_owned(),
                serde_json::to_value(request)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
            )
        }
        types::CommandPayload::ScheduleCreate(create) => {
            let payload = decode_json(&create.payload)?;
            let create = ScheduleCreate {
                task_id: create.task_id,
                run_at_ms: create.run_at_ms,
                payload,
            };
            (
                "schedule.create".to_owned(),
                serde_json::to_value(create)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
            )
        }
        types::CommandPayload::ScheduleCancel(cancel) => {
            let cancel = ScheduleCancel {
                task_id: cancel.task_id,
            };
            (
                "schedule.cancel".to_owned(),
                serde_json::to_value(cancel)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
            )
        }
    };
    Ok(PluginCommand {
        command_id: command.command_id,
        kind,
        idempotency_key: command.idempotency_key,
        deadline_ms: command.deadline_ms,
        payload,
    })
}

fn validate_wit_browser_request(request: &types::BrowserRunCommand) -> Result<(), PluginError> {
    if request.steps.is_empty()
        || request.steps.len() > 32
        || request.extra_headers.len() > 32
        || request
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
        || request
            .extra_headers
            .iter()
            .any(|header| header.name.len() > 128 || header.value.len() > 4096)
        || request.steps.iter().any(wit_browser_step_exceeds_limits)
    {
        return Err(PluginError::Permanent(
            "browser request exceeds Host count or field-size limits".to_owned(),
        ));
    }
    Ok(())
}

fn wit_browser_step_exceeds_limits(step: &types::BrowserStep) -> bool {
    match step {
        types::BrowserStep::Navigate(step) => step.url.len() > 2048,
        types::BrowserStep::Click(step) => step.selector.len() > 1024,
        types::BrowserStep::WaitFor(step) => step.selector.len() > 1024,
        types::BrowserStep::ExtractText(step) => step.selector.len() > 1024,
        types::BrowserStep::Fill(step) => {
            step.selector.len() > 1024 || step.value.len() > 64 * 1024
        }
        types::BrowserStep::Screenshot(step) => step
            .selector
            .as_ref()
            .is_some_and(|selector| selector.len() > 1024),
        types::BrowserStep::WaitForIdle(_) | types::BrowserStep::Wait(_) => false,
    }
}

fn browser_step(step: types::BrowserStep) -> BrowserStep {
    match step {
        types::BrowserStep::Navigate(step) => BrowserStep::Navigate {
            url: step.url,
            wait_until: match step.wait_until {
                types::BrowserWaitUntil::Load => BrowserWaitUntil::Load,
                types::BrowserWaitUntil::DomContentLoaded => BrowserWaitUntil::DomContentLoaded,
                types::BrowserWaitUntil::NetworkIdle => BrowserWaitUntil::NetworkIdle,
            },
            timeout_ms: step.timeout_ms,
        },
        types::BrowserStep::Click(step) => BrowserStep::Click {
            selector: step.selector,
            timeout_ms: step.timeout_ms,
        },
        types::BrowserStep::Fill(step) => BrowserStep::Fill {
            selector: step.selector,
            value: step.value,
            timeout_ms: step.timeout_ms,
        },
        types::BrowserStep::WaitFor(step) => BrowserStep::WaitFor {
            selector: step.selector,
            timeout_ms: step.timeout_ms,
        },
        types::BrowserStep::WaitForIdle(step) => BrowserStep::WaitForIdle {
            timeout_ms: step.timeout_ms,
        },
        types::BrowserStep::Wait(step) => BrowserStep::Wait {
            duration_ms: step.duration_ms,
        },
        types::BrowserStep::ExtractText(step) => BrowserStep::ExtractText {
            selector: step.selector,
            timeout_ms: step.timeout_ms,
        },
        types::BrowserStep::Screenshot(step) => BrowserStep::Screenshot {
            selector: step.selector,
            full_page: step.full_page,
            format: match step.format {
                types::BrowserScreenshotFormat::Png => BrowserScreenshotFormat::Png,
                types::BrowserScreenshotFormat::Jpeg => BrowserScreenshotFormat::Jpeg,
            },
            quality: step.quality,
        },
    }
}

fn plugin_target(target: types::MessageTarget) -> PluginMessageTarget {
    match target {
        types::MessageTarget::Group(group_id) => PluginMessageTarget::Group { group_id },
        types::MessageTarget::Private(user_id) => PluginMessageTarget::Private { user_id },
        types::MessageTarget::Channel(channel_id) => PluginMessageTarget::Channel { channel_id },
        types::MessageTarget::GuildDirect(guild_id) => {
            PluginMessageTarget::GuildDirect { guild_id }
        }
    }
}

pub(super) fn core_target(target: types::MessageTarget) -> MessageTarget {
    match target {
        types::MessageTarget::Group(group_id) => MessageTarget::Group { group_id },
        types::MessageTarget::Private(user_id) => MessageTarget::Private { user_id },
        types::MessageTarget::Channel(channel_id) => MessageTarget::Channel { channel_id },
        types::MessageTarget::GuildDirect(guild_id) => MessageTarget::GuildDirect { guild_id },
    }
}

pub(super) fn decode_json(value: &types::EncodedValue) -> Result<Value, PluginError> {
    if value.data.len() > MAX_ENCODED_VALUE_BYTES {
        return Err(PluginError::ResourceExhausted(
            "encoded JSON payload exceeds the Host limit".to_owned(),
        ));
    }
    if value.schema_version != "1.0" {
        return Err(PluginError::Permanent(format!(
            "unsupported encoded value schema version `{}`",
            value.schema_version
        )));
    }
    if value.content_type != "application/json" {
        return Err(PluginError::Permanent(format!(
            "unsupported encoded value content type `{}`",
            value.content_type
        )));
    }
    serde_json::from_slice(&value.data).map_err(|error| PluginError::Permanent(error.to_string()))
}

pub(super) fn plugin_diagnostic(diagnostic: types::Diagnostic) -> PluginDiagnostic {
    PluginDiagnostic {
        level: match diagnostic.level {
            types::DiagnosticLevel::Debug => "debug",
            types::DiagnosticLevel::Info => "info",
            types::DiagnosticLevel::Warning => "warning",
            types::DiagnosticLevel::Error => "error",
        }
        .to_owned(),
        code: diagnostic.code,
        message: diagnostic.message,
    }
}

pub(super) fn guest_error(error: types::PluginError) -> PluginError {
    match error.code {
        types::PluginErrorCode::InvalidConfig => PluginError::InvalidConfig(error.message),
        types::PluginErrorCode::PermissionDenied => PluginError::PermissionDenied(error.message),
        types::PluginErrorCode::ResourceExhausted => PluginError::ResourceExhausted(error.message),
        types::PluginErrorCode::Transient => PluginError::Transient(error.message),
        types::PluginErrorCode::Permanent => PluginError::Permanent(error.message),
    }
}

pub(super) fn runtime_error(error: anyhow::Error) -> PluginError {
    let resource_exhausted = error.chain().any(|cause| {
        if cause.downcast_ref::<MemoryLimitExceeded>().is_some() {
            return true;
        }
        cause.downcast_ref::<wasmtime::Trap>().is_some_and(|trap| {
            matches!(trap, wasmtime::Trap::OutOfFuel | wasmtime::Trap::Interrupt)
        })
    });
    let message = format!("{error:#}");
    drop(error);
    if resource_exhausted {
        PluginError::ResourceExhaustedTrap(message)
    } else {
        PluginError::GuestTrap(message)
    }
}
