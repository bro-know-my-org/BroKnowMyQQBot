//! Validated QQ commands and exclusive delivery of managed callbacks.

use std::sync::Arc;

use bot_core::{Action, ActionResult, AdapterId, ButtonInteraction, Context, MessageTarget};
use plugin_api::{
    ActionCompleted, ActionStatus, HandlerOutput, PluginCommand, PluginError, PluginEventEnvelope,
    QQ_BUTTON_ACK, QQ_BUTTON_CLICKED, QQ_MARKDOWN_REPLY, QQ_MARKDOWN_SEND, QqButtonAck,
    QqButtonClicked, QqChat, QqMarkdown, QqMarkdownSend,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    ExecutionOrigin, PluginHostError, RegisteredPlugin, StaticPluginHost, command,
    message_target_partition_key, qq_callbacks::interaction_deadline, qq_keyboard,
};

const QQ_PLATFORM: &str = "qq.official";

#[derive(Clone, Copy)]
pub(super) enum ButtonPhase {
    Other,
    Clicked,
    Acknowledged,
}

impl ButtonPhase {
    pub(super) fn for_event(event: &PluginEventEnvelope) -> Self {
        match event.event_type.as_str() {
            QQ_BUTTON_CLICKED => Self::Clicked,
            "action.completed" if successful_ack(&event.payload) => Self::Acknowledged,
            _ => Self::Other,
        }
    }
}

fn successful_ack(payload: &Value) -> bool {
    payload
        .get("kind")
        .is_some_and(|kind| kind == QQ_BUTTON_ACK)
        && serde_json::from_value::<ActionCompleted>(payload.clone()).is_ok_and(|completion| {
            completion.status == ActionStatus::Succeeded
                && completion.result.is_some_and(|result| result["code"] == 0)
        })
}

