//! Conservative normalization for old, potentially unredacted runtime files.
use super::live::{LogBuffer, LogEntry};
use serde_json::Value;
use std::collections::BTreeMap;

const MESSAGES: &[&str] = &[
    "loaded local secrets environment file",
    "starting BroKnowMyQQBot adapters",
    "QQ credentials, Gateway endpoint, and plugins are available",
    "BroKnowMyQQBot stopped",
    "optional Browser Runtime is not installed",
    "optional Browser Runtime is unavailable",
    "optional Browser Runtime discovery task failed",
    "optional Browser Runtime discovery timed out",
    "timed-out Browser Runtime discovery task failed to join",
    "plugin shutdown failed after plugin loading failure",
    "plugin shutdown failed after Gateway preflight failure",
    "plugin shutdown failed after runtime failure",
    "management service also failed during runtime shutdown",
    "using private Browser Runtime fallback directory",
    "WASM plugin is disabled",
    "loaded local WASM plugin",
    "registered plugin",
    "scheduled plugin invocation failed",
    "failed to retire scheduled worker",
    "management server listening",
    "configuration validation failed",
    "configuration validation worker failed",
    "failed to start configuration validation worker",
    "management validation jobs exceeded the shutdown grace period",
    "management validation supervisor also failed",
    "bot runtime started",
    "bot runtime is shutting down",
    "adapter stopped",
    "adapter stopped with an error",
    "dropping event from unknown adapter",
    "event handler failed",
    "event handler timed out",
    "adapter event drain exceeded the runtime shutdown deadline",
    "adapter shutdown timed out; aborting remaining tasks",
    "event dispatch shutdown timed out; aborting remaining tasks",
    "connecting to QQ Gateway",
    "QQ Gateway connected",
    "QQ Gateway session resumed",
    "QQ Gateway requested reconnect",
    "QQ Gateway connection stopped",
    "retrying QQ Gateway connection after backoff",
    "QQ Gateway closed WebSocket",
    "sent QQ Gateway Resume",
    "sent QQ Gateway Identify",
    "ignoring unsupported QQ Gateway opcode",
    "dropping malformed QQ Gateway dispatch",
    "received QQ message event",
    "received unmapped QQ Gateway event",
    "QQ message event details",
    "unmapped QQ Gateway event details",
    "QQ message action succeeded",
    "QQ message action failed",
    "QQ platform action succeeded",
    "QQ platform action failed",
    "QQ message action result details",
    "QQ Webhook callback server is listening",
    "rejected QQ Webhook callback",
    "rejected malformed QQ Webhook dispatch",
    "acknowledging duplicate QQ Webhook event",
    "accepted QQ Webhook event",
    "OneBot 11 reverse WebSocket listening",
    "OneBot 11 reverse WebSocket connected",
    "OneBot 11 reverse WebSocket disconnected",
    "OneBot 11 handshake failed",
    "OneBot 11 connection ended with an error",
    "rejected excess OneBot 11 handshake",
    "rejected additional OneBot 11 connection",
    "ignored binary OneBot frame",
    "ignored malformed OneBot action response",
    "ignored OneBot response with invalid echo",
    "ignored unknown or duplicate OneBot echo",
    "ignored invalid OneBot event",
    "OneBot event forwarder failed while draining",
];

const FIELDS: &[&str] = &[
    "record_kind",
    "plugin_id",
    "adapter_id",
    "event_id",
    "invocation_id",
    "command_id",
    "command_name",
    "action_type",
    "status",
    "elapsed_ms",
    "output_count",
    "command_count",
    "state_op_count",
    "disposition",
    "message_scope",
    "adapter_count",
];

/// Raw mode receives this safe projection, never a source JSON record or arbitrary spans/errors.
pub(crate) fn normalize(value: &Value, id: u64, buffer: &LogBuffer) -> Option<LogEntry> {
    let timestamp = value.get("timestamp")?.as_str()?;
    if timestamp.len() > 64 || chrono::DateTime::parse_from_rfc3339(timestamp).is_err() {
        return None;
    }
    let level = value.get("level")?.as_str()?;
    if !matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR") {
        return None;
    }
    let target = value.get("target")?.as_str()?;
    if target.len() > 128
        || target.is_empty()
        || !target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':'))
        || matches!(
            target,
            super::MESSAGE_LOG_TARGET | super::live::MESSAGE_TRACE_TARGET
        )
    {
        return None;
    }
    let message = value.get("message")?.as_str()?;
    let mut fields = BTreeMap::new();
    let mut instance_id = None;
    let mut capture = |map: &serde_json::Map<String, Value>| {
        if let Some(value) = map.get("instance_id").and_then(Value::as_str) {
            instance_id = Some(buffer.archive_text(value, 512));
        }
        for &key in FIELDS {
            if let Some(value) = map.get(key) {
                let text = match value {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => continue,
                };
                fields.insert(key.to_owned(), buffer.archive_text(&text, 512));
            }
        }
    };
    // tracing JSONL stores inherited span fields separately; never return the span objects.
    if let Some(spans) = value.get("spans").and_then(Value::as_array) {
        for span in spans.iter().take(16).filter_map(Value::as_object) {
            capture(span);
        }
    }
    if let Some(span) = value.get("span").and_then(Value::as_object) {
        capture(span);
    }
    capture(value.as_object()?);
    let message = if MESSAGES.contains(&message) {
        message
    } else if matches!(
        fields.get("record_kind").map(String::as_str),
        Some("plugin.invoked" | "plugin.completed" | "plugin.failed" | "plugin.action.completed")
    ) {
        "历史插件运行事件（源消息未开放）"
    } else {
        return None;
    };
    Some(LogEntry {
        id,
        timestamp: timestamp.to_owned(),
        level: level.to_owned(),
        target: buffer.archive_text(target, 128),
        category: "runtime",
        message: message.to_owned(),
        content: None,
        instance_id,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn archive_projection_omits_payloads_errors_bodies_and_arbitrary_spans() {
        let log = json!({"timestamp":"2026-11-14T00:00:00Z","level":"INFO","target":"plugin_host::runtime",
            "message":"registered plugin","error":"secret-error","payload":{"token":"secret"},
            "content":"private body","spans":[{"instance_id":"test","plugin_id":"dev.bkm.test","body":"private"}],
            "span":{"sender_id":"private-user"},"adapter_count":3});
        let row = normalize(&log, 3, &LogBuffer::default()).unwrap();
        assert_eq!(row.instance_id.as_deref(), Some("test"));
        assert_eq!(row.fields["plugin_id"], "dev.bkm.test");
        let text = serde_json::to_string(&row).unwrap();
        for forbidden in ["private", "secret", "payload", "sender_id", "spans"] {
            assert!(!text.contains(forbidden));
        }
    }
    #[test]
    fn unknown_dynamic_text_and_message_targets_are_unavailable() {
        let mut log = json!({"timestamp":"2026-11-14T00:00:00Z","level":"INFO","target":"bkmqb::bootstrap","message":"old token=secret"});
        assert!(normalize(&log, 1, &LogBuffer::default()).is_none());
        log["message"] = json!("registered plugin");
        log["target"] = json!("bkm::messages");
        assert!(normalize(&log, 1, &LogBuffer::default()).is_none());
        log["target"] = json!("bkm::message_trace");
        assert!(normalize(&log, 1, &LogBuffer::default()).is_none());
    }
}
