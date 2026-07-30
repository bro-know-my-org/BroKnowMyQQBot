//! SQLite-backed BPP private state, command ledger, and outbox.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use bot_core::MessageTarget;
use plugin_api::{PluginCommand, StateOp, StateValue};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_STATE_ENTRIES: u64 = 4_096;
const MAX_SCHEDULED_TASKS: u64 = 1_024;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("plugin store SQLite operation failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("plugin store lock was poisoned")]
    Poisoned,
    #[error("invalid plugin state key `{0}`")]
    InvalidKey(String),
    #[error("plugin state compare-and-swap failed for key `{0}`")]
    CasConflict(String),
    #[error("plugin state quota exceeded: {used} > {quota} bytes")]
    QuotaExceeded { used: u64, quota: u64 },
    #[error("plugin state entry limit exceeded: {0} > {MAX_STATE_ENTRIES}")]
    StateEntryLimit(u64),
    #[error("plugin scheduled-task quota exceeded")]
    ScheduleQuotaExceeded,
    #[error("plugin command could not be encoded")]
    CommandEncoding(#[from] serde_json::Error),
    #[error("plugin command outbox entry is missing execution origin")]
    MissingCommandOrigin,
    #[error("plugin command idempotency key was reused with different command data")]
    IdempotencyConflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxOrigin {
    pub source_event_id: String,
    pub adapter_id: String,
    pub reply_target: Option<MessageTarget>,
    pub source_message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingCommand {
    pub instance_id: String,
    pub invocation_id: String,
    pub origin: OutboxOrigin,
    pub command: PluginCommand,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScheduledTask {
    pub instance_id: String,
    pub task_id: String,
    pub adapter_id: String,
    pub run_at_ms: i64,
    pub payload: serde_json::Value,
}

#[derive(Clone)]
pub struct PluginStore {
    connection: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for PluginStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginStore")
            .finish_non_exhaustive()
    }
}

impl PluginStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS plugin_state (
                instance_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value BLOB NOT NULL,
                revision INTEGER NOT NULL,
                PRIMARY KEY (instance_id, key)
            );
            CREATE TABLE IF NOT EXISTS command_ledger (
                instance_id TEXT NOT NULL,
                invocation_id TEXT NOT NULL,
                command_id TEXT NOT NULL,
                idempotency_key TEXT,
                kind TEXT NOT NULL,
                payload BLOB NOT NULL,
                status TEXT NOT NULL,
                result BLOB,
                PRIMARY KEY (instance_id, invocation_id, command_id)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS command_idempotency
                ON command_ledger(instance_id, idempotency_key)
                WHERE idempotency_key IS NOT NULL;
            CREATE TABLE IF NOT EXISTS command_outbox (
                instance_id TEXT NOT NULL,
                invocation_id TEXT NOT NULL,
                command_id TEXT NOT NULL,
                payload BLOB NOT NULL,
                status TEXT NOT NULL,
                PRIMARY KEY (instance_id, invocation_id, command_id),
                FOREIGN KEY (instance_id, invocation_id, command_id)
                    REFERENCES command_ledger(instance_id, invocation_id, command_id)
            );
            CREATE TABLE IF NOT EXISTS scheduled_tasks (
                instance_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                adapter_id TEXT NOT NULL,
                run_at_ms INTEGER NOT NULL,
                payload BLOB NOT NULL,
                status TEXT NOT NULL,
                PRIMARY KEY (instance_id, task_id)
            );
            CREATE INDEX IF NOT EXISTS scheduled_tasks_due
                ON scheduled_tasks(status, run_at_ms);
            ",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn snapshot(&self, instance_id: &str) -> Result<BTreeMap<String, StateValue>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT key, value, revision FROM plugin_state WHERE instance_id = ? ORDER BY key",
        )?;
        let rows = statement.query_map([instance_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                StateValue {
                    value: row.get(1)?,
                    revision: row.get(2)?,
                },
            ))
        })?;
        rows.collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(StoreError::from)
    }

    pub(crate) fn replace_state(
        &self,
        instance_id: &str,
        state: &BTreeMap<String, StateValue>,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM plugin_state WHERE instance_id = ?",
            [instance_id],
        )?;
        for (key, value) in state {
            validate_key(key)?;
            transaction.execute(
                "INSERT INTO plugin_state(instance_id, key, value, revision)
                 VALUES (?, ?, ?, ?)",
                params![instance_id, key, value.value, value.revision],
            )?;
        }
        transaction.commit().map_err(StoreError::from)
    }

    pub fn commit(
        &self,
        instance_id: &str,
        invocation_id: &str,
        state_ops: &[StateOp],
        commands: &[PluginCommand],
        quota_bytes: u64,
        origin: Option<&OutboxOrigin>,
    ) -> Result<Vec<PluginCommand>, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let mut existing_idempotency_keys = 0_usize;
        let mut output_idempotency_keys = BTreeMap::new();
        for command in commands {
            let Some(key) = command.idempotency_key.as_deref() else {
                continue;
            };
            if output_idempotency_keys
                .insert(key, serde_json::to_vec(command)?)
                .is_some()
            {
                return Err(StoreError::IdempotencyConflict);
            }
            let existing = transaction
                .query_row(
                    "SELECT kind, payload FROM command_ledger
                     WHERE instance_id = ? AND idempotency_key = ?",
                    params![instance_id, key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()?;
            if let Some((kind, payload)) = existing {
                if kind != command.kind || payload != serde_json::to_vec(command)? {
                    return Err(StoreError::IdempotencyConflict);
                }
                existing_idempotency_keys += 1;
            }
        }
        if existing_idempotency_keys > 0 {
            if existing_idempotency_keys == commands.len() {
                return Ok(Vec::new());
            }
            return Err(StoreError::IdempotencyConflict);
        }
        for operation in state_ops {
            apply_state_op(&transaction, instance_id, operation)?;
        }
        let (entries, used): (u64, u64) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(key) + length(value)), 0)
             FROM plugin_state WHERE instance_id = ?",
            [instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if entries > MAX_STATE_ENTRIES {
            return Err(StoreError::StateEntryLimit(entries));
        }
        if used > quota_bytes {
            return Err(StoreError::QuotaExceeded {
                used,
                quota: quota_bytes,
            });
        }

        let mut queued = Vec::new();
        for command in commands {
            let origin = origin.ok_or(StoreError::MissingCommandOrigin)?;
            let payload = serde_json::to_vec(command)?;
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO command_ledger
                    (instance_id, invocation_id, command_id, idempotency_key, kind, payload, status)
                 VALUES (?, ?, ?, ?, ?, ?, 'pending')",
                params![
                    instance_id,
                    invocation_id,
                    command.command_id,
                    command.idempotency_key,
                    command.kind,
                    payload
                ],
            )?;
            if inserted == 1 {
                transaction.execute(
                    "INSERT INTO command_outbox
                        (instance_id, invocation_id, command_id, payload, status)
                     VALUES (?, ?, ?, ?, 'pending')",
                    params![
                        instance_id,
                        invocation_id,
                        command.command_id,
                        serde_json::to_vec(&PendingCommand {
                            instance_id: instance_id.to_owned(),
                            invocation_id: invocation_id.to_owned(),
                            origin: origin.clone(),
                            command: command.clone(),
                        })?
                    ],
                )?;
                queued.push(command.clone());
            }
        }
        transaction.commit()?;
        Ok(queued)
    }

    pub(crate) fn pending_commands(
        &self,
        instance_id: &str,
    ) -> Result<Vec<PendingCommand>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT payload FROM command_outbox
             WHERE instance_id = ? AND status = 'pending'
             ORDER BY rowid",
        )?;
        let rows = statement.query_map([instance_id], |row| {
            let payload = row.get::<_, Vec<u8>>(0)?;
            serde_json::from_slice(&payload).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    Box::new(error),
                )
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn mark_command(
        &self,
        instance_id: &str,
        invocation_id: &str,
        command_id: &str,
        status: &str,
        result: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let result = serde_json::to_vec(result)?;
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE command_ledger SET status = ?, result = ?
             WHERE instance_id = ? AND invocation_id = ? AND command_id = ?",
            params![status, result, instance_id, invocation_id, command_id],
        )?;
        transaction.execute(
            "UPDATE command_outbox SET status = ?
             WHERE instance_id = ? AND invocation_id = ? AND command_id = ?",
            params![status, instance_id, invocation_id, command_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn command_status(
        &self,
        instance_id: &str,
        invocation_id: &str,
        command_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                "SELECT status FROM command_ledger
                 WHERE instance_id = ? AND invocation_id = ? AND command_id = ?",
                params![instance_id, invocation_id, command_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub(crate) fn create_schedule(
        &self,
        task: &ScheduledTask,
        quota_bytes: u64,
    ) -> Result<bool, StoreError> {
        validate_key(&task.task_id)?;
        let payload = serde_json::to_vec(&task.payload)?;
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO scheduled_tasks
                    (instance_id, task_id, adapter_id, run_at_ms, payload, status)
                 VALUES (?, ?, ?, ?, ?, 'pending')",
            params![
                task.instance_id,
                task.task_id,
                task.adapter_id,
                task.run_at_ms,
                payload
            ],
        )?;
        if inserted == 1 {
            let (count, used): (u64, u64) = transaction.query_row(
                "SELECT COUNT(*), COALESCE(SUM(length(task_id) + length(adapter_id) + length(payload)), 0)
                 FROM scheduled_tasks WHERE instance_id = ?",
                [&task.instance_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if count > MAX_SCHEDULED_TASKS || used > quota_bytes {
                return Err(StoreError::ScheduleQuotaExceeded);
            }
        }
        transaction.commit()?;
        Ok(inserted == 1)
    }

    pub(crate) fn cancel_schedule(
        &self,
        instance_id: &str,
        task_id: &str,
    ) -> Result<bool, StoreError> {
        validate_key(task_id)?;
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .execute(
                "DELETE FROM scheduled_tasks
                 WHERE instance_id = ? AND task_id = ? AND status IN ('pending', 'firing')",
                params![instance_id, task_id],
            )
            .map(|updated| updated == 1)
            .map_err(StoreError::from)
    }

    pub(crate) fn recover_schedules(
        &self,
        instance_id: &str,
    ) -> Result<Vec<ScheduledTask>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "UPDATE scheduled_tasks SET status = 'pending'
             WHERE instance_id = ? AND status = 'firing'",
            [instance_id],
        )?;
        let mut statement = connection.prepare(
            "SELECT task_id, adapter_id, run_at_ms, payload
             FROM scheduled_tasks
             WHERE instance_id = ? AND status = 'pending'
             ORDER BY run_at_ms, task_id",
        )?;
        let rows = statement.query_map([instance_id], |row| {
            let payload = row.get::<_, Vec<u8>>(3)?;
            let payload = serde_json::from_slice(&payload).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Blob,
                    Box::new(error),
                )
            })?;
            Ok(ScheduledTask {
                instance_id: instance_id.to_owned(),
                task_id: row.get(0)?,
                adapter_id: row.get(1)?,
                run_at_ms: row.get(2)?,
                payload,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub(crate) fn requeue_firing_schedules(&self, instance_id: &str) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "UPDATE scheduled_tasks SET status = 'pending'
             WHERE instance_id = ? AND status = 'firing'",
            [instance_id],
        )?;
        Ok(())
    }

    pub(crate) fn claim_schedule(
        &self,
        instance_id: &str,
        task_id: &str,
    ) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .execute(
                "UPDATE scheduled_tasks SET status = 'firing'
                 WHERE instance_id = ? AND task_id = ? AND status = 'pending'",
                params![instance_id, task_id],
            )
            .map(|updated| updated == 1)
            .map_err(StoreError::from)
    }

    pub(crate) fn finish_schedule(
        &self,
        instance_id: &str,
        task_id: &str,
        _status: &str,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "DELETE FROM scheduled_tasks
             WHERE instance_id = ? AND task_id = ? AND status = 'firing'",
            params![instance_id, task_id],
        )?;
        Ok(())
    }
}

fn apply_state_op(
    transaction: &rusqlite::Transaction<'_>,
    instance_id: &str,
    operation: &StateOp,
) -> Result<(), StoreError> {
    match operation {
        StateOp::Put {
            key,
            value,
            expected_revision,
        } => {
            validate_key(key)?;
            if let Some(expected_revision) = expected_revision {
                let updated = transaction.execute(
                    "UPDATE plugin_state SET value = ?, revision = revision + 1
                     WHERE instance_id = ? AND key = ? AND revision = ?",
                    params![value, instance_id, key, expected_revision],
                )?;
                if updated == 0 {
                    return Err(StoreError::CasConflict(key.clone()));
                }
            } else {
                transaction.execute(
                    "INSERT INTO plugin_state(instance_id, key, value, revision)
                     VALUES (?, ?, ?, 1)
                     ON CONFLICT(instance_id, key) DO UPDATE SET
                        value = excluded.value,
                        revision = plugin_state.revision + 1",
                    params![instance_id, key, value],
                )?;
            }
        }
        StateOp::Delete {
            key,
            expected_revision,
        } => {
            validate_key(key)?;
            let deleted = if let Some(expected_revision) = expected_revision {
                transaction.execute(
                    "DELETE FROM plugin_state
                     WHERE instance_id = ? AND key = ? AND revision = ?",
                    params![instance_id, key, expected_revision],
                )?
            } else {
                transaction.execute(
                    "DELETE FROM plugin_state WHERE instance_id = ? AND key = ?",
                    params![instance_id, key],
                )?
            };
            if expected_revision.is_some() && deleted == 0 {
                return Err(StoreError::CasConflict(key.clone()));
            }
        }
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty()
        || key.len() > 256
        || key
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(StoreError::InvalidKey(key.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use plugin_api::{PluginCommand, StateOp};
    use serde_json::json;
    use uuid::Uuid;

    use bot_core::MessageTarget;

    use super::{OutboxOrigin, PluginStore, ScheduledTask, StoreError};

    fn origin() -> OutboxOrigin {
        OutboxOrigin {
            source_event_id: "event-1".to_owned(),
            adapter_id: "mock".to_owned(),
            reply_target: Some(MessageTarget::Group {
                group_id: "group-1".to_owned(),
            }),
            source_message_id: Some("message-1".to_owned()),
        }
    }

    #[test]
    fn state_and_outbox_commit_atomically_with_cas() {
        let store = PluginStore::in_memory().unwrap();
        let command = PluginCommand {
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            idempotency_key: Some("event-1/reply".to_owned()),
            deadline_ms: None,
            payload: json!({"content":"pong"}),
        };
        store
            .commit(
                "instance",
                "invoke-1",
                &[StateOp::Put {
                    key: "counter".to_owned(),
                    value: b"1".to_vec(),
                    expected_revision: None,
                }],
                std::slice::from_ref(&command),
                1024,
                Some(&origin()),
            )
            .unwrap();
        assert_eq!(store.snapshot("instance").unwrap()["counter"].revision, 1);

        let error = store
            .commit(
                "instance",
                "invoke-2",
                &[StateOp::Put {
                    key: "counter".to_owned(),
                    value: b"2".to_vec(),
                    expected_revision: Some(99),
                }],
                &[],
                1024,
                None,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::CasConflict(_)));
        assert_eq!(store.snapshot("instance").unwrap()["counter"].value, b"1");
    }

    #[test]
    fn idempotency_key_prevents_duplicate_outbox_entry() {
        let store = PluginStore::in_memory().unwrap();
        let command = PluginCommand {
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            idempotency_key: Some("stable-key".to_owned()),
            deadline_ms: None,
            payload: json!({"content":"pong"}),
        };
        assert_eq!(
            store
                .commit(
                    "instance",
                    "invoke-1",
                    &[],
                    std::slice::from_ref(&command),
                    0,
                    Some(&origin()),
                )
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .commit("instance", "invoke-2", &[], &[command], 0, Some(&origin()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn schedules_survive_database_reopen() {
        let path = std::env::temp_dir().join(format!("bkm-scheduler-{}.db", Uuid::new_v4()));
        let task = ScheduledTask {
            instance_id: "instance".to_owned(),
            task_id: "task-1".to_owned(),
            adapter_id: "qq".to_owned(),
            run_at_ms: 1234,
            payload: json!({"hello":"world"}),
        };
        {
            let store = PluginStore::open(&path).unwrap();
            assert!(store.create_schedule(&task, 1024).unwrap());
        }
        {
            let store = PluginStore::open(&path).unwrap();
            assert_eq!(store.recover_schedules("instance").unwrap(), vec![task]);
        }
        std::fs::remove_file(path).unwrap();
    }
}
