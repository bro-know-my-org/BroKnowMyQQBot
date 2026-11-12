//! Host command parsing shared by every plugin backend.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_COMMAND_PREFIX: &str = "/";
pub const MAX_COMMAND_PREFIX_CHARS: usize = 32;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error("command prefix must contain 1..=32 characters without whitespace or control characters")]
pub struct InvalidCommandPrefix;

pub fn validate_command_prefix(prefix: &str) -> Result<(), InvalidCommandPrefix> {
    if prefix.is_empty()
        || prefix.chars().count() > MAX_COMMAND_PREFIX_CHARS
        || prefix
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(InvalidCommandPrefix);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandContext {
    pub prefix: String,
    pub name: String,
    /// Text after the first whitespace separator, otherwise unchanged.
    /// Additional spaces, newlines and trailing whitespace are retained.
    pub arguments: String,
}

impl CommandContext {
    #[must_use]
    pub fn parse(prefix: &str, text: &str) -> Option<Self> {
        let tail = text.trim_start().strip_prefix(prefix)?;
        let (name, arguments) = tail.find(char::is_whitespace).map_or((tail, ""), |index| {
            let separator = tail[index..]
                .chars()
                .next()
                .expect("valid character boundary");
            (&tail[..index], &tail[index + separator.len_utf8()..])
        });
        if name.is_empty() {
            return None;
        }
        Some(Self {
            prefix: prefix.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_prefix_and_multiline_arguments_are_lossless() {
        let command = CommandContext::parse("喵!", "  喵!issue 标题\n\n 正文\n ").unwrap();
        assert_eq!(command.prefix, "喵!");
        assert_eq!(command.name, "issue");
        assert_eq!(command.arguments, "标题\n\n 正文\n ");
        assert!(CommandContext::parse("喵!", "/issue 标题").is_none());
        assert!(CommandContext::parse("!!", "!! help").is_none());
        assert_eq!(CommandContext::parse("!!", "!!help").unwrap().arguments, "");
    }

    #[test]
    fn prefix_validation_rejects_invalid_values() {
        for prefix in ["", " ", "a\nb", "\u{2003}", "\0", &"!".repeat(33)] {
            assert!(validate_command_prefix(prefix).is_err());
        }
        for prefix in ["/", "!!", "机器人：", &"喵".repeat(32)] {
            assert!(validate_command_prefix(prefix).is_ok());
        }
    }
}
