//! An opt-in, sender-bound QQ Markdown paging example.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::Mutex,
};

use plugin_api::{
    ActionCompleted, ActionStatus, HandlerOutput, HostQueries, PluginCommand, PluginError,
    PluginEventEnvelope, PluginManifest, QQ_BUTTON_ACK, QQ_BUTTON_CLICKED, QQ_MARKDOWN_REPLY,
    QQ_MARKDOWN_SEND, QqButton, QqButtonAccess, QqButtonAck, QqButtonAction, QqButtonClicked,
    QqButtonStyle, QqChat, QqKeyboard, QqMarkdown, QqMarkdownBody, QqMarkdownSend, StaticPlugin,
    Subscription,
};

use crate::{async_trait, ignored, is_command, message_plugin_manifest};

const COMMAND: &str = "md-pages";
const BUTTON_ID: &str = "md-pages-next";
const ACK_COMMAND_ID: &str = "page-ack";
const MAX_PENDING_PAGES: usize = 64;
const PENDING_TTL_MS: i64 = 60_000;
const PAGES: [&str; 3] = [
    "这是一条带按钮的 QQ Markdown 消息。",
    "点击按钮会触发回调。",
    "翻页会在当前会话发送一条新消息。",
];

#[derive(Debug)]
pub struct QqMarkdownPagesPlugin {
    manifest: PluginManifest,
    pending: Mutex<VecDeque<PendingPage>>,
}

#[derive(Debug)]
struct PendingPage {
    invocation_id: String,
    target: QqChat,
    actor_id: String,
    page: usize,
    expires_at_ms: i64,
}

impl Default for QqMarkdownPagesPlugin {
    fn default() -> Self {
        let mut manifest = message_plugin_manifest(
            "dev.bkm.qq-markdown-pages",
            "QQ Markdown Pages",
            COMMAND,
            QQ_MARKDOWN_REPLY,
            false,
        );
        ">=1.7,<2.0".clone_into(&mut manifest.protocol);
        "试用 QQ Markdown 按钮翻页".clone_into(&mut manifest.commands[0].description);
        manifest.subscriptions[0].scopes.remove("channel");
        manifest.subscriptions.extend([
            Subscription {
                id: "button-clicks".to_owned(),
                event: QQ_BUTTON_CLICKED.to_owned(),
                priority: 0,
                scopes: BTreeSet::new(),
            },
            Subscription {
                id: "action-completions".to_owned(),
                event: "action.completed".to_owned(),
                priority: 0,
                scopes: BTreeSet::new(),
            },
        ]);
        manifest
            .permissions
            .actions
            .extend([QQ_BUTTON_ACK.to_owned(), QQ_MARKDOWN_SEND.to_owned()]);
        Self {
            manifest,
            pending: Mutex::default(),
        }
    }
}

#[async_trait]
impl StaticPlugin for QqMarkdownPagesPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if let Some(click) = event.qq_button_clicked()? {
            return Ok(self.acknowledge(event, click));
        }
        Ok(match event.event_type.as_str() {
            "message.created" => first_page(event, queries),
            "action.completed" => self.after_ack(event),
            _ => ignored(),
        })
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        self.pending.lock().expect("paging queue poisoned").clear();
        Ok(())
    }
}

impl QqMarkdownPagesPlugin {
    fn acknowledge(&self, event: &PluginEventEnvelope, click: QqButtonClicked) -> HandlerOutput {
        let page = match (click.button_id.as_str(), click.data.as_str()) {
            (BUTTON_ID, "2") => 1,
            (BUTTON_ID, "3") => 2,
            (BUTTON_ID, "1") => 0,
            _ => return ignored(),
        };
        let mut pending = self.pending.lock().expect("paging queue poisoned");
        pending.retain(|entry| entry.expires_at_ms > event.received_at_ms);
        if pending.len() == MAX_PENDING_PAGES {
            return output(PluginCommand::qq_button_ack(
                ACK_COMMAND_ID,
                QqButtonAck::TooFrequent,
            ));
        }
        pending.push_back(PendingPage {
            invocation_id: event.invocation_id.clone(),
            target: click.target,
            actor_id: click.actor_id,
            page,
            expires_at_ms: event.received_at_ms.saturating_add(PENDING_TTL_MS),
        });
        output(PluginCommand::qq_button_ack(
            ACK_COMMAND_ID,
            QqButtonAck::Success,
        ))
    }

    fn after_ack(&self, event: &PluginEventEnvelope) -> HandlerOutput {
        let Ok(completed) = serde_json::from_value::<ActionCompleted>(event.payload.clone()) else {
            return ignored();
        };
        if completed.kind != QQ_BUTTON_ACK || completed.command_id != ACK_COMMAND_ID {
            return ignored();
        }
        let mut pending = self.pending.lock().expect("paging queue poisoned");
        let Some(index) = pending
            .iter()
            .position(|page| page.invocation_id == completed.source_invocation_id)
        else {
            return ignored();
        };
        let page = pending.remove(index).expect("matched a pending page");
        drop(pending);
        if completed.status != ActionStatus::Succeeded || page.expires_at_ms <= event.received_at_ms
        {
            return ignored();
        }
        output(PluginCommand::qq_markdown_send(
            "next-page",
            &QqMarkdownSend {
                target: page.target,
                markdown: markdown(page.page, page.actor_id),
            },
        ))
    }
}

