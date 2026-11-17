//! Whole-instance CAS for management edits, including writes from chat and restore.
use super::{MAX_STATE_ENTRIES, PluginStore, StateOp, StateValue, StoreError, apply_state_op};
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;

pub(super) fn initialize(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_state_generations (
            instance_id TEXT PRIMARY KEY,
            revision INTEGER NOT NULL CHECK(typeof(revision) = 'integer' AND revision > 0)
        );
        INSERT OR IGNORE INTO plugin_state_generations(instance_id, revision)
            SELECT DISTINCT instance_id, 1 FROM plugin_state;
        CREATE TRIGGER IF NOT EXISTS plugin_state_insert_generation AFTER INSERT ON plugin_state
        BEGIN
            INSERT INTO plugin_state_generations VALUES (NEW.instance_id, 1)
            ON CONFLICT(instance_id) DO UPDATE SET revision = revision + 1;
        END;
        CREATE TRIGGER IF NOT EXISTS plugin_state_update_generation AFTER UPDATE ON plugin_state
        BEGIN
            INSERT INTO plugin_state_generations VALUES (OLD.instance_id, 1)
            ON CONFLICT(instance_id) DO UPDATE SET revision = revision + 1;
            INSERT INTO plugin_state_generations(instance_id,revision)
                SELECT NEW.instance_id,1 WHERE NEW.instance_id != OLD.instance_id
            ON CONFLICT(instance_id) DO UPDATE SET revision = revision + 1;
        END;
        CREATE TRIGGER IF NOT EXISTS plugin_state_delete_generation AFTER DELETE ON plugin_state
        BEGIN
            INSERT INTO plugin_state_generations VALUES (OLD.instance_id, 1)
            ON CONFLICT(instance_id) DO UPDATE SET revision = revision + 1;
        END;",
    )?;
    Ok(())
}

