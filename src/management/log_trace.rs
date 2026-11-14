//! Read-only relationships from canonical, already retained log observations.

use std::collections::{BTreeSet, VecDeque};

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::plugins::ApiState;
use crate::logging::live::LogEntry;

const FIELDS: &[&str] = &[
    "record_kind",
    "adapter_id",
    "event_id",
    "invocation_id",
    "command_id",
    "plugin_id",
    "command_name",
    "status",
    "action_type",
    "message_scope",
    "elapsed_ms",
    "error_class",
    "command_count",
    "state_op_count",
    "diagnostic_count",
    "disposition",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TraceQuery {
    session_id: String,
    event_id: Option<String>,
    invocation_id: Option<String>,
    after: Option<u64>,
    limit: Option<usize>,
    #[serde(default)]
    include_content: bool,
}

fn valid_id(id: &str) -> bool {
    // UTF-8 truncation at 512 bytes can leave a prefix as short as 509 bytes.
    !id.is_empty()
        && id.len() < 509
        && !id.contains("[REDACTED]")
        && !id.contains('\\')
        && !id.chars().any(char::is_control)
}

impl TraceQuery {
    fn valid(&self) -> bool {
        !self.session_id.is_empty()
            && self.session_id.len() <= 128
            && self
                .session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && self.event_id.is_some() != self.invocation_id.is_some()
            && self
                .event_id
                .as_deref()
                .or(self.invocation_id.as_deref())
                .is_some_and(valid_id)
            && (1..=200).contains(&self.limit.unwrap_or(200))
    }
}

#[derive(Serialize)]
struct TracePage {
    session_id: String,
    event_id: Option<String>,
    invocation_id: Option<String>,
    entries: Vec<LogEntry>,
    next_cursor: u64,
    has_more: bool,
    partial: bool,
    retention_evicted: bool,
    oldest_id: Option<u64>,
    latest_id: u64,
    filter_scope: &'static str,
}

fn canonical(entry: &LogEntry) -> bool {
    matches!(
        entry.fields.get("record_kind").map(String::as_str),
        Some(
            "message.received"
                | "message.sending"
                | "message.sent"
                | "message.failed"
                | "plugin.invoked"
                | "plugin.completed"
                | "plugin.failed"
                | "plugin.action.completed"
        )
    )
}

fn field<'a>(entry: &'a LogEntry, key: &str) -> Option<&'a str> {
    entry
        .fields
        .get(key)
        .map(String::as_str)
        .filter(|id| valid_id(id))
}

fn select(query: &TraceQuery, latest: u64, ring: &VecDeque<LogEntry>) -> TracePage {
    // Invocation IDs are globally unique. Command IDs are only local to an
    // invocation, so never use a bare command ID or a manifest name as a join.
    let roots = ring
        .iter()
        .filter(|entry| canonical(entry))
        .filter(|entry| {
            field(entry, "invocation_id") == query.invocation_id.as_deref()
                && query.invocation_id.is_some()
        })
        .filter_map(|entry| field(entry, "event_id"))
        .collect::<BTreeSet<_>>();
    let event = query
        .event_id
        .as_deref()
        .or_else(|| (roots.len() == 1).then(|| *roots.first().expect("one root")));
    let invocations = ring
        .iter()
        .filter(|entry| canonical(entry))
        .filter(|entry| event.is_some() && field(entry, "event_id") == event)
        .filter_map(|entry| field(entry, "invocation_id"))
        .collect::<BTreeSet<_>>();
    let mut matched = ring
        .iter()
        .filter(|entry| canonical(entry))
        .filter(|entry| {
            let own_event = field(entry, "event_id");
            if let Some(event) = event {
                own_event == Some(event)
                    || (own_event.is_none()
                        && field(entry, "invocation_id").is_some_and(|id| invocations.contains(id)))
            } else {
                // An invocation without a retained event link stays invocation-only.
                roots.len() <= 1 && field(entry, "invocation_id") == query.invocation_id.as_deref()
            }
        })
        .filter(|entry| entry.id > query.after.unwrap_or(0));
    let entries = matched
        .by_ref()
        .take(query.limit.unwrap_or(200))
        .map(|entry| {
            LogEntry {
                id: entry.id,
                timestamp: entry.timestamp.clone(),
                level: entry.level.clone(),
                target: entry.target.clone(),
                category: entry.category,
                // Never forward free-form error strings; stages use kind/status.
                message: entry.fields["record_kind"].clone(),
                content: if query.include_content {
                    entry.content.clone()
                } else {
                    None
                },
                instance_id: entry.instance_id.clone(),
                fields: entry
                    .fields
                    .iter()
                    .filter(|(key, _)| FIELDS.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    let next_cursor = entries
        .last()
        .map_or(query.after.unwrap_or(0), |entry| entry.id);
    TracePage {
        session_id: query.session_id.clone(),
        event_id: event.map(str::to_owned),
        invocation_id: query.invocation_id.clone(),
        entries,
        next_cursor,
        has_more: matched.next().is_some(),
        partial: true,
        retention_evicted: ring.front().is_some_and(|entry| entry.id > 1),
        oldest_id: ring.front().map(|entry| entry.id),
        latest_id: latest,
        filter_scope: "all_retained_levels_and_instances",
    }
}

pub(super) async fn get(
    State(state): State<ApiState>,
    Query(query): Query<TraceQuery>,
) -> Response {
    if !query.valid() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"链路查询参数无效"})),
        )
            .into_response();
    }
    match state
        .logs
        .inspect_retained(&query.session_id, |latest, ring| {
            if query.after.is_some_and(|after| after > latest) {
                None
            } else {
                Some(select(&query, latest, ring))
            }
        }) {
        Ok(Some(page)) => Json(page).into_response(),
        Ok(None) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"链路游标无效"})),
        )
            .into_response(),
        Err(session) => (
            StatusCode::GONE,
            Json(json!({"error":"日志进程会话已变化，无法取回旧链路", "session_id": session})),
        )
            .into_response(),
    }
}

#[cfg(test)]
#[path = "log_trace_tests.rs"]
mod tests;
