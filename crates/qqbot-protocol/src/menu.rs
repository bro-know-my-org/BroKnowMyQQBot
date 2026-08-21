//! Typed QQ bot share-link and custom-menu requests and responses.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareLinkValidationError {
    CallbackDataTooLong { length: usize },
}

impl fmt::Display for ShareLinkValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallbackDataTooLong { length } => write!(
                formatter,
                "QQ share-link callback_data must not exceed 32 characters, got {length}"
            ),
        }
    }
}

impl std::error::Error for ShareLinkValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuValidationError {
    TooManyItems {
        count: usize,
    },
    TooManySubMenuItems {
        count: usize,
    },
    MissingMenu,
    EmptyField {
        field: &'static str,
    },
    WeightedNameTooLong {
        field: &'static str,
        weight: usize,
        maximum: usize,
    },
    IncompatibleBehavior {
        item_type: BotMenuItemType,
    },
    IncompatibleSubMenuBehavior {
        item_type: BotSubMenuItemType,
    },
    InvalidHttpsUrl {
        field: &'static str,
    },
}

impl fmt::Display for MenuValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyItems { count } => write!(
                formatter,
                "QQ custom menu must not contain more than 10 items, got {count}"
            ),
            Self::TooManySubMenuItems { count } => write!(
                formatter,
                "QQ custom menu item must not contain more than 5 sub-menu items, got {count}"
            ),
            Self::MissingMenu => formatter.write_str("QQ custom menu update must contain menu"),
            Self::EmptyField { field } => write!(formatter, "{field} must not be empty"),
            Self::WeightedNameTooLong {
                field,
                weight,
                maximum,
            } => write!(
                formatter,
                "{field} must not exceed {maximum} weighted characters (non-ASCII counts as 2), got {weight}"
            ),
            Self::IncompatibleBehavior { item_type } => write!(
                formatter,
                "QQ custom menu item type {item_type:?} has missing or incompatible behavior fields"
            ),
            Self::IncompatibleSubMenuBehavior { item_type } => write!(
                formatter,
                "QQ custom sub-menu item type {item_type:?} has missing or incompatible behavior fields"
            ),
            Self::InvalidHttpsUrl { field } => {
                write!(formatter, "{field} must be a valid HTTPS URL")
            }
        }
    }
}

impl std::error::Error for MenuValidationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GenerateShareLinkRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
}

