//! Human-readable console event formatting with optional localization.

use std::fmt;

use chrono::{Local, SecondsFormat};
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

#[derive(Debug, Clone, Copy)]
pub(super) enum ConsoleLanguage {
    English,
    SimplifiedChinese,
}

impl ConsoleLanguage {
    pub(super) fn parse(value: &str) -> Self {
        if value == "zh-CN" {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }
}

#[derive(Debug)]
pub(super) struct ConsoleFormat {
    language: ConsoleLanguage,
    ansi: bool,
}

impl ConsoleFormat {
    pub(super) const fn new(language: ConsoleLanguage, ansi: bool) -> Self {
        Self { language, ansi }
    }
}

impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let mut visitor = ConsoleVisitor::default();
        event.record(&mut visitor);
        let timestamp = Local::now().to_rfc3339_opts(SecondsFormat::Millis, false);
        if self.ansi {
            write!(writer, "\x1b[2;90m{timestamp}\x1b[0m ")?;
            write!(
                writer,
                "{}{:<5}\x1b[0m ",
                level_style(*metadata.level()),
                metadata.level()
            )?;
        } else {
            write!(writer, "{timestamp} {:<5} ", metadata.level())?;
        }
        if let Some(message) = visitor.message {
            write_message(&mut writer, localize(self.language, &message), self.ansi)?;
        } else {
            write_message(&mut writer, metadata.target(), self.ansi)?;
        }
        for field in visitor.fields.into_iter().filter(|field| {
            event.metadata().target() != super::MESSAGE_LOG_TARGET
                || !matches!(
                    field.name,
                    "event_id" | "message_id" | "sender_id" | "target_id"
                )
        }) {
            write_field(&mut writer, &field, self.ansi)?;
        }
        writeln!(writer)
    }
}

fn write_message(writer: &mut Writer<'_>, message: &str, ansi: bool) -> fmt::Result {
    let message = message.escape_debug();
    if ansi {
        write!(writer, "\x1b[1m{message}\x1b[0m")
    } else {
        write!(writer, "{message}")
    }
}

fn write_field(writer: &mut Writer<'_>, field: &ConsoleField, ansi: bool) -> fmt::Result {
    let escaped = field.value.escape_debug().to_string();
    if ansi {
        let value_style = if field.name == "content" {
            "\x1b[32m"
        } else if field.name == "error" {
            "\x1b[31m"
        } else {
            "\x1b[37m"
        };
        write!(
            writer,
            " \x1b[36m{}\x1b[0m={value_style}{}\x1b[0m",
            field.name, escaped
        )
    } else {
        write!(writer, " {}={}", field.name, escaped)
    }
}

const fn level_style(level: tracing::Level) -> &'static str {
    match level {
        tracing::Level::TRACE => "\x1b[35m",
        tracing::Level::DEBUG => "\x1b[34m",
        tracing::Level::INFO => "\x1b[32m",
        tracing::Level::WARN => "\x1b[33m",
        tracing::Level::ERROR => "\x1b[1;31m",
    }
}

#[derive(Debug)]
struct ConsoleField {
    name: &'static str,
    value: String,
}

#[derive(Debug, Default)]
struct ConsoleVisitor {
    message: Option<String>,
    fields: Vec<ConsoleField>,
}

impl ConsoleVisitor {
    fn record_value(&mut self, field: &tracing::field::Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.push(ConsoleField {
                name: field.name(),
                value,
            });
        }
    }
}