fn generation(connection: &Connection, id: &str) -> Result<u64, StoreError> {
    Ok(connection
        .query_row(
            "SELECT revision FROM plugin_state_generations WHERE instance_id = ?",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn check_quota(connection: &Connection, id: &str, quota: u64) -> Result<(), StoreError> {
    let (entries, used): (u64, u64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(CAST(key AS BLOB)) + length(value)), 0)
         FROM plugin_state WHERE instance_id = ?",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if entries > MAX_STATE_ENTRIES {
        return Err(StoreError::StateEntryLimit(entries));
    }
    if used > quota {
        return Err(StoreError::QuotaExceeded { used, quota });
    }
    Ok(())
}

impl PluginStore {
    /// Generation and rows share one `SQLite` read snapshot, including other connections.
    pub fn management_snapshot(
        &self,
        id: &str,
    ) -> Result<(u64, BTreeMap<String, StateValue>), StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let revision = generation(&transaction, id)?;
        check_quota(&transaction, id, 64 * 1024 * 1024)?;
        let mut statement = transaction.prepare(
            "SELECT key, value, revision FROM plugin_state WHERE instance_id = ? ORDER BY key",
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((
                row.get(0)?,
                StateValue {
                    value: row.get(1)?,
                    revision: row.get(2)?,
                },
            ))
        })?;
        let state = rows.collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok((revision, state))
    }

    /// No command ledger, task queue or outbox is reachable from this transaction.
    pub fn commit_management(
        &self,
        id: &str,
        expected_revision: u64,
        expected_installation: Option<i64>,
        operations: &[StateOp],
        quota: u64,
    ) -> Result<u64, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if generation(&transaction, id)? != expected_revision {
            return Err(StoreError::CasConflict(String::new()));
        }
        if let Some(expected) = expected_installation {
            let current = transaction
                .query_row(
                    "SELECT updated_at_ms FROM plugin_installations WHERE instance_id = ?",
                    [id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if current != Some(expected) {
                return Err(StoreError::InstallationConflict);
            }
        }
        for operation in operations {
            apply_state_op(&transaction, id, operation)?;
        }
        check_quota(&transaction, id, quota)?;
        let revision = generation(&transaction, id)?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Only bounded IDs from the status sample are queried; no private values are loaded.
    pub fn management_revisions(
        &self,
        ids: &[String],
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let mut connection = self.connection.try_lock().map_err(StoreError::from)?;
        let previous: u64 = connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
        connection.busy_timeout(std::time::Duration::from_millis(20))?;
        let result = (|| {
            let transaction = connection.transaction()?;
            ids.iter()
                .take(256)
                .map(|id| {
                    if id.len() > 512 {
                        return Err(StoreError::InvalidStatus);
                    }
                    Ok((id.clone(), generation(&transaction, id)?.to_string()))
                })
                .collect()
        })();
        connection.busy_timeout(std::time::Duration::from_millis(previous))?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommitOptions;
    fn put(key: &str, value: &[u8], revision: Option<u64>) -> StateOp {
        StateOp::Put {
            key: key.into(),
            value: value.into(),
            expected_revision: revision,
        }
    }

    #[test]
    fn chat_write_and_delete_recreate_invalidate_management_cas() {
        let store = PluginStore::in_memory().unwrap();
        let (zero, rows) = store.management_snapshot("a").unwrap();
        assert_eq!(zero, 0);
        assert!(rows.is_empty());
        store
            .commit(
                "a",
                "chat",
                &[put("repos/A", b"a/b", None)],
                &[],
                CommitOptions::new(1024),
            )
            .unwrap();
        assert!(matches!(
            store.commit_management(
                "a",
                zero,
                None,
                &[put("repos/A", b"other/repo", None)],
                1024
            ),
            Err(StoreError::CasConflict(_))
        ));
        let (revision, _) = store.management_snapshot("a").unwrap();
        let delete = StateOp::Delete {
            key: "repos/A".into(),
            expected_revision: Some(1),
        };
        let next = store
            .commit_management(
                "a",
                revision,
                None,
                &[delete, put("repos/A", b"a/c", None)],
                1024,
            )
            .unwrap();
        assert!(next > revision);
        assert!(matches!(
            store.commit_management("a", revision, None, &[], 1024),
            Err(StoreError::CasConflict(_))
        ));
        assert_eq!(store.management_snapshot("b").unwrap().0, 0);
    }

    #[test]
    fn quota_and_row_cas_rollback_generation_and_all_operations() {
        let store = PluginStore::in_memory().unwrap();
        let initial = store
            .commit_management("a", 0, None, &[put("repos/A", b"a/b", None)], 1024)
            .unwrap();
        for operations in [
            vec![
                put("repos/B", b"a/c", None),
                put("repos/A", b"x/y", Some(0)),
            ],
            vec![put("repos/B", &vec![0; 1024], None)],
        ] {
            assert!(
                store
                    .commit_management("a", initial, None, &operations, 1024)
                    .is_err()
            );
            let (revision, state) = store.management_snapshot("a").unwrap();
            assert_eq!(revision, initial);
            assert_eq!(state.len(), 1);
            assert_eq!(state["repos/A"].value, b"a/b");
        }
        store.replace_state("a", &BTreeMap::new()).unwrap();
        assert!(store.management_snapshot("a").unwrap().0 > initial);
        assert!(matches!(
            store.commit_management(
                "a",
                store.management_snapshot("a").unwrap().0,
                Some(1),
                &[],
                1024
            ),
            Err(StoreError::InstallationConflict)
        ));
    }

    #[test]
    fn migration_existing_rows_and_external_connection_writes_are_observed() {
        let directory =
            std::env::temp_dir().join(format!("bkm-management-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.db");
        let other = Connection::open(&path).unwrap();
        other.execute_batch("CREATE TABLE plugin_state (instance_id TEXT NOT NULL,key TEXT NOT NULL,value BLOB NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(instance_id,key)); INSERT INTO plugin_state VALUES ('a','repos/A',x'612f62',1);").unwrap();
        let store = PluginStore::open(&path).unwrap();
        let (initial, _) = store.management_snapshot("a").unwrap();
        assert!(initial > 0);
        other
            .execute(
                "UPDATE plugin_state SET value = x'612f63' WHERE instance_id='a'",
                [],
            )
            .unwrap();
        assert!(store.management_snapshot("a").unwrap().0 > initial);
        assert!(matches!(
            store.commit_management("a", initial, None, &[], 1024),
            Err(StoreError::CasConflict(_))
        ));
        let expected = store.management_snapshot("a").unwrap().0.to_string();
        assert_eq!(
            store.management_revisions(&["a".into()]).unwrap()["a"],
            expected
        );
        let lock = store.connection.lock().unwrap();
        assert!(matches!(
            store.management_revisions(&["a".into()]),
            Err(StoreError::StatusBusy)
        ));
        drop(lock);
        drop(store);
        drop(other);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn byte_quota_handles_unicode_nul_and_supported_large_state() {
        let store = PluginStore::in_memory().unwrap();
        for key in ["é", "a\0trailing"] {
            assert!(matches!(
                store.commit_management("a", 0, None, &[put(key, b"", None)], 1),
                Err(StoreError::QuotaExceeded { .. })
            ));
            assert_eq!(store.management_snapshot("a").unwrap().0, 0);
        }
        let large = vec![7; 17 * 1024 * 1024];
        let revision = store
            .commit_management(
                "a",
                0,
                None,
                &[put("large", &large, None)],
                64 * 1024 * 1024,
            )
            .unwrap();
        let (snapshot_revision, state) = store.management_snapshot("a").unwrap();
        assert_eq!(snapshot_revision, revision);
        assert_eq!(state["large"].value, large);
    }

    #[test]
    fn externally_overfull_snapshot_is_rejected_instead_of_truncated() {
        let store = PluginStore::in_memory().unwrap();
        {
            let mut connection = store.connection.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            for index in 0..=MAX_STATE_ENTRIES {
                transaction
                    .execute(
                        "INSERT INTO plugin_state VALUES ('a', ?, x'61', 1)",
                        [format!("row/{index}")],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        assert!(matches!(
            store.management_snapshot("a"),
            Err(StoreError::StateEntryLimit(4097))
        ));
    }
}
