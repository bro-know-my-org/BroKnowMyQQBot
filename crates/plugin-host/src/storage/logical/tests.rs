use super::*;
use rusqlite::params;

fn seed(store: &PluginStore, value: &[u8]) {
    store.connection.lock().unwrap().execute(
        "INSERT OR REPLACE INTO plugin_state VALUES ('dev.test.plugin/default', 'business/key', ?, 7)",
        [value],
    ).unwrap();
}

#[test]
fn snapshot_preserves_opaque_values_and_has_no_execution_tables() {
    let store = PluginStore::in_memory().unwrap();
    seed(&store, &[0, 255, 3]);
    store.connection.lock().unwrap().execute(
        "INSERT INTO scheduled_tasks VALUES ('dev.test.plugin/default', 'old', 'mock', 1, '{}', 'pending')", [],
    ).unwrap();
    let snapshot = store.logical_snapshot().unwrap();
    assert_eq!(snapshot.state[0].value, [0, 255, 3]);
    assert_eq!(snapshot.state[0].revision, 7);
    let encoded = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(encoded.as_object().unwrap().len(), 2);
    assert!(encoded.get("scheduled_tasks").is_none());
    assert_eq!(
        serde_json::from_value::<LogicalSnapshot>(encoded).unwrap(),
        snapshot
    );
}

#[test]
fn oversized_database_values_fail_before_copying_them() {
    let store = PluginStore::in_memory().unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO plugin_state VALUES ('dev.test.plugin/default', 'key', zeroblob(?), 1)",
            [MAX_VALUE_BYTES + 1],
        )
        .unwrap();
    assert!(matches!(
        store.logical_snapshot(),
        Err(StoreError::InvalidLogicalSnapshot)
    ));
}

#[test]
fn excessive_rows_and_invalid_keys_revisions_are_rejected() {
    let store = PluginStore::in_memory().unwrap();
    seed(&store, b"original");
    let original = store.logical_snapshot().unwrap();
    let mut invalid = original.clone();
    invalid.state.push(invalid.state[0].clone());
    assert!(invalid.validate().is_err());
    for key in ["a/../b", "a//b", ".", ""] {
        invalid = original.clone();
        invalid.state[0].key = key.to_owned();
        assert!(invalid.validate().is_err());
    }
    for revision in [0, u64::MAX] {
        invalid = original.clone();
        invalid.state[0].revision = revision;
        assert!(invalid.validate().is_err());
    }
    let mut connection = store.connection.lock().unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..=MAX_ROWS {
        transaction
            .execute(
                "INSERT INTO plugin_state VALUES ('other', ?, X'', 1)",
                [index.to_string()],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);
    assert!(matches!(
        store.logical_snapshot(),
        Err(StoreError::InvalidLogicalSnapshot)
    ));
}

#[test]
fn external_transaction_cannot_split_state_and_installation_snapshot() {
    let path = std::env::temp_dir().join(format!("bkm-logical-{}.db", uuid::Uuid::new_v4()));
    let store = PluginStore::open(&path).unwrap();
    seed(&store, b"before");
    store.upsert_installation(&installation()).unwrap();
    let mut external = rusqlite::Connection::open(&path).unwrap();
    external.execute_batch("PRAGMA journal_mode=WAL").unwrap();
    let transaction = external.transaction().unwrap();
    transaction
        .execute(
            "UPDATE plugin_state SET value = ?",
            params![b"after".as_slice()],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE plugin_installations SET version = '0.2.0', updated_at_ms = 2",
            [],
        )
        .unwrap();
    let snapshot = store
        .logical_snapshot_inner(|| transaction.commit().unwrap())
        .unwrap();
    assert_eq!(snapshot.state[0].value, b"before");
    assert_eq!(snapshot.installations[0].version, "0.1.0");
    let new = store.logical_snapshot().unwrap();
    assert_eq!(new.state[0].value, b"after");
    assert_eq!(new.installations[0].version, "0.2.0");
    drop((store, external));
    std::fs::remove_file(path).unwrap();
}

fn installation() -> PluginInstallation {
    PluginInstallation {
        plugin_id: "dev.test.plugin".into(),
        metadata: plugin_api::PluginMetadata::single_locale("en", "Test", "Fixture"),
        instance_id: "dev.test.plugin/default".into(),
        version: "0.1.0".into(),
        package_path: "unused.bkm-plugin".into(),
        package_sha256: "0".repeat(64),
        source: "local".into(),
        trust_level: "local-wasm".into(),
        signature_status: "unsigned".into(),
        requested_permissions: vec![],
        granted_permissions: vec![],
        config: std::collections::BTreeMap::default(),
        enabled: false,
        installed_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn oversized_directly_constructed_records_are_rejected() {
    let mut item = installation();
    item.source = "x".repeat(MAX_RECORD_BYTES + 1);
    assert!(
        LogicalSnapshot {
            installations: vec![item],
            state: vec![]
        }
        .validate()
        .is_err()
    );
}