fn first_page(event: &PluginEventEnvelope, queries: &dyn HostQueries) -> HandlerOutput {
    if !is_command(queries.command_context().as_ref(), COMMAND)
        || queries.caller_authority().platform != "qq.official"
    {
        return ignored();
    }
    let Some(target) = event.payload.pointer("/data/target") else {
        return ignored();
    };
    if !serde_json::from_value::<QqChat>(target.clone())
        .is_ok_and(|target| target.validate().is_ok())
    {
        return ignored();
    }
    let Some(actor_id) = event
        .payload
        .pointer("/data/sender/id")
        .and_then(serde_json::Value::as_str)
    else {
        return ignored();
    };
    let first = markdown(0, actor_id.to_owned());
    if first.validate().is_err() {
        return ignored();
    }
    output(PluginCommand::qq_markdown_reply("first-page", &first))
}

fn markdown(page: usize, actor_id: String) -> QqMarkdown {
    let next = (page + 1) % PAGES.len();
    QqMarkdown {
        body: QqMarkdownBody::Content {
            content: format!("## 第 {} / {} 页\n\n{}", page + 1, PAGES.len(), PAGES[page]),
        },
        keyboard: Some(QqKeyboard::Inline {
            rows: vec![vec![QqButton {
                id: BUTTON_ID.to_owned(),
                label: if next == 0 {
                    "返回首页"
                } else {
                    "下一页"
                }
                .to_owned(),
                visited_label: None,
                style: QqButtonStyle::Blue,
                access: QqButtonAccess::Users {
                    ids: vec![actor_id],
                },
                action: QqButtonAction::Callback {
                    data: (next + 1).to_string(),
                },
                unsupported_tips: None,
            }]],
        }),
        force_verify_image_resource: false,
    }
}

fn output(command: PluginCommand) -> HandlerOutput {
    HandlerOutput {
        commands: vec![command],
        ..HandlerOutput::default()
    }
}

#[cfg(test)]
mod tests {
    use plugin_api::{CallerAuthority, CommandContext, StateValue};
    use serde_json::{Value, json};

    use super::*;

    struct Queries(&'static str, Option<&'static str>);

    impl HostQueries for Queries {
        fn config_get(&self, _key: &str) -> Option<&Value> {
            None
        }

        fn state_get(&self, _key: &str) -> Option<&StateValue> {
            None
        }

        fn state_scan(&self, _prefix: &str, _limit: usize) -> Vec<(&str, &StateValue)> {
            Vec::new()
        }

        fn granted_capabilities(&self) -> &BTreeSet<String> {
            static EMPTY: BTreeSet<String> = BTreeSet::new();
            &EMPTY
        }

        fn invocation_time_ms(&self) -> i64 {
            0
        }

        fn caller_authority(&self) -> CallerAuthority {
            CallerAuthority {
                platform: self.0.to_owned(),
                ..CallerAuthority::default()
            }
        }

        fn command_context(&self) -> Option<CommandContext> {
            self.1.and_then(|text| CommandContext::parse("！", text))
        }
    }

    fn event(invocation: &str, event_type: &str, payload: Value) -> PluginEventEnvelope {
        PluginEventEnvelope {
            protocol_version: "1.7.0".to_owned(),
            event_id: format!("event-{invocation}"),
            delivery_id: format!("delivery-{invocation}"),
            invocation_id: invocation.to_owned(),
            occurred_at_ms: None,
            received_at_ms: 0,
            adapter_id: "qq".to_owned(),
            event_type: event_type.to_owned(),
            trace_id: None,
            payload,
            extensions: Vec::new(),
        }
    }

    #[tokio::test]
    async fn first_page_uses_message_command_context_and_binds_sender() {
        let plugin = QqMarkdownPagesPlugin::default();
        plugin.manifest().validate().unwrap();
        let message = event(
            "request",
            "message.created",
            json!({"type":"message", "data": {
                "target": {"scope":"group", "group_id":"group-a"},
                "sender": {"id":"member-a"},
                "text": "！md-pages"
            }}),
        );
        let result = plugin
            .on_event(&message, &Queries("qq.official", Some("！md-pages")))
            .await
            .unwrap();
        let [reply] = result.commands.as_slice() else {
            panic!("expected one Markdown reply");
        };
        assert_eq!(reply.kind, QQ_MARKDOWN_REPLY);
        assert_eq!(
            reply.payload["keyboard"]["rows"][0][0]["action"],
            json!({"type":"callback", "data":"2"})
        );
        assert_eq!(
            reply.payload["keyboard"]["rows"][0][0]["access"],
            json!({"type":"users", "ids":["member-a"]})
        );
        for queries in [
            Queries("qq.official", None),
            Queries("qq.official", Some("！md-pages 2")),
            Queries("onebot11", Some("！md-pages")),
        ] {
            let ignored = plugin.on_event(&message, &queries).await.unwrap();
            assert_eq!(ignored.commands, []);
        }
    }

