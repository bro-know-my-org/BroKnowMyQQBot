wit_bindgen::generate!({
    path: "../../../crates/plugin-api/wit",
    world: "qq-plugin",
});

use std::{cell::RefCell, collections::VecDeque};

use bkm::plugin::{queries, types::*};
use exports::bkm::plugin::{
    lifecycle::Guest as Lifecycle,
    qq_handler::{self as qq, Guest as Handler},
};

struct Pager;

const BUTTON_ID: &str = "md-pages-next";
const ACK_ID: &str = "page-ack";
const MAX_PENDING_PAGES: usize = 64;
const PENDING_TTL_MS: i64 = 60_000;
const PAGES: [&str; 3] = [
    "这是一条带按钮的 QQ Markdown 消息。",
    "点击按钮会触发回调。",
    "翻页会在当前会话发送一条新消息。",
];

struct Pending {
    invocation_id: String,
    button: qq::ButtonClicked,
    page: usize,
    expires_at_ms: i64,
}

thread_local! {
    static PENDING: RefCell<VecDeque<Pending>> = const { RefCell::new(VecDeque::new()) };
}

impl Lifecycle for Pager {
    fn validate_config(_: Vec<ConfigEntry>) -> Result<(), PluginError> {
        Ok(())
    }

    fn init(_: InitContext) -> Result<(), PluginError> {
        PENDING.with_borrow_mut(VecDeque::clear);
        Ok(())
    }

    fn health() -> Result<HealthStatus, PluginError> {
        Ok(HealthStatus::Healthy)
    }

    fn migrate_state(_: u32, _: u32, _: Vec<StateEntry>) -> Result<MigrationOutput, PluginError> {
        Ok(MigrationOutput { state_ops: vec![] })
    }

    fn shutdown() -> Result<(), PluginError> {
        PENDING.with_borrow_mut(VecDeque::clear);
        Ok(())
    }
}

impl Handler for Pager {
    fn on_event(input: qq::Event) -> Result<qq::Output, PluginError> {
        Ok(match input {
            qq::Event::Core(event) => match event.payload {
                EventPayload::MessageCreated(message)
                    if queries::caller_authority().platform == "qq.official"
                        && queries::command_context().is_some_and(|command| {
                            command.name == "md-pages" && command.arguments.trim().is_empty()
                        }) =>
                {
                    output(
                        "first-page",
                        qq::QqCommandPayload::MarkdownReply(markdown(0, message.sender.id)),
                    )
                }
                EventPayload::ActionCompleted(result) => after_ack(result, event.received_at_ms),
                _ => ignored(),
            },
            qq::Event::Button(event) => clicked(event),
        })
    }
}

fn clicked(event: qq::ButtonEnvelope) -> qq::Output {
    let page = match (event.button.button_id.as_str(), event.button.data.as_str()) {
        (BUTTON_ID, "1") => 0,
        (BUTTON_ID, "2") => 1,
        (BUTTON_ID, "3") => 2,
        _ => return ignored(),
    };
    let response = PENDING.with_borrow_mut(|pages| {
        pages.retain(|entry| entry.expires_at_ms > event.received_at_ms);
        if pages.len() == MAX_PENDING_PAGES {
            return qq::AckResult::TooFrequent;
        }
        pages.push_back(Pending {
            invocation_id: event.invocation_id,
            button: event.button,
            page,
            expires_at_ms: event.received_at_ms.saturating_add(PENDING_TTL_MS),
        });
        qq::AckResult::Success
    });
    output(ACK_ID, qq::QqCommandPayload::ButtonAck(response))
}

fn after_ack(result: ActionCompleted, received_at_ms: i64) -> qq::Output {
    if result.kind != "qq.button.ack" || result.command_id != ACK_ID {
        return ignored();
    }
    let Some(pending) = PENDING.with_borrow_mut(|pages| {
        let index = pages
            .iter()
            .position(|pending| pending.invocation_id == result.source_invocation_id)?;
        pages.remove(index)
    }) else {
        return ignored();
    };
    if !matches!(result.status, ActionStatus::Succeeded) || pending.expires_at_ms <= received_at_ms
    {
        return ignored();
    }
    output(
        "next-page",
        qq::QqCommandPayload::MarkdownSend(qq::MarkdownSend {
            target: pending.button.target,
            message: markdown(pending.page, pending.button.actor_id),
        }),
    )
}

fn markdown(page: usize, actor_id: String) -> qq::Markdown {
    let next = (page + 1) % PAGES.len();
    qq::Markdown {
        body: qq::MarkdownBody::Content(format!(
            "## 第 {} / {} 页\n\n{}",
            page + 1,
            PAGES.len(),
            PAGES[page]
        )),
        keyboard: Some(qq::Keyboard::Inline(vec![vec![qq::Button {
            id: BUTTON_ID.to_owned(),
            label: if next == 0 {
                "返回首页"
            } else {
                "下一页"
            }
            .to_owned(),
            visited_label: None,
            style: qq::ButtonStyle::Blue,
            access: qq::ButtonAccess::Users(vec![actor_id]),
            action: qq::ButtonAction::Callback((next + 1).to_string()),
            unsupported_tips: None,
        }]])),
        force_verify_image_resource: false,
    }
}

fn output(command_id: &str, payload: qq::QqCommandPayload) -> qq::Output {
    qq::Output {
        disposition: Disposition::Continue,
        state_ops: vec![],
        diagnostics: vec![],
        commands: vec![qq::Command {
            command_id: command_id.to_owned(),
            idempotency_key: None,
            deadline_ms: None,
            payload,
        }],
    }
}

fn ignored() -> qq::Output {
    qq::Output {
        disposition: Disposition::Ignore,
        state_ops: vec![],
        diagnostics: vec![],
        commands: vec![],
    }
}

export!(Pager);
