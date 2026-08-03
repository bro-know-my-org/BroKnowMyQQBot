use super::super::{
    ActionCompleted, ActionStatus, BTreeSet, CommandDeclaration, Disposition, HandlerOutput,
    HostQueries, PluginError, PluginEventEnvelope, PluginManifest, ScheduleCancel, ScheduleCreate,
    ScheduleTriggered, StateOp, StaticPlugin, Subscription, Value, async_trait, command_tail,
    hex_bytes, ignored, json, message_plugin_manifest, message_text, plugin_command, reply,
};
use sha2::{Digest as _, Sha256};

const TASK_PREFIX: &str = "tasks/";
const TASK_INDEX_KEY: &str = "task-index";
const MAX_TASKS: usize = 128;
const MAX_LISTED_TASKS: usize = 20;
const MAX_DELAY_SECONDS: i64 = 365 * 24 * 60 * 60;
const MAX_CONTENT_CHARS: usize = 500;

#[derive(Debug)]
pub struct ReminderPlugin {
    manifest: PluginManifest,
}

impl Default for ReminderPlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.reminder",
            "Reminder",
            "remind",
            "message.reply",
            true,
        );
        manifest.commands.extend([
            command("reminders", "List your pending reminders"),
            command("remind-cancel", "Cancel one of your reminders"),
        ]);
        manifest.subscriptions.extend([
            Subscription {
                id: "reminder-triggers".to_owned(),
                event: "schedule.triggered".to_owned(),
                priority: 0,
                scopes: BTreeSet::new(),
            },
            Subscription {
                id: "reminder-results".to_owned(),
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
impl StaticPlugin for ReminderPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        match event.event_type.as_str() {
            "message.created" => reminder_message(event, queries),
            "schedule.triggered" => reminder_triggered(event, queries),
            "action.completed" => reminder_completed(event, queries),
            _ => Ok(ignored()),
        }
    }
}

fn reminder_message(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let Some(text) = message_text(event) else {
        return Ok(ignored());
    };
    if let Some(arguments) = command_tail(text, "/remind") {
        return create_reminder(event, queries, arguments);
    }
    if command_tail(text, "/reminders") == Some("") {
        return list_reminders(event, queries);
    }
    if let Some(task_id) = command_tail(text, "/remind-cancel") {
        return cancel_reminder(event, queries, task_id);
    }
    Ok(ignored())
}

fn create_reminder(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    arguments: &str,
) -> Result<HandlerOutput, PluginError> {
    let Some((delay, content)) = arguments.split_once(char::is_whitespace) else {
        return Ok(reminder_usage(event));
    };
    let Ok(delay_seconds @ 1..=MAX_DELAY_SECONDS) = delay.parse::<i64>() else {
        return Ok(reminder_usage(event));
    };
    let content = content.trim();
    if content.is_empty() || content.chars().count() > MAX_CONTENT_CHARS {
        return Ok(reminder_usage(event));
    }
    let owner = task_owner(event)?;
    let task_id = reminder_task_id(&event.event_id);
    let (mut task_ids, index_revision) = task_index(queries)?;
    if task_ids.iter().any(|existing| existing == &task_id) {
        return Ok(ignored());
    }
    if task_ids.len() >= MAX_TASKS {
        return Ok(reply(event, "reminder limit reached", Vec::new()));
    }
    task_ids.push(task_id.clone());
    let run_at_ms = event
        .received_at_ms
        .saturating_add(delay_seconds.saturating_mul(1_000));
    let task = json!({
        "task_id":task_id,
        "owner":owner,
        "target":event.payload.pointer("/data/target"),
        "content":content,
        "run_at_ms":run_at_ms
    });
    let schedule = ScheduleCreate {
        task_id: task_id.clone(),
        run_at_ms,
        payload: json!({
            "target":task["target"],
            "content":content
        }),
    };
    Ok(HandlerOutput {
        disposition: Disposition::Continue,
        state_ops: vec![
            StateOp::Put {
                key: task_key(&task_id),
                value: serde_json::to_vec(&task)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
                expected_revision: None,
            },
            StateOp::Put {
                key: TASK_INDEX_KEY.to_owned(),
                value: serde_json::to_vec(&task_ids)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
                expected_revision: index_revision,
            },
        ],
        commands: vec![
            plugin_command(
                event,
                "schedule",
                "schedule.create",
                serde_json::to_value(schedule)
                    .map_err(|error| PluginError::Permanent(error.to_string()))?,
            ),
            plugin_command(
                event,
                "reply",
                "message.reply",
                json!({"content":format!("reminder {task_id} scheduled in {delay_seconds}s")}),
            ),
        ],
        diagnostics: Vec::new(),
    })
}

fn list_reminders(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let owner = task_owner(event)?;
    let mut reminders = queries
        .state_scan(TASK_PREFIX, MAX_TASKS)
        .into_iter()
        .filter_map(|(_, state)| serde_json::from_slice::<Value>(&state.value).ok())
        .filter(|task| task.get("owner") == Some(&owner))
        .collect::<Vec<_>>();
    reminders.sort_by_key(|task| task.get("run_at_ms").and_then(Value::as_i64));
    let lines = reminders
        .iter()
        .take(MAX_LISTED_TASKS)
        .filter_map(|task| {
            Some(format!(
                "{} @ {} [{}] — {}",
                task.get("task_id")?.as_str()?,
                task.get("run_at_ms")?.as_i64()?,
                task.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("scheduled"),
                task.get("content")?.as_str()?
            ))
        })
        .collect::<Vec<_>>();
    let content = if lines.is_empty() {
        "no pending reminders".to_owned()
    } else {
        format!("pending reminders:\n{}", lines.join("\n"))
    };
    Ok(reply(event, &content, Vec::new()))
}

fn cancel_reminder(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    task_id: &str,
) -> Result<HandlerOutput, PluginError> {
    if task_id.is_empty() || task_id.len() > 256 {
        return Ok(reminder_usage(event));
    }
    let key = task_key(task_id);
    let Some(state) = queries.state_get(&key) else {
        return Ok(reply(event, "reminder not found", Vec::new()));
    };
    let task: Value = serde_json::from_slice(&state.value)
        .map_err(|error| PluginError::Permanent(format!("invalid reminder state: {error}")))?;
    if task.get("owner") != Some(&task_owner(event)?) {
        return Ok(reply(
            event,
            "reminder belongs to another sender or conversation",
            Vec::new(),
        ));
    }
    if task.get("status").and_then(Value::as_str) == Some("failed") {
        return Ok(reply(
            event,
            "failed reminder removed",
            remove_task_operations(queries, task_id)?,
        ));
    }
    Ok(HandlerOutput {
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
    })
}

fn reminder_triggered(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let trigger: ScheduleTriggered = serde_json::from_value(event.payload.clone())
        .map_err(|error| PluginError::Permanent(error.to_string()))?;
    let target = trigger
        .payload
        .get("target")
        .cloned()
        .ok_or_else(|| PluginError::Permanent("reminder target is missing".to_owned()))?;
    let content = trigger
        .payload
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Permanent("reminder content is missing".to_owned()))?;
    let key = task_key(&trigger.task_id);
    let state = queries
        .state_get(&key)
        .ok_or_else(|| PluginError::Permanent("reminder state is missing".to_owned()))?;
    let mut task: Value = serde_json::from_slice(&state.value)
        .map_err(|error| PluginError::Permanent(format!("invalid reminder state: {error}")))?;
    task.as_object_mut()
        .ok_or_else(|| PluginError::Permanent("reminder state is not an object".to_owned()))?
        .insert("status".to_owned(), Value::String("delivering".to_owned()));
    Ok(HandlerOutput {
        disposition: Disposition::Continue,
        state_ops: vec![StateOp::Put {
            key,
            value: serde_json::to_vec(&task)
                .map_err(|error| PluginError::Permanent(error.to_string()))?,
            expected_revision: Some(state.revision),
        }],
        commands: vec![plugin_command(
            event,
            "send",
            "message.send",
            json!({"target":target,"content":content}),
        )],
        diagnostics: Vec::new(),
    })
}

