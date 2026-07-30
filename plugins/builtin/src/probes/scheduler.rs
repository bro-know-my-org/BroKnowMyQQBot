use super::{
    ActionCompleted, ActionStatus, BTreeSet, CommandDeclaration, Disposition, HandlerOutput,
    HostQueries, PluginError, PluginEventEnvelope, PluginManifest, ScheduleCancel, ScheduleCreate,
    ScheduleTriggered, StaticPlugin, Subscription, Value, async_trait, command_argument,
    command_tail, hex_bytes, ignored, json, message_plugin_manifest, message_text, plugin_command,
    reply,
};

const MAX_PENDING_TASKS: usize = 128;

#[derive(Debug)]
pub struct SchedulerProbePlugin {
    manifest: PluginManifest,
}

impl Default for SchedulerProbePlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.scheduler-probe",
            "Scheduler Probe",
            "schedule",
            "message.reply",
            true,
        );
        manifest.commands.push(CommandDeclaration {
            name: "schedule-cancel".to_owned(),
            aliases: Vec::new(),
            description: "Cancel a scheduler probe task".to_owned(),
        });
        manifest.subscriptions.extend([
            Subscription {
                id: "schedule-triggers".to_owned(),
                event: "schedule.triggered".to_owned(),
                priority: 0,
                scopes: BTreeSet::new(),
            },
            Subscription {
                id: "schedule-results".to_owned(),
                event: "action.completed".to_owned(),
                priority: 0,
                scopes: BTreeSet::new(),
            },
        ]);
        manifest.permissions.scheduler = true;
        manifest
            .permissions
            .actions
            .insert("message.send".to_owned());
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for SchedulerProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if event.event_type == "message.created" {
            return scheduler_message(event, queries);
        }
        if event.event_type == "schedule.triggered" {
            return schedule_triggered(event, queries);
        }
        if event.event_type == "action.completed" {
            return schedule_completed(event, queries);
        }
        Ok(ignored())
    }
}

fn schedule_triggered(
    event: &PluginEventEnvelope,
    _queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let trigger: ScheduleTriggered = serde_json::from_value(event.payload.clone())
        .map_err(|error| PluginError::Permanent(error.to_string()))?;
    let target = trigger
        .payload
        .get("target")
        .cloned()
        .ok_or_else(|| PluginError::Permanent("schedule target is missing".to_owned()))?;
    let content = trigger
        .payload
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Permanent("schedule content is missing".to_owned()))?;
    Ok(HandlerOutput {
        disposition: Disposition::Continue,
        state_ops: Vec::new(),
        commands: vec![plugin_command(
            event,
            "send",
            "message.send",
            json!({"target":target,"content":content}),
        )],
        diagnostics: Vec::new(),
    })
}

fn schedule_completed(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let completion: ActionCompleted = serde_json::from_value(event.payload.clone())
        .map_err(|error| PluginError::Permanent(error.to_string()))?;
    if completion.status != ActionStatus::Succeeded {
        if completion.kind == "message.send" {
            return cleanup_delivery_state(queries, &completion.source_event_id);
        }
        let relevant = matches!(
            completion.kind.as_str(),
            "schedule.create" | "schedule.cancel"
        );
        if relevant {
            let state_ops = (completion.kind == "schedule.create"
                && !matches!(
                    completion.status,
                    ActionStatus::Unknown | ActionStatus::TimedOut
                ))
            .then(|| plugin_api::StateOp::Delete {
                key: task_state_key(&format!(
                    "probe-{}",
                    hex_bytes(completion.source_event_id.as_bytes())
                )),
                expected_revision: None,
            })
            .into_iter()
            .collect();
            return Ok(reply(
                event,
                &format!("scheduler command {:?}", completion.status),
                state_ops,
            ));
        }
        return Ok(ignored());
    }
    let result = completion
        .result
        .ok_or_else(|| PluginError::Permanent("scheduler command result is missing".to_owned()))?;
    match completion.kind.as_str() {
        "schedule.create" => schedule_created(event, &result),
        "schedule.cancel" => schedule_cancelled(event, queries, &result),
        "message.send" => cleanup_delivery_state(queries, &completion.source_event_id),
        _ => Ok(ignored()),
    }
}

fn cleanup_delivery_state(
    queries: &dyn HostQueries,
    source_event_id: &str,
) -> Result<HandlerOutput, PluginError> {
    let task_id = source_event_id
        .rsplit('/')
        .next()
        .ok_or_else(|| PluginError::Permanent("schedule task ID is missing".to_owned()))?;
    Ok(HandlerOutput {
        disposition: Disposition::Continue,
        state_ops: vec![plugin_api::StateOp::Delete {
            key: task_state_key(task_id),
            expected_revision: queries
                .state_get(&task_state_key(task_id))
                .map(|state| state.revision),
        }],
        commands: Vec::new(),
        diagnostics: Vec::new(),
    })
}