impl Visit for ConsoleVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record_value(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.record_value(field, format!("{value:?}"));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record_value(field, value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record_value(field, value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record_value(field, value.to_string());
    }
}

fn localize(language: ConsoleLanguage, message: &str) -> &str {
    if !matches!(language, ConsoleLanguage::SimplifiedChinese) {
        return message;
    }
    match message {
        "loaded local secrets environment file" => "已加载本地密钥环境文件",
        "registered plugin" => "已注册插件",
        "WASM plugin is disabled" => "WASM 插件已禁用",
        "loaded local WASM plugin" => "已加载本地 WASM 插件",
        "QQ credentials, Gateway endpoint, and plugins are available" => {
            "QQ 凭据、Gateway 端点和插件均可用"
        }
        "failed to listen for shutdown signal" => "监听关闭信号失败",
        "plugin shutdown failed after runtime failure" => "Runtime 失败后插件关闭失败",
        "connecting to QQ Gateway" => "正在连接 QQ Gateway",
        "QQ Gateway connected" => "QQ Gateway 已连接",
        "QQ Gateway session resumed" => "QQ Gateway 会话已恢复",
        "QQ Gateway requested reconnect" => "QQ Gateway 请求重新连接",
        "QQ Gateway connection stopped" => "QQ Gateway 连接已停止",
        "retrying QQ Gateway connection after backoff" => "退避后重试 QQ Gateway 连接",
        "ignoring unsupported QQ Gateway opcode" => "忽略不支持的 QQ Gateway OpCode",
        "QQ Gateway closed WebSocket" => "QQ Gateway 已关闭 WebSocket",
        "sent QQ Gateway Resume" => "已发送 QQ Gateway Resume",
        "sent QQ Gateway Identify" => "已发送 QQ Gateway Identify",
        "received QQ message event" => "收到 QQ 消息事件",
        "received QQ message content" => "收到 QQ 消息正文",
        "sent QQ message content" => "已发送 QQ 消息正文",
        "failed to send QQ message content" => "QQ 消息正文发送失败",
        "received unmapped QQ Gateway event" => "收到尚未映射的 QQ Gateway 事件",
        "QQ message event details" => "QQ 消息事件详情",
        "unmapped QQ Gateway event details" => "未映射 QQ Gateway 事件详情",
        "QQ message action succeeded" => "QQ 消息操作成功",
        "QQ message action failed" => "QQ 消息操作失败",
        "QQ message action result details" => "QQ 消息操作结果详情",
        "starting BroKnowMyQQBot with QQ Official WebSocket adapter" => {
            "正在使用 QQ 官方 WebSocket Adapter 启动 BroKnowMyQQBot"
        }
        "bot runtime started" => "Bot Runtime 已启动",
        "bot runtime is shutting down" => "Bot Runtime 正在关闭",
        "dropping event from unknown adapter" => "丢弃来自未知 Adapter 的事件",
        "event dispatch shutdown timed out; aborting remaining tasks" => {
            "事件分发关闭超时，正在中止剩余任务"
        }
        "adapter shutdown timed out; aborting remaining tasks" => {
            "Adapter 关闭超时，正在中止剩余任务"
        }
        "event handler failed" => "事件处理器执行失败",
        "event handler timed out" => "事件处理器执行超时",
        "adapter failed to commit handled event" => "Adapter 提交已处理事件失败",
        "adapter stopped with an error" => "Adapter 因错误停止",
        "isolated plugin failure" => "已隔离插件故障",
        "plugin diagnostic" => "插件诊断",
        "plugin command failed" => "插件命令执行失败",
        "scheduler lock is poisoned" => "调度器锁已损坏",
        "failed to persist scheduler completion" => "持久化调度任务完成状态失败",
        "scheduled plugin invocation failed" => "定时插件调用失败",
        "BroKnowMyQQBot stopped" => "BroKnowMyQQBot 已停止",
        _ => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_messages_default_to_english_and_can_use_chinese() {
        assert_eq!(
            localize(ConsoleLanguage::English, "QQ Gateway connected"),
            "QQ Gateway connected"
        );
        assert_eq!(
            localize(ConsoleLanguage::SimplifiedChinese, "QQ Gateway connected"),
            "QQ Gateway 已连接"
        );
        assert_eq!(level_style(tracing::Level::INFO), "\x1b[32m");
        assert_eq!(level_style(tracing::Level::ERROR), "\x1b[1;31m");
    }
}