impl GenerateShareLinkRequest {
    pub fn validate(&self) -> Result<(), ShareLinkValidationError> {
        if let Some(length) = self
            .callback_data
            .as_ref()
            .map(|value| value.chars().count())
            .filter(|length| *length > 32)
        {
            return Err(ShareLinkValidationError::CallbackDataTooLong { length });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLink {
    pub url_link: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMenuResponse {
    pub version: u64,
    #[serde(default)]
    pub menu: Option<BotMenu>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateBotMenuRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<BotMenu>,
}

impl UpdateBotMenuRequest {
    pub fn validate(&self) -> Result<(), MenuValidationError> {
        self.menu
            .as_ref()
            .ok_or(MenuValidationError::MissingMenu)?
            .validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMenuVersion {
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMenu {
    pub items: Vec<BotMenuItem>,
}

impl BotMenu {
    pub fn validate(&self) -> Result<(), MenuValidationError> {
        if self.items.len() > 10 {
            return Err(MenuValidationError::TooManyItems {
                count: self.items.len(),
            });
        }
        for item in &self.items {
            item.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotMenuItemType {
    Switch,
    SendMessage,
    Link,
    Menu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMenuItem {
    pub name: String,
    #[serde(rename = "type")]
    pub item_type: BotMenuItemType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_menu_items: Option<Vec<BotSubMenuItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<BotMenuSwitch>,
}

impl BotMenuItem {
    fn validate(&self) -> Result<(), MenuValidationError> {
        validate_weighted_name("QQ custom menu item name", &self.name, 10)?;
        match self.item_type {
            BotMenuItemType::Switch => {
                require_only_behavior(self, BotMenuBehavior::Switch)?;
                self.switch
                    .as_ref()
                    .expect("validated switch presence")
                    .validate()
            }
            BotMenuItemType::SendMessage => {
                require_only_behavior(self, BotMenuBehavior::SendMessage)?;
                validate_non_blank(
                    "QQ custom menu send_message",
                    self.send_message
                        .as_deref()
                        .expect("validated send_message presence"),
                )
            }
            BotMenuItemType::Link => {
                require_only_behavior(self, BotMenuBehavior::Link)?;
                validate_https_url(
                    "QQ custom menu link",
                    self.link.as_deref().expect("validated link presence"),
                )
            }
            BotMenuItemType::Menu => {
                require_only_behavior(self, BotMenuBehavior::SubMenu)?;
                let items = self
                    .sub_menu_items
                    .as_ref()
                    .expect("validated sub-menu presence");
                if items.len() > 5 {
                    return Err(MenuValidationError::TooManySubMenuItems { count: items.len() });
                }
                for item in items {
                    item.validate()?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotSubMenuItemType {
    SendMessage,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotSubMenuItem {
    pub name: String,
    #[serde(rename = "type")]
    pub item_type: BotSubMenuItemType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl BotSubMenuItem {
    fn validate(&self) -> Result<(), MenuValidationError> {
        validate_weighted_name("QQ custom sub-menu item name", &self.name, 14)?;
        match self.item_type {
            BotSubMenuItemType::SendMessage => {
                if self.link.is_some() || self.send_message.is_none() {
                    return Err(MenuValidationError::IncompatibleSubMenuBehavior {
                        item_type: self.item_type,
                    });
                }
                validate_non_blank(
                    "QQ custom sub-menu send_message",
                    self.send_message
                        .as_deref()
                        .expect("validated send_message presence"),
                )
            }
            BotSubMenuItemType::Link => {
                if self.send_message.is_some() || self.link.is_none() {
                    return Err(MenuValidationError::IncompatibleSubMenuBehavior {
                        item_type: self.item_type,
                    });
                }
                validate_https_url(
                    "QQ custom sub-menu link",
                    self.link.as_deref().expect("validated link presence"),
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMenuSwitch {
    pub switch_id: String,
    pub default: bool,
}

impl BotMenuSwitch {
    fn validate(&self) -> Result<(), MenuValidationError> {
        validate_non_blank("QQ custom menu switch_id", &self.switch_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BotMenuBehavior {
    SendMessage,
    Link,
    Switch,
    SubMenu,
}

fn require_only_behavior(
    item: &BotMenuItem,
    expected: BotMenuBehavior,
) -> Result<(), MenuValidationError> {
    let actual = match (
        item.send_message.is_some(),
        item.link.is_some(),
        item.switch.is_some(),
        item.sub_menu_items.is_some(),
    ) {
        (true, false, false, false) => Some(BotMenuBehavior::SendMessage),
        (false, true, false, false) => Some(BotMenuBehavior::Link),
        (false, false, true, false) => Some(BotMenuBehavior::Switch),
        (false, false, false, true) => Some(BotMenuBehavior::SubMenu),
        _ => None,
    };
    if actual != Some(expected) {
        return Err(MenuValidationError::IncompatibleBehavior {
            item_type: item.item_type,
        });
    }
    Ok(())
}

fn validate_weighted_name(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), MenuValidationError> {
    validate_non_blank(field, value)?;
    let weight = value
        .chars()
        .map(|character| usize::from(!character.is_ascii()) + 1)
        .sum::<usize>();
    if weight > maximum {
        return Err(MenuValidationError::WeightedNameTooLong {
            field,
            weight,
            maximum,
        });
    }
    Ok(())
}

fn validate_non_blank(field: &'static str, value: &str) -> Result<(), MenuValidationError> {
    if value.trim().is_empty() {
        return Err(MenuValidationError::EmptyField { field });
    }
    Ok(())
}

fn validate_https_url(field: &'static str, value: &str) -> Result<(), MenuValidationError> {
    validate_non_blank(field, value)?;
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(MenuValidationError::InvalidHttpsUrl { field });
    }
    let url = Url::parse(value).map_err(|_| MenuValidationError::InvalidHttpsUrl { field })?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(MenuValidationError::InvalidHttpsUrl { field });
    }
    Ok(())
}