fn reminder_completed(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
) -> Result<HandlerOutput, PluginError> {
    let completion: ActionCompleted = serde_json::from_value(event.payload.clone())
        .map_err(|error| PluginError::Permanent(error.to_string()))?;
    match completion.kind.as_str() {
        "schedule.create"
            if matches!(
                completion.status,
                ActionStatus::Unknown | ActionStatus::TimedOut
            ) =>
        {
            Ok(reply(
                event,
                "reminder scheduling result is unknown; task retained for recovery",
                Vec::new(),
            ))
        }
        "schedule.create" if completion.status != ActionStatus::Succeeded => {
            let task_id = reminder_task_id(&completion.source_event_id);
            Ok(reply(
                event,
                &format!("failed to schedule reminder: {:?}", completion.status),
                remove_task_operations(queries, &task_id)?,
            ))
        }
        "schedule.cancel" => complete_cancellation(event, queries, &completion),
        "message.send" => complete_delivery(queries, &completion),
        _ => Ok(ignored()),
    }
}

fn complete_cancellation(
    event: &PluginEventEnvelope,
    queries: &dyn HostQueries,
    completion: &ActionCompleted,
) -> Result<HandlerOutput, PluginError> {
    if completion.status != ActionStatus::Succeeded {
        return Ok(reply(
            event,
            &format!("failed to cancel reminder: {:?}", completion.status),
            Vec::new(),
        ));
    }
    let result = completion
        .result
        .as_ref()
        .ok_or_else(|| PluginError::Permanent("cancellation result is missing".to_owned()))?;
    let task_id = result
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::Permanent("cancelled task ID is missing".to_owned()))?;
    let cancelled = result
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(|| PluginError::Permanent("cancellation status is missing".to_owned()))?;
    let state_ops = if cancelled {
        remove_task_operations(queries, task_id)?
    } else {
        Vec::new()
    };
    Ok(reply(
        event,
        if cancelled {
            "reminder cancelled"
        } else {
            "reminder was no longer pending"
        },
        state_ops,
    ))
}