pub(super) fn validate_origin_command(
    origin: ExecutionOrigin<'_>,
    phase: ButtonPhase,
    kind: &str,
) -> Result<(), PluginError> {
    if matches!(phase, ButtonPhase::Clicked) && kind != QQ_BUTTON_ACK {
        return Err(PluginError::PermissionDenied(
            "the initial QQ button event permits only an ACK command".to_owned(),
        ));
    }
    if origin.is_button_callback()
        && matches!(
            kind,
            "message.reply" | "message.send" | "media.reply" | "media.send" | "schedule.create"
        )
    {
        return Err(PluginError::PermissionDenied(
            "QQ callbacks send only through qq.markdown.send after successful ACK".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_commands(
    plugin: &RegisteredPlugin,
    output: &HandlerOutput,
    initial_click: bool,
) -> Result<(), &'static str> {
    let mut ack = false;
    for command in &output.commands {
        let markdown = match command.kind.as_str() {
            QQ_BUTTON_ACK => {
                if ack {
                    return Err("QQ button event permits a single ACK command");
                }
                decode::<QqButtonAck>(&command.payload)?;
                ack = true;
                continue;
            }
            _ if initial_click => continue,
            QQ_MARKDOWN_REPLY => decode::<QqMarkdown>(&command.payload)?,
            QQ_MARKDOWN_SEND => {
                let request = decode::<QqMarkdownSend>(&command.payload)?;
                request.markdown.validate_target(&request.target)?;
                request.markdown
            }
            _ => continue,
        };
        markdown.validate()?;
        if markdown
            .keyboard
            .as_ref()
            .is_some_and(|keyboard| qq_keyboard::callback_count(keyboard) > 0)
            && (!plugin.granted_capabilities.contains(QQ_BUTTON_ACK)
                || !plugin
                    .manifest
                    .subscriptions
                    .iter()
                    .any(|sub| sub.event == QQ_BUTTON_CLICKED))
        {
            return Err(
                "QQ callback buttons require qq.button.ack and a qq.button.clicked subscription",
            );
        }
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(payload: &Value) -> Result<T, &'static str> {
    serde_json::from_value(payload.clone()).map_err(|_| "invalid QQ command payload")
}

impl StaticPluginHost {
    pub(super) async fn dispatch_button(
        &self,
        context: &Context,
        button: &ButtonInteraction,
    ) -> Result<(), PluginHostError> {
        if context.platform() != QQ_PLATFORM {
            return Ok(());
        }
        let Some(target) = chat_target(&button.target) else {
            return Ok(());
        };
        let (Some(mut issued), Some(deadline)) = (
            self.qq_callbacks.owner(context, button, &target),
            interaction_deadline(context),
        ) else {
            return Ok(());
        };
        let Some(plugin) = issued.owner.upgrade() else {
            return Ok(());
        };
        if !self
            .plugins
            .iter()
            .any(|active| Arc::ptr_eq(active, &plugin))
            || !plugin.granted_capabilities.contains(QQ_BUTTON_ACK)
            || plugin
                .priority_for(QQ_BUTTON_CLICKED, Some(chat_scope(&target)))
                .is_none()
            || !plugin.is_ready_generation(issued.generation)?
        {
            return Ok(());
        }
        let Some(expires) = issued.wait_until_published(deadline).await else {
            return Ok(());
        };
        let Ok(execution) = tokio::time::timeout_at(
            deadline,
            plugin
                .execution
                .acquire(message_target_partition_key(&button.target)),
        )
        .await
        else {
            return Ok(());
        };
        let _execution = execution?;
        if !plugin.is_ready_generation(issued.generation)?
            || expires <= tokio::time::Instant::now()
            || !self.qq_callbacks.admit(context, button, deadline)
        {
            return Ok(());
        }
        let public = QqButtonClicked {
            actor_id: button.actor_id.clone(),
            button_id: button.button_id.clone(),
            target,
            data: issued.data,
        };
        let envelope = Self::event_envelope(context, QQ_BUTTON_CLICKED, &public)?;
        self.deliver_with_retry(&plugin, ExecutionOrigin::Event(context), envelope)
            .await?;
        Ok(())
    }

    pub(super) async fn execute_qq_command(
        &self,
        plugin: &Arc<RegisteredPlugin>,
        origin: ExecutionOrigin<'_>,
        event_id: &str,
        phase: ButtonPhase,
        command: &PluginCommand,
    ) -> Result<Value, PluginError> {
        let invalid = |message: &str| PluginError::Permanent(message.to_owned());
        let denied = |message: &str| PluginError::PermissionDenied(message.to_owned());
        let context = match origin {
            ExecutionOrigin::Event(context) if context.platform() == QQ_PLATFORM => Some(context),
            ExecutionOrigin::Scheduled { .. } if command.kind == QQ_MARKDOWN_SEND => None,
            _ => {
                return Err(denied(
                    "QQ commands require a live QQ event or scheduled send",
                ));
            }
        };
        if command.kind == QQ_BUTTON_ACK {
            return self
                .acknowledge_button(context, event_id, phase, &command.payload)
                .await;
        }
        let (target, reply_to, markdown) = markdown_arguments(context, phase, command)?;
        markdown.validate_target(&target).map_err(invalid)?;
        let adapter_id = AdapterId::new(origin.adapter_id());
        let generation = plugin
            .ready_generation()
            .map_err(|_| denied("QQ plugin lifecycle unavailable"))?
            .ok_or_else(|| denied("QQ plugin is no longer ready"))?;
        let (keyboard, pending) = if let Some(keyboard) = markdown.keyboard {
            if qq_keyboard::callback_count(&keyboard) > 0
                && plugin
                    .priority_for(QQ_BUTTON_CLICKED, Some(chat_scope(&target)))
                    .is_none()
            {
                return Err(denied(
                    "QQ callback buttons require a subscription for this conversation type",
                ));
            }
            let (wire, pending) = self.qq_callbacks.prepare_keyboard(
                keyboard,
                plugin,
                generation,
                &adapter_id,
                &target,
            )?;
            (Some(wire), pending)
        } else {
            (None, None)
        };
        let action = Action::Platform {
            name: "qq.message.markdown.once".to_owned(),
            payload: json!({
                "target": target,
                "body": qq_keyboard::markdown_body(markdown.body, markdown.force_verify_image_resource),
                "keyboard": keyboard,
                "reply_to": reply_to,
            }),
        };
        let ActionResult { message_id, .. } = if let Some(context) = context {
            context
                .execute_with_event_id(event_id, action)
                .await
                .map_err(|error| command::context_command_error(&error))?
        } else {
            let adapter = self
                .adapters
                .get(origin.adapter_id())
                .filter(|adapter| adapter.platform() == QQ_PLATFORM)
                .ok_or_else(|| denied("scheduled Markdown requires the configured QQ adapter"))?;
            bot_core::execute_traced_action(adapter.as_ref(), event_id, action)
                .await
                .map_err(|error| command::adapter_command_error(&error))?
        };
        if let Some(pending) = pending {
            pending.publish();
        }
        Ok(json!({ "message_id": message_id }))
    }

    async fn acknowledge_button(
        &self,
        context: Option<&Context>,
        event_id: &str,
        phase: ButtonPhase,
        payload: &Value,
    ) -> Result<Value, PluginError> {
        let wrong_event =
            || PluginError::PermissionDenied("QQ ACK requires the current button event".to_owned());
        if !matches!(phase, ButtonPhase::Clicked) {
            return Err(wrong_event());
        }
        let context = context.ok_or_else(wrong_event)?;
        let button = context.button_interaction().ok_or_else(wrong_event)?;
        let ack = decode::<QqButtonAck>(payload)
            .map_err(|error| PluginError::Permanent(error.to_owned()))?;
        let deadline = self.qq_callbacks.take_ack(context, button).ok_or_else(|| {
            PluginError::PermissionDenied(
                "QQ button event expired or was already acknowledged".to_owned(),
            )
        })?;
        let action = Action::Platform {
            name: "qq.interaction.respond.managed".to_owned(),
            payload: json!({ "interaction_id": button.interaction_id, "code": ack.code() }),
        };
        tokio::time::timeout_at(deadline, context.execute_with_event_id(event_id, action))
            .await
            .map_err(|_| {
                PluginError::Transient("QQ ACK deadline elapsed during the attempt".to_owned())
            })?
            .map_err(|error| command::context_command_error(&error))?;
        Ok(json!({ "code": ack.code() }))
    }
}

fn markdown_arguments<'a>(
    context: Option<&'a Context>,
    phase: ButtonPhase,
    command: &PluginCommand,
) -> Result<(QqChat, Option<&'a str>, QqMarkdown), PluginError> {
    let invalid = |message: &str| PluginError::Permanent(message.to_owned());
    let denied = |message: &str| PluginError::PermissionDenied(message.to_owned());
    let args = if command.kind == QQ_MARKDOWN_REPLY {
        let context = context.ok_or_else(|| denied("QQ Markdown reply requires a message"))?;
        let target = context
            .reply_target()
            .and_then(chat_target)
            .ok_or_else(|| denied("QQ Markdown reply requires a group or C2C message"))?;
        let reply_to = context
            .source_message_id()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| denied("QQ Markdown reply requires the original message ID"))?;
        (
            target,
            Some(reply_to),
            decode(&command.payload).map_err(invalid)?,
        )
    } else {
        let request = decode::<QqMarkdownSend>(&command.payload).map_err(invalid)?;
        if let Some(button) = context.and_then(Context::button_interaction) {
            if !matches!(phase, ButtonPhase::Acknowledged)
                || chat_target(&button.target).as_ref() != Some(&request.target)
            {
                return Err(denied(
                    "a QQ callback may send only to its own conversation after successful ACK",
                ));
            }
        }
        (request.target, None, request.markdown)
    };
    Ok(args)
}

fn chat_target(target: &MessageTarget) -> Option<QqChat> {
    match target {
        MessageTarget::Group { group_id } => Some(QqChat::Group {
            group_id: group_id.clone(),
        }),
        MessageTarget::Private { user_id } => Some(QqChat::Private {
            user_id: user_id.clone(),
        }),
        _ => None,
    }
}

const fn chat_scope(target: &QqChat) -> &'static str {
    match target {
        QqChat::Group { .. } => "group",
        QqChat::Private { .. } => "private",
    }
}
