//! Bounded, redacted, current-process runtime logs for the management UI.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt::{self, Write as _},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

const CAPACITY: usize = 1000;
const FIELD_COUNT: usize = 16;
pub(crate) const MESSAGE_TRACE_TARGET: &str = "bkm::message_trace";

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogCategory {
    #[default]
    Runtime,
    Message,
    All,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct LogSelection<'a> {
    pub(crate) level: Option<&'a str>,
    pub(crate) instance: Option<&'a str>,
    pub(crate) category: LogCategory,
    pub(crate) include_content: bool,
}

#[derive(Clone)]
pub(crate) struct LogBuffer {
    session_id: Arc<str>,
    entries: Arc<Mutex<(u64, VecDeque<LogEntry>)>>,
    redactions: Arc<Vec<String>>,
    updates: watch::Sender<u64>,
    content_users: Arc<AtomicUsize>,
    content_epoch: Arc<AtomicU64>,
}

pub(crate) struct ContentCapture(LogBuffer);

impl Drop for ContentCapture {
    fn drop(&mut self) {
        let mut state = self
            .0
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.0.content_users.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.content_epoch.fetch_add(1, Ordering::Release);
            for entry in &mut state.1 {
                entry.content = None;
            }
        }
    }
}

impl fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogBuffer")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LogEntry {
    pub(crate) id: u64,
    pub(crate) timestamp: String,
    pub(crate) level: String,
    pub(crate) target: String,
    pub(crate) category: &'static str,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    pub(crate) instance_id: Option<String>,
    pub(crate) fields: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct LogPage {
    pub(crate) session_id: String,
    pub(crate) entries: Vec<LogEntry>,
    pub(crate) next_cursor: u64,
    pub(crate) dropped: bool,
    pub(crate) has_more: bool,
}

impl Default for LogBuffer {
    fn default() -> Self {
        let secrets = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.to_str()?.to_ascii_uppercase();
                (key.starts_with("BKMQB_")
                    && [
                        "TOKEN",
                        "SECRET",
                        "PASSWORD",
                        "APP_ID",
                        "AUTHORIZATION",
                        "BEARER",
                    ]
                    .iter()
                    .any(|part| key.contains(part)))
                .then(|| value.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())
            })
            .collect();
        Self::new(secrets)
    }
}

