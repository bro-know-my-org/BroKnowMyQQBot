use super::*;
use crate::{CommitOptions, LogicalStateEntry, OutboxOrigin};
use plugin_api::{PluginCommand, PluginEventEnvelope};
use std::sync::atomic::{AtomicBool, Ordering};
const ID: &str = "dev.test.plugin/default";

#[test]
fn individually_valid_snapshots_cannot_commit_an_oversized_merged_database() {
    let store = PluginStore::in_memory().unwrap();
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..super::super::MAX_ROWS {
        transaction
            .execute(
                "INSERT INTO plugin_state VALUES ('unaffected', ?, X'00', 1)",
                [index.to_string()],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    store.logical_snapshot().unwrap();
    import().validate().unwrap();
    assert!(matches!(
        store.restore_logical_offline(
            &uuid::Uuid::new_v4().to_string(),
            &import(),
            &[],
            &[ID.into()],
            |_| panic!("oversized merge reached journal callback"),
            || panic!("oversized merge reached config callback")
        ),
        Err(StoreError::InvalidLogicalSnapshot)
    ));
    assert_eq!(
        store.logical_snapshot().unwrap().state.len(),
        super::super::MAX_ROWS
    );
}

fn seed(store: &PluginStore) -> LogicalSnapshot {
    let command = PluginCommand {
        command_id: "old-command".into(),
        kind: "message.reply".into(),
        idempotency_key: Some("keep-this-key".into()),
        deadline_ms: None,
        payload: serde_json::json!({"content":"old"}),
    };
    let origin = OutboxOrigin {
        source_event_id: "historical".into(),
        adapter_id: "mock".into(),
        platform: None,
        reply_target: None,
        source_message_id: None,
    };
    store
        .commit(
            ID,
            "old-invocation",
            &[plugin_api::StateOp::Put {
                key: "business".into(),
                value: b"current".to_vec(),
                expected_revision: None,
            }],
            &[command],
            CommitOptions::new(1024).with_origin(&origin),
        )
        .unwrap();
    let connection = store.connection.lock().unwrap();
    connection
        .execute(
            "INSERT INTO scheduled_tasks VALUES (?, 'old-task', 'mock', 1, '{}', 'pending')",
            [ID],
        )
        .unwrap();
    connection.execute("INSERT INTO plugin_deliveries (instance_id,event_id,delivery_id,invocation_id,attempt,envelope,origin,status,next_attempt_ms,updated_at_ms)
        VALUES (?, 'historical', 'd', 'i', 1, '{}', '{}', 'committed', 0, 1)", [ID]).unwrap();
    drop(connection);
    store.logical_snapshot().unwrap()
}
fn import() -> LogicalSnapshot {
    LogicalSnapshot {
        installations: vec![],
        state: vec![LogicalStateEntry {
            instance_id: ID.into(),
            key: "business".into(),
            value: b"restored".to_vec(),
            revision: 1,
        }],
    }
}

#[test]
fn restore_is_atomic_and_never_replays_or_rewinds_execution_history() {
    let store = PluginStore::in_memory().unwrap();
    seed(&store);
    let id = uuid::Uuid::new_v4().to_string();
    store
        .restore_logical_offline(
            &id,
            &import(),
            &[],
            &[ID.into()],
            |old| {
                assert_eq!(old.state[0].value, b"current");
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
    assert!(store.logical_restore_committed(&id).unwrap());
    let restored = store.logical_snapshot().unwrap();
    assert_eq!(restored.state[0].value, b"restored");
    assert_eq!(restored.state[0].revision, 2);
    assert!(store.pending_commands(ID).unwrap().is_empty());
    let connection = store.connection.lock().unwrap();
    for (table, expected) in [
        ("command_ledger", "cancelled"),
        ("command_outbox", "cancelled"),
        ("plugin_deliveries", "retired"),
        ("scheduled_tasks", "cancelled"),
    ] {
        let status: String = connection
            .query_row(&format!("SELECT status FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, expected);
    }
    let key: String = connection
        .query_row("SELECT idempotency_key FROM command_ledger", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(key, "keep-this-key");
    drop(connection);
    // An adapter redelivery of the retired event cannot reopen it.
    let envelope: PluginEventEnvelope = serde_json::from_value(serde_json::json!({"protocol_version":"1.4", "event_id":"historical", "delivery_id":"new", "invocation_id":"new", "received_at_ms":1,"adapter_id":"mock", "event_type":"message.created", "payload":{}, "extensions":[]})).unwrap();
    let origin = OutboxOrigin {
        source_event_id: "historical".into(),
        adapter_id: "mock".into(),
        platform: None,
        reply_target: None,
        source_message_id: None,
    };
    assert!(
        store
            .begin_delivery(ID, &envelope, &origin, 2)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .restore_logical_offline(
                &id,
                &import(),
                &[],
                &[ID.into()],
                |_| panic!("replay"),
                || Ok(())
            )
            .is_err()
    );
}

#[test]
fn failed_prepare_or_config_commit_rolls_back_database_and_marker() {
    let store = PluginStore::in_memory().unwrap();
    let before = seed(&store);
    for fail_prepare in [true, false] {
        let id = uuid::Uuid::new_v4().to_string();
        let called = AtomicBool::new(false);
        let result = store.restore_logical_offline(
            &id,
            &import(),
            &[],
            &[ID.into()],
            |_| {
                if fail_prepare {
                    Err(StoreError::InvalidLogicalSnapshot)
                } else {
                    Ok(())
                }
            },
            || {
                called.store(true, Ordering::SeqCst);
                Err(StoreError::InvalidLogicalSnapshot)
            },
        );
        assert!(result.is_err());
        assert_eq!(called.load(Ordering::SeqCst), !fail_prepare);
        assert_eq!(store.logical_snapshot().unwrap(), before);
        assert!(!store.logical_restore_committed(&id).unwrap());
        assert_eq!(
            store
                .command_status(ID, "old-invocation", "old-command")
                .unwrap()
                .as_deref(),
            Some("pending")
        );
    }
}

#[test]
fn stale_installations_and_invalid_scope_refuse_before_any_callback() {
    let store = PluginStore::in_memory().unwrap();
    seed(&store);
    let item = installation();
    store.upsert_installation(&item).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        store.restore_logical_offline(
            &id,
            &import(),
            &[],
            &[ID.into()],
            |_| panic!("stale"),
            || Ok(())
        ),
        Err(StoreError::InstallationConflict)
    ));
    assert!(
        store
            .restore_logical_offline(&id, &import(), &[item], &[], |_| panic!("scope"), || Ok(()))
            .is_err()
    );
}

fn installation() -> PluginInstallation {
    serde_json::from_value(serde_json::json!({
        "plugin_id":"dev.test.plugin","metadata":{"default_locale":"en","locales":{"en":{"name":"Test","description":""}}},
        "instance_id":ID,"version":"0.1.0","package_path":"unused","package_sha256":"unused","source":"local",
        "trust_level":"local-wasm","signature_status":"unsigned","requested_permissions":[],"granted_permissions":[],"config":{},"enabled":false,"installed_at_ms":1,"updated_at_ms":1
    })).unwrap()
}

#[test]
fn replacement_size_accounts_for_preserved_installation_timestamp() {
    let store = PluginStore::in_memory().unwrap();
    let mut current = installation();
    current.installed_at_ms = i64::MAX;
    store.upsert_installation(&current).unwrap();
    let mut replacement = installation();
    replacement.updated_at_ms = chrono_timestamp();
    replacement.source.clear();
    replacement.source = "x"
        .repeat(super::super::MAX_RECORD_BYTES - serde_json::to_vec(&replacement).unwrap().len());
    let imported = LogicalSnapshot {
        installations: vec![replacement],
        state: vec![],
    };
    imported.validate().unwrap();
    assert!(matches!(
        store.restore_logical_offline(
            &uuid::Uuid::new_v4().to_string(),
            &imported,
            &[current],
            &[ID.into()],
            |_| panic!("oversized replacement reached prepare"),
            || panic!("oversized replacement reached config")
        ),
        Err(StoreError::InvalidLogicalSnapshot)
    ));
}

#[test]
fn empty_source_namespace_clears_only_the_explicit_affected_target() {
    let store = PluginStore::in_memory().unwrap();
    seed(&store);
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO plugin_state VALUES ('unrelated', 'keep', X'01', 1)",
            [],
        )
        .unwrap();
    store
        .restore_logical_offline(
            &uuid::Uuid::new_v4().to_string(),
            &LogicalSnapshot {
                installations: vec![],
                state: vec![],
            },
            &[],
            &[ID.into()],
            |_| Ok(()),
            || Ok(()),
        )
        .unwrap();
    let restored = store.logical_snapshot().unwrap();
    assert_eq!(restored.state.len(), 1);
    assert_eq!(restored.state[0].instance_id, "unrelated");
}