fn schedule_created(
    event: &PluginEventEnvelope,
    result: &Value,
) -> Result<HandlerOutput, PluginError> {
    let task_id = required_str(result, "task_id", "schedule creation task ID")?;
    let created = result
        .get("created")
        .and_then(Value::as_bool)
        .ok_or_else(|| PluginError::Permanent("schedule creation status is missing".to_owned()))?;
    let run_at_ms = result
        .get("run_at_ms")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            PluginError::Permanent("schedule creation deadline is missing".to_owned())
        })?;
    let delay_seconds = run_at_ms
        .saturating_sub(event.received_at_ms)
        .saturating_add(999)
        / 1_000;
    let content = if created {
        format!("scheduled task {task_id} in {delay_seconds}s")
    } else {
        format!("task already exists: {task_id}")
    };
    Ok(reply(event, &content, Vec::new()))
}

fn schedule_cancelled(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    result: &Value,
) -> Result<HandlerOutput, PluginError> {
    let task_id = required_str(result, "task_id", "schedule cancellation task ID")?;
    let cancelled = result
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            PluginError::Permanent("schedule cancellation status is missing".to_owned())
        })?;
    let content = if cancelled {
        format!("cancelled task {task_id}")
    } else {
        format!("task not found or no longer pending: {task_id}")
    };
    let state_ops = vec![plugin_api::StateOp::Delete {
        key: task_state_key(task_id),
        expected_revision: queries
            .state_get(&task_state_key(task_id))
            .map(|state| state.revision),
    }];
    Ok(reply(event, &content, state_ops))
}

fn required_str<'a>(value: &'a Value, key: &str, label: &str) -> Result<&'a str, PluginError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Permanent(format!("{label} is missing")))
}

fn scheduler_message(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let Some(text) = message_text(event) else {
        return Ok(ignored());
    };
    if let Some(arguments) = command_tail(text, "/schedule") {
        if queries.state_scan("tasks/", MAX_PENDING_TASKS + 1).len() >= MAX_PENDING_TASKS {
            return Ok(reply(
                event,
                "scheduler task limit reached; cancel or wait for an existing task",
                Vec::new(),
            ));
        }
        let mut arguments = arguments.split_whitespace();
        let delay_seconds = match (arguments.next(), arguments.next()) {
            (None, None) => 5,
            (Some(value), None) => match value.parse::<i64>() {
                Ok(value @ 1..=86_400) => value,
                _ => return Ok(schedule_usage(event)),
            },
            _ => return Ok(schedule_usage(event)),
        };
        let task_id = format!("probe-{}", hex_bytes(event.event_id.as_bytes()));
        let target = event
            .payload
            .pointer("/data/target")
            .cloned()
            .ok_or_else(|| PluginError::Permanent("message target is missing".to_owned()))?;
        let create = ScheduleCreate {
            task_id: task_id.clone(),
            run_at_ms: event.received_at_ms.saturating_add(delay_seconds * 1_000),
            payload: json!({"target":target,"content":"scheduled task fired"}),
        };
        return Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: vec![plugin_api::StateOp::Put {
                key: task_state_key(&task_id),
                value: serde_json::to_vec(&task_owner(event)?)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
                expected_revision: None,
            }],
            commands: vec![plugin_command(
                event,
                "schedule",
                "schedule.create",
                serde_json::to_value(create)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
            )],
            diagnostics: Vec::new(),
        });
    }
    if let Some(task_id) = command_argument(text, "/schedule-cancel") {
        let key = task_state_key(task_id);
        let Some(state) = queries.state_get(&key) else {
            return Ok(reply(
                event,
                "task not found or no longer pending",
                Vec::new(),
            ));
        };
        let owner: Value = serde_json::from_slice(&state.value).map_err(|error| {
            PluginError::Permanent(format!("invalid task owner state: {error}"))
        })?;
        if owner != task_owner(event)? {
            return Ok(reply(
                event,
                "task belongs to a different sender or conversation",
                Vec::new(),
            ));
        }
        return Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: Vec::new(),
            commands: vec![plugin_command(
                event,
                "cancel",
                "schedule.cancel",
                serde_json::to_value(ScheduleCancel {
                    task_id: task_id.to_owned(),
                })
                .map_err(|error| PluginError::Permanent(error.to_string()))?,
            )],
            diagnostics: Vec::new(),
        });
    }
    Ok(ignored())
}

fn task_state_key(task_id: &str) -> String {
    format!("tasks/{task_id}")
}

fn task_owner(event: &PluginEventEnvelope) -> Result<Value, PluginError> {
    let sender = event
        .payload
        .pointer("/data/sender/id")
        .cloned()
        .ok_or_else(|| PluginError::Permanent("message sender is missing".to_owned()))?;
    let target = event
        .payload
        .pointer("/data/target")
        .cloned()
        .ok_or_else(|| PluginError::Permanent("message target is missing".to_owned()))?;
    Ok(json!({"sender": sender, "target": target}))
}

fn schedule_usage(event: &PluginEventEnvelope) -> HandlerOutput {
    reply(
        event,
        "usage: /schedule [delay-seconds: 1..86400]",
        Vec::new(),
    )
}
