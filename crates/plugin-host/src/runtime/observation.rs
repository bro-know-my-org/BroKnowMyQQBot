//! Metadata-only observations shared by the static and WASM execution paths.

use std::time::Duration;

use plugin_api::{
    ActionCompleted, ActionStatus, Disposition, HandlerOutput, PluginCommand, PluginEventEnvelope,
};
use tracing::{Span, debug, info, info_span, warn};

use super::{PluginHostError, RegisteredPlugin, action_status_name};

pub(super) fn invocation_span(plugin: &RegisteredPlugin, envelope: &PluginEventEnvelope) -> Span {
    info_span!(
        "plugin.invocation",
        plugin_id = %plugin.manifest.id,
        instance_id = plugin.instance_id,
        event_id = envelope.event_id,
        invocation_id = envelope.invocation_id,
        command_name = tracing::field::Empty,
    )
}

pub(super) fn action_span(
    plugin: &RegisteredPlugin,
    event_id: &str,
    invocation_id: &str,
    command: &PluginCommand,
) -> Span {
    info_span!(
        "plugin.action",
        plugin_id = %plugin.manifest.id,
        instance_id = plugin.instance_id,
        event_id,
        invocation_id,
        command_id = command.command_id,
        action_type = command.kind,
    )
}

pub(super) struct OutputObservation {
    commands: usize,
    state_ops: usize,
    diagnostics: usize,
    disposition: &'static str,
    observable: bool,
    declared_command: bool,
}

impl OutputObservation {
    pub(super) fn new(output: &HandlerOutput, declared_command: bool) -> Self {
        Self {
            commands: output.commands.len(),
            state_ops: output.state_ops.len(),
            diagnostics: output.diagnostics.len(),
            disposition: match output.disposition {
                Disposition::Continue => "continue",
                Disposition::Stop => "stop",
                Disposition::Ignore => "ignore",
            },
            observable: !output.commands.is_empty()
                || !output.state_ops.is_empty()
                || !output.diagnostics.is_empty()
                || output.disposition == Disposition::Stop,
            declared_command,
        }
    }

    pub(super) fn completed(&self, elapsed: Duration) {
        let status = if self.observable {
            "output_committed"
        } else {
            "no_output"
        };
        let elapsed_ms = elapsed_ms(elapsed);
        macro_rules! completed {
            ($level:ident) => {
                $level!(
                    record_kind = "plugin.completed",
                    status,
                    command_count = self.commands,
                    state_op_count = self.state_ops,
                    diagnostic_count = self.diagnostics,
                    disposition = self.disposition,
                    elapsed_ms,
                    "plugin invocation completed"
                );
            };
        }
        if self.observable || self.declared_command {
            completed!(info);
        } else {
            completed!(debug);
        }
    }
}

pub(super) fn action_completed(
    plugin: &RegisteredPlugin,
    completion: &ActionCompleted,
    elapsed: Duration,
) {
    let status_name = action_status_name(completion.status);
    let elapsed_ms = elapsed_ms(elapsed);
    macro_rules! completed {
        ($level:ident) => {
            $level!(
                record_kind = "plugin.action.completed",
                plugin_id = %plugin.manifest.id, instance_id = plugin.instance_id,
                event_id = completion.source_event_id, invocation_id = completion.source_invocation_id,
                command_id = completion.command_id, action_type = completion.kind,
                status = status_name, elapsed_ms, "plugin action completed"
            );
        };
    }
    if completion.status == ActionStatus::Succeeded {
        completed!(info);
    } else {
        completed!(warn);
    }
}

pub(super) fn elapsed_ms(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

pub(super) const fn host_error_class(error: &PluginHostError) -> &'static str {
    match error {
        PluginHostError::GuestTrap { .. } | PluginHostError::Panic { .. } => "guest_trap",
        PluginHostError::ResourceExhaustedTrap { .. } | PluginHostError::Timeout { .. } => {
            "resource_exhausted"
        }
        PluginHostError::TransientInvocation { .. } => "transient",
        PluginHostError::InvalidOutput { .. } => "invalid_output",
        PluginHostError::Store(_) => "host_failure",
        PluginHostError::LifecycleChanged { .. } => "lifecycle_changed",
        _ => "plugin_error",
    }
}
