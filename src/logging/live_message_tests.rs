use super::*;
use tracing_subscriber::layer::SubscriberExt as _;

fn capture(buffer: &LogBuffer, emit: impl FnOnce()) {
    tracing::subscriber::with_default(tracing_subscriber::registry().with(buffer.layer()), emit);
}

fn messages(buffer: &LogBuffer, include_content: bool) -> LogPage {
    buffer.filtered_snapshot(
        None,
        200,
        LogSelection {
            category: LogCategory::Message,
            include_content,
            ..Default::default()
        },
    )
}

#[test]
fn metadata_is_available_without_capturing_content_and_runtime_stays_separate() {
    let buffer = LogBuffer::new(vec!["private-value".to_owned()]);
    capture(&buffer, || {
        tracing::info!("runtime record");
        tracing::info!(target: MESSAGE_TRACE_TARGET, record_kind = "message.received",
            event_id = "event-1", direction = "inbound", sender_id = "private-sender",
            content = "message body", "received message");
    });
    let runtime = buffer.snapshot(None, 200, None, None);
    assert_eq!(runtime.entries.len(), 1);
    assert_eq!(runtime.entries[0].category, "runtime");
    let page = messages(&buffer, true);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].category, "message");
    assert_eq!(page.entries[0].content, None);
    assert!(!page.entries[0].fields.contains_key("content"));
    assert_eq!(page.entries[0].fields["event_id"], "event-1");
    assert_eq!(page.entries[0].fields["sender_id"], "[REDACTED]");
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("message body")
    );
}

#[test]
fn explicit_leases_capture_redacted_content_hide_it_per_reader_and_erase_on_last_drop() {
    let buffer = LogBuffer::new(vec!["private-value".to_owned()]);
    let first = buffer.capture_content();
    let second = buffer.capture_content();
    capture(&buffer, || {
        let span = tracing::info_span!(
            "plugin",
            plugin_id = "dev.bkm.ping",
            instance_id = "ping-1",
            event_id = "event-1",
            invocation_id = "invocation-1",
            command_name = "ping"
        );
        let _entered = span.enter();
        tracing::info!(target: MESSAGE_TRACE_TARGET, record_kind = "message.sent",
            direction = "outbound", status = "succeeded", content = "pong private-value", "message sent");
        tracing::info!(content = "runtime body must stay redacted", "runtime event");
    });
    assert_eq!(
        messages(&buffer, true).entries[0].content.as_deref(),
        Some("pong [REDACTED]")
    );
    assert_eq!(
        messages(&buffer, true).entries[0].instance_id.as_deref(),
        Some("ping-1")
    );
    assert_eq!(
        messages(&buffer, true).entries[0].fields["command_name"],
        "ping"
    );
    assert_eq!(messages(&buffer, false).entries[0].content, None);
    assert_eq!(
        buffer.snapshot(None, 1, None, None).entries[0].fields["content"],
        "[REDACTED]"
    );
    drop(first);
    assert!(messages(&buffer, true).entries[0].content.is_some());
    drop(second);
    assert_eq!(messages(&buffer, true).entries[0].content, None);
    assert!(
        buffer
            .entries
            .lock()
            .unwrap()
            .1
            .iter()
            .all(|entry| entry.content.is_none())
    );
    capture(
        &buffer,
        || tracing::info!(target: MESSAGE_TRACE_TARGET, content = "later body", "later"),
    );
    let _new = buffer.capture_content();
    assert!(
        messages(&buffer, true)
            .entries
            .iter()
            .all(|entry| entry.content.is_none())
    );
}

#[test]
fn category_pagination_advances_over_other_records_and_never_guesses_instance() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        tracing::info!("runtime");
        tracing::info!(target: MESSAGE_TRACE_TARGET, instance_id = "one", "first message");
        tracing::warn!("other runtime");
        tracing::info!(target: MESSAGE_TRACE_TARGET, instance_id = "one", "second message");
        tracing::info!(target: MESSAGE_TRACE_TARGET, "unattributed inbound");
    });
    let select = || LogSelection {
        category: LogCategory::Message,
        instance: Some("one"),
        ..Default::default()
    };
    let first = buffer.filtered_snapshot(Some(0), 1, select());
    assert_eq!(first.entries[0].id, 2);
    assert_eq!(first.next_cursor, 3);
    assert!(first.has_more);
    let next = buffer.filtered_snapshot(Some(first.next_cursor), 1, select());
    assert_eq!(next.entries[0].id, 4);
    assert_eq!(next.next_cursor, 5);
    assert!(!next.has_more);
    let all = buffer.filtered_snapshot(
        None,
        200,
        LogSelection {
            category: LogCategory::All,
            ..Default::default()
        },
    );
    assert_eq!(all.entries.len(), 5);
    let reset = buffer.filtered_stream_snapshot(Some(1), Some("old-process"), 1, select());
    assert!(reset.dropped);
    assert_eq!(reset.entries[0].id, 4);
}

#[test]
fn message_content_is_bounded_and_known_secret_prefix_cannot_leak_at_cutoff() {
    let secret = "q".repeat(3000);
    let buffer = LogBuffer::new(vec![secret.clone()]);
    let _lease = buffer.capture_content();
    capture(
        &buffer,
        || tracing::info!(target: MESSAGE_TRACE_TARGET, content = secret.as_str(), "large"),
    );
    let body = messages(&buffer, true).entries.remove(0).content.unwrap();
    assert!(body.len() <= 2048);
    assert!(!body.contains('q'));
    assert!(body.contains("[REDACTED]"));
}

#[test]
fn old_formatter_cannot_publish_body_into_a_new_capture_epoch() {
    struct BlockedBody {
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl std::fmt::Debug for BlockedBody {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.entered.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
            formatter.write_str("old private body")
        }
    }
    let buffer = LogBuffer::new(Vec::new());
    let old = buffer.capture_content();
    let (entered, started) = std::sync::mpsc::channel();
    let (resume, blocked) = std::sync::mpsc::channel();
    let writer_buffer = buffer.clone();
    let writer = std::thread::spawn(move || {
        let body = BlockedBody {
            entered,
            resume: std::sync::Mutex::new(blocked),
        };
        capture(&writer_buffer, || {
            tracing::info!(target: MESSAGE_TRACE_TARGET, content = ?body, "old event");
        });
    });
    started
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    drop(old);
    let _new = buffer.capture_content();
    resume.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(messages(&buffer, true).entries[0].content, None);
    capture(
        &buffer,
        || tracing::info!(target: MESSAGE_TRACE_TARGET, content = "new body", "new event"),
    );
    assert_eq!(
        messages(&buffer, true).entries[1].content.as_deref(),
        Some("new body")
    );
}
