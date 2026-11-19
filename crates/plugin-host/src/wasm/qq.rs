//! WIT projection for the opt-in BPP 1.7 QQ handler.

use plugin_api::{
    HandlerOutput, PluginCommand, PluginError, PluginEventEnvelope, QqButton, QqButtonAccess,
    QqButtonAck, QqButtonAction, QqButtonStyle, QqChat, QqKeyboard, QqMarkdown, QqMarkdownBody,
    QqMarkdownParam, QqMarkdownSend,
};

use super::{conversion, exports::bkm::plugin::qq_handler as wit, types};

pub(super) fn wit_event(event: &PluginEventEnvelope) -> Result<wit::Event, PluginError> {
    let Some(button) = event.qq_button_clicked()? else {
        return conversion::wit_event(event).map(wit::Event::Core);
    };
    Ok(wit::Event::Button(wit::ButtonEnvelope {
        protocol_version: event.protocol_version.clone(),
        event_id: event.event_id.clone(),
        delivery_id: event.delivery_id.clone(),
        invocation_id: event.invocation_id.clone(),
        occurred_at_ms: event.occurred_at_ms,
        received_at_ms: event.received_at_ms,
        adapter_id: event.adapter_id.clone(),
        trace_id: event.trace_id.clone(),
        button: wit::ButtonClicked {
            actor_id: button.actor_id,
            target: match button.target {
                QqChat::Group { group_id } => wit::QqChat::Group(group_id),
                QqChat::Private { user_id } => wit::QqChat::Private(user_id),
            },
            button_id: button.button_id,
            data: button.data,
        },
    }))
}

pub(super) fn handler_output(
    output: wit::Output,
    initial_click: bool,
) -> Result<HandlerOutput, PluginError> {
    conversion::validate_wit_commands(
        output.commands.len(),
        output
            .commands
            .iter()
            .filter_map(|command| match &command.payload {
                wit::QqCommandPayload::Core(payload) => Some(payload),
                _ => None,
            }),
        !initial_click,
    )?;
    let commands = output
        .commands
        .into_iter()
        .map(|command| qq_command(command, initial_click))
        .collect::<Result<_, _>>()?;
    Ok(conversion::handler_output_parts(
        output.disposition,
        output.state_ops,
        commands,
        output.diagnostics,
    ))
}

fn qq_command(command: wit::Command, initial_click: bool) -> Result<PluginCommand, PluginError> {
    use wit::QqCommandPayload;
    let id = command.command_id;
    let mut converted = match command.payload {
        QqCommandPayload::Core(payload) => {
            return conversion::convert_command(
                types::Command {
                    command_id: id,
                    idempotency_key: command.idempotency_key,
                    deadline_ms: command.deadline_ms,
                    payload,
                },
                !initial_click,
            );
        }
        QqCommandPayload::MarkdownReply(message) => {
            PluginCommand::qq_markdown_reply(id, &markdown(message))
        }
        QqCommandPayload::MarkdownSend(request) => PluginCommand::qq_markdown_send(
            id,
            &QqMarkdownSend {
                target: match request.target {
                    wit::QqChat::Group(group_id) => QqChat::Group { group_id },
                    wit::QqChat::Private(user_id) => QqChat::Private { user_id },
                },
                markdown: markdown(request.message),
            },
        ),
        QqCommandPayload::ButtonAck(code) => PluginCommand::qq_button_ack(
            id,
            match code {
                wit::AckResult::Success => QqButtonAck::Success,
                wit::AckResult::Failure => QqButtonAck::Failure,
                wit::AckResult::TooFrequent => QqButtonAck::TooFrequent,
                wit::AckResult::Duplicate => QqButtonAck::Duplicate,
                wit::AckResult::NoPermission => QqButtonAck::NoPermission,
                wit::AckResult::OnlyAdministrator => QqButtonAck::OnlyAdministrator,
            },
        ),
    };
    converted.idempotency_key = command.idempotency_key;
    converted.deadline_ms = command.deadline_ms;
    Ok(converted)
}

fn markdown(message: wit::Markdown) -> QqMarkdown {
    let body = match message.body {
        wit::MarkdownBody::Content(content) => QqMarkdownBody::Content { content },
        wit::MarkdownBody::Template(template) => QqMarkdownBody::Template {
            id: template.id,
            params: template
                .params
                .into_iter()
                .map(|param| QqMarkdownParam {
                    key: param.key,
                    values: param.values,
                })
                .collect(),
        },
    };
    QqMarkdown {
        body,
        keyboard: message.keyboard.map(|keyboard| match keyboard {
            wit::Keyboard::Template(id) => QqKeyboard::Template { id },
            wit::Keyboard::Inline(rows) => QqKeyboard::Inline {
                rows: rows
                    .into_iter()
                    .map(|row| row.into_iter().map(button).collect())
                    .collect(),
            },
        }),
        force_verify_image_resource: message.force_verify_image_resource,
    }
}

fn button(value: wit::Button) -> QqButton {
    QqButton {
        id: value.id,
        label: value.label,
        visited_label: value.visited_label,
        style: match value.style {
            wit::ButtonStyle::Gray => QqButtonStyle::Gray,
            wit::ButtonStyle::Blue => QqButtonStyle::Blue,
            wit::ButtonStyle::Red => QqButtonStyle::Red,
            wit::ButtonStyle::BlueFilled => QqButtonStyle::BlueFilled,
        },
        access: match value.access {
            wit::ButtonAccess::Everyone => QqButtonAccess::Everyone {},
            wit::ButtonAccess::Administrators => QqButtonAccess::Administrators {},
            wit::ButtonAccess::Users(ids) => QqButtonAccess::Users { ids },
        },
        action: match value.action {
            wit::ButtonAction::Link(url) => QqButtonAction::Link { url },
            wit::ButtonAction::Command(command) => QqButtonAction::Command {
                text: command.text,
                enter: command.enter,
                reply: command.reply,
            },
            wit::ButtonAction::Callback(data) => QqButtonAction::Callback { data },
        },
        unsupported_tips: value.unsupported_tips,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_with_browser_and_ack() -> wit::Output {
        wit::Output {
            disposition: types::Disposition::Continue,
            state_ops: Vec::new(),
            diagnostics: Vec::new(),
            commands: vec![
                wit::Command {
                    command_id: "browser".to_owned(),
                    deadline_ms: None,
                    idempotency_key: None,
                    payload: wit::QqCommandPayload::Core(types::CommandPayload::BrowserRun(
                        types::BrowserRunCommand {
                            steps: Vec::new(),
                            viewport: types::BrowserViewport {
                                width: 1280,
                                height: 720,
                                device_scale_factor: 1,
                            },
                            user_agent: None,
                            locale: None,
                            timezone: None,
                            color_scheme: None,
                            extra_headers: Vec::new(),
                        },
                    )),
                },
                wit::Command {
                    command_id: "ack".to_owned(),
                    deadline_ms: None,
                    idempotency_key: None,
                    payload: wit::QqCommandPayload::ButtonAck(wit::AckResult::Success),
                },
            ],
        }
    }

    #[test]
    fn forbidden_empty_browser_plan_survives_wit_conversion_for_an_independent_denial() {
        assert!(handler_output(output_with_browser_and_ack(), false).is_err());
        let output = handler_output(output_with_browser_and_ack(), true).unwrap();
        assert_eq!(output.commands[0].kind, "browser.run");
        assert_eq!(output.commands[0].payload["steps"], serde_json::json!([]));
        assert_eq!(
            output.commands[1],
            PluginCommand::qq_button_ack("ack", QqButtonAck::Success)
        );
    }
}
