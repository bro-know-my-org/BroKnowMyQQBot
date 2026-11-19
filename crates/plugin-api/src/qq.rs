//! BPP 1.7 contracts for QQ group and C2C rich messages.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{PluginCommand, PluginError, PluginEventEnvelope};

pub const QQ_MARKDOWN_REPLY: &str = "qq.markdown.reply";
pub const QQ_MARKDOWN_SEND: &str = "qq.markdown.send";
pub const QQ_BUTTON_ACK: &str = "qq.button.ack";
pub const QQ_BUTTON_CLICKED: &str = "qq.button.clicked";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqChat {
    Group { group_id: String },
    Private { user_id: String },
}

impl QqChat {
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = match self {
            Self::Group { group_id } => group_id,
            Self::Private { user_id } => user_id,
        };
        identifier(id, 256)
            .then_some(())
            .ok_or("invalid QQ conversation ID")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqMarkdown {
    pub body: QqMarkdownBody,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<QqKeyboard>,
    #[serde(default)]
    pub force_verify_image_resource: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqMarkdownBody {
    Content {
        content: String,
    },
    Template {
        id: String,
        params: Vec<QqMarkdownParam>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqMarkdownParam {
    pub key: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqKeyboard {
    Template { id: String },
    Inline { rows: Vec<Vec<QqButton>> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqButton {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visited_label: Option<String>,
    pub style: QqButtonStyle,
    pub access: QqButtonAccess,
    pub action: QqButtonAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_tips: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QqButtonStyle {
    Gray,
    Blue,
    Red,
    BlueFilled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqButtonAccess {
    Everyone {},
    Administrators {},
    Users { ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqButtonAction {
    Link {
        url: String,
    },
    Command {
        text: String,
        enter: bool,
        reply: bool,
    },
    Callback {
        data: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqMarkdownSend {
    pub target: QqChat,
    pub markdown: QqMarkdown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QqButtonAck {
    #[default]
    Success,
    Failure,
    TooFrequent,
    Duplicate,
    NoPermission,
    OnlyAdministrator,
}

impl QqButtonAck {
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::TooFrequent => 2,
            Self::Duplicate => 3,
            Self::NoPermission => 4,
            Self::OnlyAdministrator => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QqButtonClicked {
    pub actor_id: String,
    pub target: QqChat,
    pub button_id: String,
    pub data: String,
}

impl PluginEventEnvelope {
    /// Returns the Host-projected callback details for a QQ button event.
    pub fn qq_button_clicked(&self) -> Result<Option<QqButtonClicked>, PluginError> {
        if self.event_type != QQ_BUTTON_CLICKED {
            return Ok(None);
        }
        QqButtonClicked::deserialize(&self.payload)
            .map(Some)
            .map_err(|_| PluginError::Permanent("invalid QQ button event".to_owned()))
    }
}

impl PluginCommand {
    pub fn qq_markdown_reply(id: impl Into<String>, markdown: &QqMarkdown) -> Self {
        qq_command(id, QQ_MARKDOWN_REPLY, markdown)
    }

    pub fn qq_markdown_send(id: impl Into<String>, request: &QqMarkdownSend) -> Self {
        qq_command(id, QQ_MARKDOWN_SEND, request)
    }

    pub fn qq_button_ack(id: impl Into<String>, result: QqButtonAck) -> Self {
        qq_command(id, QQ_BUTTON_ACK, &result)
    }
}

fn qq_command(id: impl Into<String>, kind: &str, payload: &impl Serialize) -> PluginCommand {
    PluginCommand {
        command_id: id.into(),
        kind: kind.to_owned(),
        idempotency_key: None,
        deadline_ms: None,
        payload: serde_json::to_value(payload).expect("QQ contract types are JSON serializable"),
    }
}

impl QqMarkdown {
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.body {
            QqMarkdownBody::Content { content } if !text(content, 16 * 1024) => {
                return Err("invalid QQ Markdown content");
            }
            QqMarkdownBody::Template { id, params } => {
                let mut keys = BTreeSet::new();
                if !identifier(id, 128)
                    || params.len() > 20
                    || params.iter().any(|param| {
                        !identifier(&param.key, 128)
                            || !keys.insert(&param.key)
                            || !(1..=20).contains(&param.values.len())
                            || param
                                .values
                                .iter()
                                .any(|value| value.len() > 2048 || value.contains('\0'))
                    })
                {
                    return Err("invalid QQ Markdown template or parameters");
                }
            }
            QqMarkdownBody::Content { .. } => {}
        }
        if let Some(keyboard) = &self.keyboard {
            keyboard.validate()?;
        }
        Ok(())
    }

    pub fn validate_target(&self, target: &QqChat) -> Result<(), &'static str> {
        target.validate()?;
        if let Some(QqKeyboard::Inline { rows }) = &self.keyboard {
            let unsupported = rows.iter().flatten().any(|button| match target {
                QqChat::Group { .. } => {
                    matches!(button.action, QqButtonAction::Command { enter: true, .. })
                }
                QqChat::Private { .. } => {
                    matches!(button.access, QqButtonAccess::Administrators {})
                }
            });
            if unsupported {
                return Err(
                    "QQ button action or permission is unavailable in this conversation type",
                );
            }
        }
        Ok(())
    }
}

impl QqKeyboard {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Template { id } if !identifier(id, 128) => Err("invalid QQ Keyboard template ID"),
            Self::Template { .. } => Ok(()),
            Self::Inline { rows } => {
                if !(1..=5).contains(&rows.len())
                    || rows.iter().any(|row| !(1..=5).contains(&row.len()))
                {
                    return Err("QQ Keyboard allows one to five rows and buttons per row");
                }
                let mut ids = BTreeSet::new();
                for button in rows.iter().flatten() {
                    button.validate()?;
                    if !ids.insert(&button.id) {
                        return Err("duplicate QQ button ID");
                    }
                }
                Ok(())
            }
        }
    }
}

impl QqButton {
    fn validate(&self) -> Result<(), &'static str> {
        if !identifier(&self.id, 64)
            || !label(&self.label, 64)
            || self
                .visited_label
                .as_ref()
                .is_some_and(|value| !label(value, 64))
            || self
                .unsupported_tips
                .as_ref()
                .is_some_and(|value| !label(value, 256))
        {
            return Err("invalid QQ button identity or label");
        }
        if let QqButtonAccess::Users { ids } = &self.access {
            if !(1..=20).contains(&ids.len())
                || ids.iter().any(|id| !identifier(id, 256))
                || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
            {
                return Err("invalid QQ button permission IDs");
            }
        }
        match &self.action {
            QqButtonAction::Link { url } if !valid_link(url) => Err("invalid QQ button link"),
            QqButtonAction::Command { text: value, .. } if !text(value, 1024) => {
                Err("invalid QQ button command")
            }
            QqButtonAction::Callback { data } if data.len() > 1024 || data.contains('\0') => {
                Err("invalid QQ button callback data")
            }
            _ => Ok(()),
        }
    }
}

fn identifier(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.contains('\0')
}

fn label(value: &str, limit: usize) -> bool {
    text(value, limit) && !value.chars().any(char::is_control)
}

fn valid_link(value: &str) -> bool {
    identifier(value, 2048)
        && url::Url::parse(value).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
        })
}
