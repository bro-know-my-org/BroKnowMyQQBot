//! Host command execution and action-completion mapping.

use super::{
    Action, ActionCompleted, ActionStatus, Adapter, AdapterError, Arc, BTreeMap, BTreeSet,
    ContextError, ExecutionOrigin, HttpExecutionError, HttpExecutor, HttpRequest, PluginCommand,
    PluginError, PluginManifest, SendMessageAction, Value,
};

pub(super) async fn execute_context_command(
    origin: ExecutionOrigin<'_>,
    command: &PluginCommand,
    http_executor: &dyn HttpExecutor,
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
        other => Err(PluginError::Permanent(format!(
            "command executor `{other}` is not implemented"
        ))),
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