fn complete_delivery(
    queries: &dyn HostQueries,
    completion: &ActionCompleted,
) -> Result<HandlerOutput, PluginError> {
    let task_id = completion
        .source_event_id
        .rsplit('/')
        .next()
        .filter(|task_id| task_id.starts_with("reminder-"))
        .ok_or_else(|| PluginError::Permanent("delivery task ID is missing".to_owned()))?;
    if completion.status == ActionStatus::Succeeded {
        return Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: remove_task_operations(queries, task_id)?,
            commands: Vec::new(),
            diagnostics: Vec::new(),
        });
    }
    let key = task_key(task_id);
    let Some(state) = queries.state_get(&key) else {
        return Ok(ignored());
    };
    let mut task: Value = serde_json::from_slice(&state.value)
        .map_err(|error| PluginError::Permanent(format!("invalid reminder state: {error}")))?;
    let object = task
        .as_object_mut()
        .ok_or_else(|| PluginError::Permanent("reminder state is not an object".to_owned()))?;
    object.insert("status".to_owned(), Value::String("failed".to_owned()));
    object.insert(
        "failure_status".to_owned(),
        Value::String(format!("{:?}", completion.status)),
    );
    Ok(HandlerOutput {
        disposition: Disposition::Continue,
        state_ops: vec![StateOp::Put {
            key,
            value: serde_json::to_vec(&task)
                .map_err(|error| PluginError::Permanent(error.to_string()))?,
            expected_revision: Some(state.revision),
        }],
        commands: Vec::new(),
        diagnostics: Vec::new(),
    })
}

fn task_index(queries: &dyn HostQueries) -> Result<(Vec<String>, Option<u64>), PluginError> {
    let Some(state) = queries.state_get(TASK_INDEX_KEY) else {
        return Ok((Vec::new(), None));
    };
    let task_ids = serde_json::from_slice(&state.value)
        .map_err(|error| PluginError::Permanent(format!("invalid reminder index: {error}")))?;
    Ok((task_ids, Some(state.revision)))
}

fn remove_task_operations(
    queries: &dyn HostQueries,
    task_id: &str,
) -> Result<Vec<StateOp>, PluginError> {
    let (mut task_ids, index_revision) = task_index(queries)?;
    task_ids.retain(|existing| existing != task_id);
    let mut operations = Vec::new();
    if let Some(state) = queries.state_get(&task_key(task_id)) {
        operations.push(StateOp::Delete {
            key: task_key(task_id),
            expected_revision: Some(state.revision),
        });
    }
    operations.push(StateOp::Put {
        key: TASK_INDEX_KEY.to_owned(),
        value: serde_json::to_vec(&task_ids)
            .map_err(|error| PluginError::Permanent(error.to_string()))?,
        expected_revision: index_revision,
    });
    Ok(operations)
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
    Ok(json!({"sender":sender,"target":target}))
}

fn task_key(task_id: &str) -> String {
    format!("{TASK_PREFIX}{task_id}")
}

fn reminder_task_id(event_id: &str) -> String {
    format!(
        "reminder-{}",
        hex_bytes(&Sha256::digest(event_id.as_bytes()))
    )
}

fn command(name: &str, description: &str) -> CommandDeclaration {
    CommandDeclaration {
        name: name.to_owned(),
        aliases: Vec::new(),
        description: description.to_owned(),
    }
}

fn reminder_usage(event: &PluginEventEnvelope) -> HandlerOutput {
    reply(
        event,
        "usage: /remind <seconds: 1..31536000> <text>; /reminders; /remind-cancel <task-id>",
        Vec::new(),
    )
}