impl LogBuffer {
    fn new(mut redactions: Vec<String>) -> Self {
        redactions.retain(|value| !value.is_empty());
        // Debug formatting can escape quotes and controls in otherwise known credentials.
        let escaped = redactions
            .iter()
            .map(|value| {
                let debug = format!("{value:?}");
                debug[1..debug.len() - 1].to_owned()
            })
            .collect::<Vec<_>>();
        redactions.extend(escaped);
        redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
        redactions.dedup();
        Self {
            session_id: uuid::Uuid::new_v4().to_string().into(),
            entries: Arc::new(Mutex::new((0, VecDeque::with_capacity(CAPACITY)))),
            redactions: Arc::new(redactions),
            updates: watch::channel(0).0,
            content_users: Arc::new(AtomicUsize::new(0)),
            content_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn layer(&self) -> LiveLayer {
        LiveLayer(self.clone())
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
    }

    pub(crate) fn capture_content(&self) -> ContentCapture {
        let _state = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.content_users.fetch_add(1, Ordering::Relaxed) == 0 {
            self.content_epoch.fetch_add(1, Ordering::Release);
        }
        ContentCapture(self.clone())
    }

    #[cfg(test)]
    pub(crate) fn stream_snapshot(
        &self,
        after: Option<u64>,
        session_id: Option<&str>,
        limit: usize,
        level: Option<&str>,
        instance: Option<&str>,
    ) -> LogPage {
        self.filtered_stream_snapshot(
            after,
            session_id,
            limit,
            LogSelection {
                level,
                instance,
                ..Default::default()
            },
        )
    }

    pub(crate) fn filtered_stream_snapshot(
        &self,
        after: Option<u64>,
        session_id: Option<&str>,
        limit: usize,
        selection: LogSelection<'_>,
    ) -> LogPage {
        let reset = session_id.is_some_and(|session| session != self.session_id.as_ref());
        let mut page = self.filtered_snapshot(if reset { None } else { after }, limit, selection);
        page.dropped |= reset;
        page
    }

    #[cfg(test)]
    pub(crate) fn snapshot(
        &self,
        after: Option<u64>,
        limit: usize,
        level: Option<&str>,
        instance: Option<&str>,
    ) -> LogPage {
        self.filtered_snapshot(
            after,
            limit,
            LogSelection {
                level,
                instance,
                ..Default::default()
            },
        )
    }

    pub(crate) fn filtered_snapshot(
        &self,
        after: Option<u64>,
        limit: usize,
        selection: LogSelection<'_>,
    ) -> LogPage {
        let state = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (latest, ring) = &*state;
        let limit = limit.clamp(1, CAPACITY);
        let matches = |entry: &&LogEntry| {
            severity(&entry.level) >= severity(selection.level.unwrap_or("TRACE"))
                && selection
                    .instance
                    .is_none_or(|instance| entry.instance_id.as_deref() == Some(instance))
                && match selection.category {
                    LogCategory::Runtime => entry.category == "runtime",
                    LogCategory::Message => entry.category == "message",
                    LogCategory::All => true,
                }
        };
        let reset = after.is_some_and(|cursor| cursor > *latest);
        let dropped = reset
            || after.is_some_and(|cursor| {
                ring.front()
                    .is_some_and(|first| cursor < first.id.saturating_sub(1))
            });
        let mut next_cursor = *latest;
        let mut has_more = false;
        let mut entries = if after.is_none() || reset {
            let mut entries = ring
                .iter()
                .rev()
                .filter(matches)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            entries.reverse();
            entries
        } else {
            let mut entries = Vec::new();
            next_cursor = after.unwrap_or_default();
            for entry in ring
                .iter()
                .filter(|entry| entry.id > after.unwrap_or_default())
            {
                if matches(&entry) {
                    if entries.len() == limit {
                        has_more = true;
                        break;
                    }
                    entries.push(entry.clone());
                }
                next_cursor = entry.id;
            }
            entries
        };
        if !selection.include_content {
            for entry in &mut entries {
                entry.content = None;
            }
        }
        LogPage {
            session_id: self.session_id.to_string(),
            entries,
            next_cursor,
            dropped,
            has_more,
        }
    }

    fn text(&self, value: &dyn fmt::Debug, limit: usize) -> String {
        let mut writer = Bounded {
            text: String::new(),
            limit,
            truncated: false,
        };
        let _ = write!(writer, "{value:?}");
        self.clean(writer.text, limit, writer.truncated)
    }

    fn clean(&self, mut text: String, limit: usize, truncated: bool) -> String {
        if truncated {
            // A value may end mid-secret at the capture boundary. Remove the whole
            // matching suffix before replacement, including secrets longer than limit.
            let suffix = self
                .redactions
                .iter()
                .flat_map(|secret| {
                    text.char_indices()
                        .map(|(start, _)| start)
                        .filter(|start| secret.starts_with(&text[*start..]))
                })
                .min();
            if let Some(start) = suffix {
                text.truncate(start);
                text.push_str("[REDACTED]");
            }
        }
        for secret in self.redactions.iter() {
            text = text.replace(secret, "[REDACTED]");
        }
        let mut output = Bounded {
            text: String::new(),
            limit,
            truncated: false,
        };
        for character in text.chars() {
            if character.is_control() {
                if write!(output, "{}", character.escape_default()).is_err() {
                    break;
                }
            } else if output.write_char(character).is_err() {
                break;
            }
        }
        output.text
    }
}

fn severity(level: &str) -> u8 {
    match level {
        "ERROR" => 5,
        "WARN" => 4,
        "INFO" => 3,
        "DEBUG" => 2,
        _ => 1,
    }
}

struct Bounded {
    text: String,
    limit: usize,
    truncated: bool,
}

impl fmt::Write for Bounded {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = self.limit.saturating_sub(self.text.len());
        let mut end = available.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        if end < value.len() {
            self.truncated = true;
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Captured {
    message: Option<String>,
    content: Option<String>,
    instance_id: Option<String>,
    fields: BTreeMap<String, String>,
}

impl Captured {
    fn merge(&mut self, other: &Self) {
        if other.instance_id.is_some() {
            self.instance_id.clone_from(&other.instance_id);
        }
        for (key, value) in &other.fields {
            if self.fields.len() < FIELD_COUNT || self.fields.contains_key(key) {
                self.fields.insert(key.clone(), value.clone());
            }
        }
    }
}

struct Visitor<'a> {
    buffer: &'a LogBuffer,
    captured: &'a mut Captured,
    capture_content: bool,
}

impl Visitor<'_> {
    fn record(&mut self, field: &Field, value: impl FnOnce(usize) -> String) {
        let name = field.name();
        if name == "content" && self.capture_content {
            self.captured.content = Some(value(2048));
            return;
        }
        let limit = if name == "message" { 2048 } else { 512 };
        let value = if sensitive(name) {
            "[REDACTED]".to_owned()
        } else {
            value(limit)
        };
        match name {
            "message" => self.captured.message = Some(value),
            "instance_id" => self.captured.instance_id = Some(value),
            _ if self.captured.fields.len() < FIELD_COUNT
                || self.captured.fields.contains_key(name) =>
            {
                let key = self.buffer.clean(name.to_owned(), 64, false);
                self.captured.fields.insert(key, value);
            }
            _ => {}
        }
    }
}

impl Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let buffer = self.buffer;
        self.record(field, |limit| buffer.text(value, limit));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        let buffer = self.buffer;
        self.record(field, |limit| {
            let mut writer = Bounded {
                text: String::new(),
                limit,
                truncated: false,
            };
            let _ = writer.write_str(value);
            buffer.clean(writer.text, limit, writer.truncated)
        });
    }
}

fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "credential",
        "api_key",
        "private_key",
        "signature",
        "authorization",
        "cookie",
        "app_id",
        "sender_id",
        "user_id",
        "target_id",
        "content",
        "raw",
        "body",
        "payload",
    ]
    .iter()
    .any(|part| name.contains(part))
}

#[derive(Debug)]
pub(crate) struct LiveLayer(LogBuffer);

impl<S: Subscriber + for<'lookup> LookupSpan<'lookup>> Layer<S> for LiveLayer {
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut captured = Captured::default();
        attributes.record(&mut Visitor {
            buffer: &self.0,
            captured: &mut captured,
            capture_content: false,
        });
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(captured);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        if let Some(span) = context.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(captured) = extensions.get_mut::<Captured>() {
                values.record(&mut Visitor {
                    buffer: &self.0,
                    captured,
                    capture_content: false,
                });
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        if event.metadata().target() == super::MESSAGE_LOG_TARGET {
            return;
        }
        let is_message = event.metadata().target() == MESSAGE_TRACE_TARGET;
        let capture_epoch = self.0.content_epoch.load(Ordering::Acquire);
        let mut captured = Captured::default();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<Captured>() {
                    captured.merge(fields);
                }
            }
        }
        event.record(&mut Visitor {
            buffer: &self.0,
            captured: &mut captured,
            capture_content: is_message && capture_epoch % 2 == 1,
        });
        if is_message {
            captured.fields.remove("content");
        }
        let mut state = self
            .0
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 += 1;
        // The final capture lease and insertion share the ring lock. A writer
        // already formatting content cannot reinsert it after capture stops.
        let body_preview = if is_message
            && capture_epoch % 2 == 1
            && self.0.content_epoch.load(Ordering::Acquire) == capture_epoch
        {
            captured.content
        } else {
            None
        };
        let entry = LogEntry {
            id: state.0,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            level: event.metadata().level().to_string(),
            target: self
                .0
                .clean(event.metadata().target().to_owned(), 128, false),
            category: if is_message { "message" } else { "runtime" },
            message: captured.message.unwrap_or_default(),
            content: body_preview,
            instance_id: captured.instance_id,
            fields: captured.fields,
        };
        if state.1.len() == CAPACITY {
            state.1.pop_front();
        }
        state.1.push_back(entry);
        // Publish while holding the ring lock so concurrent writers cannot
        // announce IDs out of order. Watch coalesces updates without a queue.
        self.0.updates.send_replace(state.0);
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "live_message_tests.rs"]
mod message_tests;
