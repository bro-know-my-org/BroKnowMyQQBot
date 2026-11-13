use super::*;
use tracing_subscriber::layer::SubscriberExt as _;

fn capture(buffer: &LogBuffer, action: impl FnOnce()) {
    tracing::subscriber::with_default(tracing_subscriber::registry().with(buffer.layer()), action);
}

#[test]
fn captures_events_and_excludes_message_target() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        tracing::info!(target: "runtime", answer = 42, "started");
        tracing::info!(target: super::super::MESSAGE_LOG_TARGET, "private message");
    });
    let page = buffer.snapshot(None, 100, None, None);
    assert_eq!(page.entries.len(), 1);
    let entry = &page.entries[0];
    assert_eq!(entry.message, "started");
    assert_eq!(entry.level, "INFO");
    assert_eq!(entry.target, "runtime");
    assert_eq!(entry.fields["answer"], "42");
    assert!(chrono::DateTime::parse_from_rfc3339(&entry.timestamp).is_ok());
}

#[test]
fn explicit_instance_and_span_updates_override_ancestors() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        let parent = tracing::info_span!("parent", instance_id = "first", request = 7);
        let _entered = parent.enter();
        tracing::info!("inherited");
        parent.record("instance_id", "changed");
        tracing::info!(instance_id = "event", "override");
        tracing::info!(plugin_id = "unrelated", "updated");
        let child = tracing::info_span!("child", instance_id = "nested");
        let _child = child.enter();
        tracing::info!("child");
    });
    let page = buffer.snapshot(None, 100, None, None);
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| entry.instance_id.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("first"),
            Some("event"),
            Some("changed"),
            Some("nested")
        ]
    );
    assert_eq!(page.entries[0].fields["request"], "7");
    let isolated = LogBuffer::new(Vec::new());
    capture(&isolated, || {
        tracing::info!(plugin_id = "no-instance", "global");
    });
    assert!(
        isolated.snapshot(None, 10, None, None).entries[0]
            .instance_id
            .is_none()
    );
}

#[test]
fn redacts_fields_known_credentials_errors_and_boundary_prefixes() {
    let secret = "private-credential";
    let buffer = LogBuffer::new(vec![secret.to_owned(), "quoted\"credential".to_owned()]);
    capture(&buffer, || {
        tracing::warn!(access_token = secret, content = "text", app_id = "123", detail = secret,
            error = ?std::io::Error::other(secret), "login {secret}");
        tracing::info!("{}private-credential", "x".repeat(2040));
        tracing::info!(detail = ?"quoted\"credential", "escaped");
    });
    let page = buffer.snapshot(None, 100, None, None);
    let serialized = serde_json::to_string(&page).unwrap();
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains("private-"));
    assert!(!serialized.contains("quoted"));
    assert_eq!(page.entries[0].fields["access_token"], "[REDACTED]");
    assert_eq!(page.entries[0].fields["content"], "[REDACTED]");
    assert_eq!(page.entries[0].fields["app_id"], "[REDACTED]");
    assert!(page.entries[0].message.contains("[REDACTED]"));
    assert!(!format!("{buffer:?}").contains(secret));
}

#[test]
fn bounds_unicode_debug_values_and_controls() {
    struct Huge;
    impl fmt::Debug for Huge {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..1_000_000 {
                f.write_str("中文")?;
            }
            Ok(())
        }
    }
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        tracing::info!(detail = ?Huge, a = 1, b = 2, c = 3, d = 4, e = 5, f = 6,
            g = 7, h = 8, i = 9, j = 10, k = 11, l = 12, m = 13, n = 14,
            o = 15, p = 16, q = 17, "{}\u{1b}\n", "纸".repeat(1000));
        tracing::info!("controls\u{1b}\n\r");
    });
    let page = buffer.snapshot(None, 100, None, None);
    let entry = &page.entries[0];
    assert!(entry.message.len() <= 2048);
    assert_eq!(entry.fields.len(), FIELD_COUNT);
    assert!(entry.fields["detail"].len() <= 512);
    assert!(entry.fields["detail"].starts_with("中文"));
    assert!(
        page.entries[1]
            .message
            .chars()
            .all(|character| !character.is_control())
    );
}

#[test]
fn threshold_filter_and_instance_match_are_exact() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        tracing::debug!(instance_id = "one", "debug");
        tracing::info!(instance_id = "one", "info");
        tracing::warn!(instance_id = "one", "warn");
        tracing::error!(instance_id = "one-more", "error");
    });
    let page = buffer.snapshot(None, 10, Some("WARN"), Some("one"));
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].message, "warn");
    assert_eq!(page.next_cursor, 4);
}

#[test]
fn cursor_paginates_and_advances_across_nonmatches() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        tracing::warn!("first");
        tracing::info!("skip");
        tracing::warn!("second");
        tracing::info!("skip-again");
    });
    let initial = buffer.snapshot(None, 1, None, None);
    assert_eq!(initial.entries[0].id, 4);
    assert_eq!(initial.next_cursor, 4);
    let first = buffer.snapshot(Some(0), 1, Some("WARN"), None);
    assert_eq!(first.entries[0].id, 1);
    assert_eq!(first.next_cursor, 2);
    assert!(first.has_more);
    let second = buffer.snapshot(Some(first.next_cursor), 1, Some("WARN"), None);
    assert_eq!(second.entries[0].id, 3);
    assert_eq!(second.next_cursor, 4);
    assert!(!second.has_more);
    let empty = buffer.snapshot(Some(0), 1, Some("ERROR"), None);
    assert!(empty.entries.is_empty());
    assert_eq!(empty.next_cursor, 4);
}

#[test]
fn ring_overflow_and_process_reset_report_dropped_windows() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || {
        for sequence in 0..1005 {
            tracing::info!(sequence, "event");
        }
    });
    let all = buffer.snapshot(Some(0), CAPACITY, None, None);
    assert_eq!(all.entries.len(), CAPACITY);
    assert_eq!(all.entries[0].id, 6);
    assert_eq!(all.next_cursor, 1005);
    assert!(all.dropped);
    assert!(!buffer.snapshot(Some(5), 1, None, None).dropped);
    let reset = buffer.snapshot(Some(2000), 2, None, None);
    assert!(reset.dropped);
    assert_eq!(reset.entries[0].id, 1004);
    assert_eq!(reset.next_cursor, 1005);
    let fresh = LogBuffer::new(Vec::new()).snapshot(Some(1005), 10, None, None);
    assert!(fresh.dropped);
    assert_ne!(fresh.session_id, reset.session_id);
    assert_eq!(fresh.next_cursor, 0);
}

#[test]
fn watch_retains_latest_without_receivers_and_reset_checks_session() {
    let buffer = LogBuffer::new(Vec::new());
    capture(&buffer, || tracing::info!("before subscription"));
    let mut updates = buffer.subscribe();
    assert_eq!(*updates.borrow_and_update(), 1);
    capture(&buffer, || {
        tracing::info!("second");
        tracing::info!("third");
    });
    assert!(updates.has_changed().unwrap());
    assert_eq!(*updates.borrow_and_update(), 3);
    for after in [0, 2, 3, 99] {
        let page = buffer.stream_snapshot(Some(after), Some("old-session"), 2, None, None);
        assert!(page.dropped);
        assert_eq!(page.entries[0].id, 2);
        assert_eq!(page.next_cursor, 3);
    }
    let fresh =
        LogBuffer::new(Vec::new()).stream_snapshot(Some(0), Some("old-session"), 2, None, None);
    assert!(fresh.dropped);
    assert_eq!(fresh.next_cursor, 0);
}