    #[tokio::test]
    async fn only_a_matching_successful_ack_sends_the_next_page_once() {
        let plugin = QqMarkdownPagesPlugin::default();
        let queries = Queries("qq.official", None);
        let target = QqChat::Private {
            user_id: "member-a".to_owned(),
        };
        let click = |invocation, data: &str| {
            event(
                invocation,
                QQ_BUTTON_CLICKED,
                json!({
                    "button_id": BUTTON_ID,
                    "target": target,
                    "actor_id": "member-a",
                    "data": data,
                }),
            )
        };
        let completed = |invocation, status| {
            event(
                "completion",
                "action.completed",
                json!({
                    "source_event_id": format!("event-{invocation}"),
                    "source_invocation_id": invocation,
                    "command_id": ACK_COMMAND_ID,
                    "kind": QQ_BUTTON_ACK,
                    "status": status,
                    "retryable": false,
                }),
            )
        };

        let ack = plugin
            .on_event(&click("ack-a", "2"), &queries)
            .await
            .unwrap();
        assert_eq!(
            ack.commands,
            [PluginCommand::qq_button_ack(
                ACK_COMMAND_ID,
                QqButtonAck::Success
            )]
        );
        let second_ack = plugin
            .on_event(&click("ack-b", "3"), &queries)
            .await
            .unwrap();
        assert_eq!(second_ack.commands, ack.commands);
        for completion in [
            completed("unrelated", ActionStatus::Succeeded),
            completed("ack-b", ActionStatus::Failed),
            completed("ack-b", ActionStatus::Succeeded),
        ] {
            let ignored = plugin.on_event(&completion, &queries).await.unwrap();
            assert_eq!(ignored.commands, []);
        }

        let success = completed("ack-a", ActionStatus::Succeeded);
        let page = plugin.on_event(&success, &queries).await.unwrap();
        let [send] = page.commands.as_slice() else {
            panic!("expected one Markdown send");
        };
        assert_eq!(send.kind, QQ_MARKDOWN_SEND);
        let send: QqMarkdownSend = serde_json::from_value(send.payload.clone()).unwrap();
        assert_eq!(send.target, target);
        assert_eq!(send.markdown, markdown(1, "member-a".to_owned()));
        let duplicate = plugin.on_event(&success, &queries).await.unwrap();
        assert_eq!(duplicate.commands, []);
    }

    #[test]
    fn a_full_pending_queue_rejects_the_new_click_and_keeps_previous_receipts() {
        let plugin = QqMarkdownPagesPlugin::default();
        let click = QqButtonClicked {
            actor_id: "reader".to_owned(),
            target: QqChat::Private {
                user_id: "reader".to_owned(),
            },
            button_id: BUTTON_ID.to_owned(),
            data: "2".to_owned(),
        };
        for index in 0..=MAX_PENDING_PAGES {
            let request = event(&format!("click-{index}"), QQ_BUTTON_CLICKED, Value::Null);
            let expected = if index == MAX_PENDING_PAGES {
                QqButtonAck::TooFrequent
            } else {
                QqButtonAck::Success
            };
            assert_eq!(
                plugin.acknowledge(&request, click.clone()).commands,
                [PluginCommand::qq_button_ack(ACK_COMMAND_ID, expected)]
            );
        }
        let first_receipt = event(
            "completion",
            "action.completed",
            json!({
                "source_event_id":"event-click-0",
                "source_invocation_id":"click-0",
                "command_id":ACK_COMMAND_ID,
                "kind":QQ_BUTTON_ACK,
                "status":"succeeded",
                "retryable":false
            }),
        );
        let [send] = plugin
            .after_ack(&first_receipt)
            .commands
            .try_into()
            .unwrap();
        assert_eq!(send.kind, QQ_MARKDOWN_SEND);
    }

    #[test]
    fn pending_entries_without_receipts_expire_before_the_next_click() {
        let plugin = QqMarkdownPagesPlugin::default();
        let click = QqButtonClicked {
            actor_id: "reader".to_owned(),
            target: QqChat::Private {
                user_id: "reader".to_owned(),
            },
            button_id: BUTTON_ID.to_owned(),
            data: "2".to_owned(),
        };
        for index in 0..MAX_PENDING_PAGES {
            let request = event(&format!("lost-{index}"), QQ_BUTTON_CLICKED, Value::Null);
            plugin.acknowledge(&request, click.clone());
        }
        let mut request = event("fresh", QQ_BUTTON_CLICKED, Value::Null);
        request.received_at_ms = PENDING_TTL_MS;
        assert_eq!(
            plugin.acknowledge(&request, click).commands,
            [PluginCommand::qq_button_ack(
                ACK_COMMAND_ID,
                QqButtonAck::Success
            )]
        );
        assert_eq!(plugin.pending.lock().unwrap().len(), 1);
    }
}
