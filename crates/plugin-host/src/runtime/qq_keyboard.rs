//! Converts validated BPP data into QQ wire fields.

use plugin_api::{
    QqButton, QqButtonAccess, QqButtonAction, QqButtonStyle, QqKeyboard, QqMarkdownBody,
};
use serde_json::{Value, json};

pub(super) fn markdown_body(body: QqMarkdownBody, verify_image: bool) -> Value {
    let mut body = match body {
        QqMarkdownBody::Content { content } => json!({ "content": content }),
        QqMarkdownBody::Template { id, params } => {
            json!({ "custom_template_id": id, "params": params })
        }
    };
    if verify_image {
        body["force_verify_image_resource"] = Value::Bool(true);
    }
    body
}

pub(super) fn callback_count(keyboard: &QqKeyboard) -> usize {
    match keyboard {
        QqKeyboard::Template { .. } => 0,
        QqKeyboard::Inline { rows } => rows
            .iter()
            .flatten()
            .filter(|button| matches!(button.action, QqButtonAction::Callback { .. }))
            .count(),
    }
}

pub(super) fn keyboard(
    keyboard: QqKeyboard,
    mut issue: impl FnMut(&QqButton, &str) -> String,
) -> Value {
    match keyboard {
        QqKeyboard::Template { id } => json!({ "id": id }),
        QqKeyboard::Inline { rows } => {
            let rows: Vec<_> = rows
                .into_iter()
                .map(|row| {
                    let buttons: Vec<_> = row
                        .into_iter()
                        .map(|button| wire_button(&button, &mut issue))
                        .collect();
                    json!({ "buttons": buttons })
                })
                .collect();
            json!({ "content": { "rows": rows } })
        }
    }
}

fn wire_button(button: &QqButton, issue: &mut impl FnMut(&QqButton, &str) -> String) -> Value {
    let color = match button.style {
        QqButtonStyle::Gray => 0,
        QqButtonStyle::Blue => 1,
        QqButtonStyle::Red => 3,
        QqButtonStyle::BlueFilled => 4,
    };
    let permission = match &button.access {
        QqButtonAccess::Administrators {} => json!({ "type": 1 }),
        QqButtonAccess::Everyone {} => json!({ "type": 2 }),
        QqButtonAccess::Users { ids } => json!({ "type": 0, "specify_user_ids": ids }),
    };
    let mut action = match &button.action {
        QqButtonAction::Link { url } => json!({ "type": 0, "data": url }),
        QqButtonAction::Callback { data } => json!({ "type": 1, "data": issue(button, data) }),
        QqButtonAction::Command { text, enter, reply } => {
            json!({ "type": 2, "data": text, "enter": enter, "reply": reply })
        }
    };
    action["permission"] = permission;
    if let Some(tips) = &button.unsupported_tips {
        action["unsupport_tips"] = json!(tips);
    }
    json!({
        "id": button.id,
        "render_data": {
            "label": button.label,
            "visited_label": button.visited_label.as_ref().unwrap_or(&button.label),
            "style": color,
        },
        "action": action,
    })
}
