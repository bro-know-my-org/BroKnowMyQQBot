//! SQLite-backed BPP private state, command ledger, and outbox.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use bot_core::MessageTarget;
use plugin_api::{PluginCommand, PluginEventEnvelope, PluginMetadata, StateOp, StateValue};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
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
    #[error("plugin installation changed concurrently; reload it and retry")]
    InstallationConflict,
    #[error("plugin delivery is no longer owned by this invocation")]
    StaleDelivery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxOrigin {
    pub source_event_id: String,
    pub adapter_id: String,
    pub reply_target: Option<MessageTarget>,
    pub source_message_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct CommitOptions<'a> {
    pub quota_bytes: u64,
    pub origin: Option<&'a OutboxOrigin>,
    delivery: Option<(&'a str, &'a str)>,
}

impl<'a> CommitOptions<'a> {
    pub const fn new(quota_bytes: u64) -> Self {
        Self {
            quota_bytes,
            origin: None,
            delivery: None,
        }
    }

    #[must_use]
    pub const fn with_origin(mut self, origin: &'a OutboxOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    #[must_use]
    pub(crate) const fn with_delivery(mut self, event_id: &'a str, delivery_id: &'a str) -> Self {
        self.delivery = Some((event_id, delivery_id));
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingCommand {
    pub instance_id: String,
    pub invocation_id: String,
    pub origin: OutboxOrigin,
    pub command: PluginCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingDelivery {
    pub instance_id: String,
    pub event_id: String,
    pub delivery_id: String,
    pub invocation_id: String,
    pub attempt: u32,
    pub next_attempt_ms: i64,
    pub envelope: PluginEventEnvelope,
    pub origin: OutboxOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeadLetter {
    pub instance_id: String,
    pub event_id: String,
    pub attempts: u32,
    pub last_error: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeliveryFailurePolicy {
    pub(crate) next_attempt_ms: i64,
    pub(crate) max_attempts: u32,
    pub(crate) circuit_threshold: u32,
    pub(crate) counts_toward_circuit: bool,
    pub(crate) now_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeliveryFailureResult {
    pub(crate) updated: bool,
    pub(crate) dead_letter: bool,
    pub(crate) circuit_open: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginInstallation {
    pub plugin_id: String,
    pub metadata: PluginMetadata,
    pub instance_id: String,
    pub version: String,
    pub package_path: String,
    pub package_sha256: String,
    pub source: String,
    pub trust_level: String,
    pub signature_status: String,
    pub requested_permissions: Vec<String>,
    pub granted_permissions: Vec<String>,
    pub config: BTreeMap<String, serde_json::Value>,
    #[serde(default = "legacy_admins_explicit")]
    pub admins_explicit: bool,
    pub enabled: bool,
    pub installed_at_ms: i64,
    pub updated_at_ms: i64,
}

const fn legacy_admins_explicit() -> bool {
    true
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

    #[allow(clippy::too_many_lines)]
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
            CREATE TABLE IF NOT EXISTS plugin_deliveries (
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                delivery_id TEXT NOT NULL,
                invocation_id TEXT NOT NULL,
                attempt INTEGER NOT NULL,
                envelope BLOB NOT NULL,
                origin BLOB NOT NULL,
                status TEXT NOT NULL,
                next_attempt_ms INTEGER NOT NULL,
                last_error TEXT,
                health_generation INTEGER NOT NULL DEFAULT 0,
                updated_at_ms INTEGER NOT NULL,
                PRIMARY KEY (instance_id, event_id)
            );
            CREATE INDEX IF NOT EXISTS plugin_deliveries_pending
                ON plugin_deliveries(instance_id, status, next_attempt_ms);
            CREATE TABLE IF NOT EXISTS plugin_health (
                instance_id TEXT PRIMARY KEY,
                consecutive_failures INTEGER NOT NULL,
                circuit_open INTEGER NOT NULL,
                failure_generation INTEGER NOT NULL DEFAULT 0,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS plugin_installations (
                instance_id TEXT PRIMARY KEY,
                plugin_id TEXT NOT NULL,
                metadata BLOB NOT NULL,
                version TEXT NOT NULL,
                package_path TEXT NOT NULL,
                package_sha256 TEXT NOT NULL,
                source TEXT NOT NULL,
                trust_level TEXT NOT NULL,
                signature_status TEXT NOT NULL,
                requested_permissions BLOB NOT NULL,
                granted_permissions BLOB NOT NULL,
                config BLOB NOT NULL,
                admins_explicit INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL,
                installed_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            ",
        )?;
        ensure_column(
            &connection,
            "plugin_deliveries",
            "health_generation",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            &connection,
            "plugin_health",
            "failure_generation",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&connection, "plugin_installations", "metadata", "BLOB")?;
        migrate_installation_metadata(&connection)?;
        migrate_installation_admins_explicit(&connection)?;
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
        options: CommitOptions<'_>,
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
                mark_delivery_committed(&transaction, instance_id, options.delivery)?;
                transaction.commit()?;
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
        if used > options.quota_bytes {
            return Err(StoreError::QuotaExceeded {
                used,
                quota: options.quota_bytes,
            });
        }

        let mut queued = Vec::new();
        for command in commands {
            let origin = options.origin.ok_or(StoreError::MissingCommandOrigin)?;
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
        mark_delivery_committed(&transaction, instance_id, options.delivery)?;
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

    pub(crate) fn begin_delivery(
        &self,
        instance_id: &str,
        envelope: &PluginEventEnvelope,
        origin: &OutboxOrigin,
        now_ms: i64,
    ) -> Result<Option<PendingDelivery>, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT status, attempt, next_attempt_ms FROM plugin_deliveries
                 WHERE instance_id = ? AND event_id = ?",
                params![instance_id, envelope.event_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        if existing.as_ref().is_some_and(|(status, _, _)| {
            matches!(
                status.as_str(),
                "running" | "committed" | "succeeded" | "dead_letter"
            )
        }) {
            return Ok(None);
        }
        if existing
            .as_ref()
            .is_some_and(|(status, _, next_attempt_ms)| {
                status == "pending" && *next_attempt_ms > now_ms
            })
        {
            return Ok(None);
        }
        let attempt = existing.map_or(1, |(_, attempt, _)| attempt.saturating_add(1));
        let health_generation = transaction
            .query_row(
                "SELECT failure_generation FROM plugin_health WHERE instance_id = ?",
                [instance_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()?
            .unwrap_or(0);
        transaction.execute(
            "INSERT INTO plugin_deliveries
                (instance_id, event_id, delivery_id, invocation_id, attempt, envelope, origin,
                 status, next_attempt_ms, last_error, health_generation, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'running', ?, NULL, ?, ?)
             ON CONFLICT(instance_id, event_id) DO UPDATE SET
                delivery_id = excluded.delivery_id,
                invocation_id = excluded.invocation_id,
                attempt = excluded.attempt,
                envelope = excluded.envelope,
                origin = excluded.origin,
                status = 'running',
                next_attempt_ms = excluded.next_attempt_ms,
                last_error = NULL,
                health_generation = excluded.health_generation,
                updated_at_ms = excluded.updated_at_ms",
            params![
                instance_id,
                envelope.event_id,
                envelope.delivery_id,
                envelope.invocation_id,
                attempt,
                serde_json::to_vec(envelope)?,
                serde_json::to_vec(origin)?,
                now_ms,
                health_generation,
                now_ms,
            ],
        )?;
        transaction.commit()?;
        Ok(Some(PendingDelivery {
            instance_id: instance_id.to_owned(),
            event_id: envelope.event_id.clone(),
            delivery_id: envelope.delivery_id.clone(),
            invocation_id: envelope.invocation_id.clone(),
            attempt,
            next_attempt_ms: now_ms,
            envelope: envelope.clone(),
            origin: origin.clone(),
        }))
    }

    pub(crate) fn mark_delivery_succeeded(
        &self,
        instance_id: &str,
        event_id: &str,
        delivery_id: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let health_generation = transaction
            .query_row(
                "SELECT health_generation FROM plugin_deliveries
                 WHERE instance_id = ? AND event_id = ? AND delivery_id = ?
                   AND status IN ('running', 'committed')",
                params![instance_id, event_id, delivery_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()?;
        let Some(health_generation) = health_generation else {
            return Ok(false);
        };
        let updated = transaction.execute(
            "UPDATE plugin_deliveries SET status = 'succeeded', next_attempt_ms = 0,
             last_error = NULL, updated_at_ms = ?
             WHERE instance_id = ? AND event_id = ? AND delivery_id = ?
               AND status IN ('running', 'committed')",
            params![now_ms, instance_id, event_id, delivery_id],
        )?;
        if updated == 0 {
            return Ok(false);
        }
        reset_health_if_generation(&transaction, instance_id, health_generation, now_ms)?;
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn mark_delivery_failed(
        &self,
        instance_id: &str,
        event_id: &str,
        delivery_id: &str,
        error: &str,
        policy: DeliveryFailurePolicy,
    ) -> Result<DeliveryFailureResult, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let attempt = transaction
            .query_row(
                "SELECT attempt FROM plugin_deliveries
             WHERE instance_id = ? AND event_id = ? AND delivery_id = ? AND status = 'running'",
                params![instance_id, event_id, delivery_id],
                |row| row.get::<_, u32>(0),
            )
            .optional()?;
        let Some(attempt) = attempt else {
            return Ok(DeliveryFailureResult {
                updated: false,
                dead_letter: false,
                circuit_open: false,
            });
        };
        let dead_letter = attempt >= policy.max_attempts;
        transaction.execute(
            "UPDATE plugin_deliveries SET status = ?, next_attempt_ms = ?, last_error = ?,
             updated_at_ms = ?
             WHERE instance_id = ? AND event_id = ? AND delivery_id = ? AND status = 'running'",
            params![
                if dead_letter {
                    "dead_letter"
                } else {
                    "pending"
                },
                policy.next_attempt_ms,
                error,
                policy.now_ms,
                instance_id,
                event_id,
                delivery_id,
            ],
        )?;
        let circuit_open = if policy.counts_toward_circuit {
            transaction.execute(
                "INSERT INTO plugin_health
                    (instance_id, consecutive_failures, circuit_open, failure_generation, updated_at_ms)
                 VALUES (?, 1, 0, 1, ?)
                 ON CONFLICT(instance_id) DO UPDATE SET
                    consecutive_failures = consecutive_failures + 1,
                    failure_generation = failure_generation + 1,
                    updated_at_ms = excluded.updated_at_ms",
                params![instance_id, policy.now_ms],
            )?;
            let failures = transaction.query_row(
                "SELECT consecutive_failures FROM plugin_health WHERE instance_id = ?",
                [instance_id],
                |row| row.get::<_, u32>(0),
            )?;
            let circuit_open = failures >= policy.circuit_threshold;
            if circuit_open {
                transaction.execute(
                    "UPDATE plugin_health SET circuit_open = 1 WHERE instance_id = ?",
                    [instance_id],
                )?;
            }
            circuit_open
        } else {
            transaction
                .query_row(
                    "SELECT circuit_open FROM plugin_health WHERE instance_id = ?",
                    [instance_id],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false)
        };
        transaction.commit()?;
        Ok(DeliveryFailureResult {
            updated: true,
            dead_letter,
            circuit_open,
        })
    }

    pub(crate) fn committed_command_results(
        &self,
        instance_id: &str,
    ) -> Result<
        Vec<(
            PendingCommand,
            plugin_api::ActionCompleted,
            PluginEventEnvelope,
        )>,
        StoreError,
    > {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT o.payload, l.result, d.envelope
             FROM command_outbox o
             JOIN command_ledger l USING (instance_id, invocation_id, command_id)
             JOIN plugin_deliveries d
               ON d.instance_id = l.instance_id AND d.invocation_id = l.invocation_id
             WHERE d.instance_id = ? AND d.status = 'committed' AND l.result IS NOT NULL
             ORDER BY o.rowid",
        )?;
        let rows = statement.query_map([instance_id], |row| {
            let pending = decode_json_column(row, 0)?;
            let completion = decode_json_column(row, 1)?;
            let envelope = decode_json_column(row, 2)?;
            Ok((pending, completion, envelope))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub(crate) fn finalize_committed_deliveries(
        &self,
        instance_id: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current_generation = transaction
            .query_row(
                "SELECT failure_generation FROM plugin_health WHERE instance_id = ?",
                [instance_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let can_reset_health = transaction
            .query_row(
                "SELECT 1 FROM plugin_deliveries
                 WHERE instance_id = ? AND status = 'committed' AND health_generation = ?
                 LIMIT 1",
                params![instance_id, current_generation],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let finalized = transaction.execute(
            "UPDATE plugin_deliveries SET status = 'succeeded', next_attempt_ms = 0,
             last_error = NULL, updated_at_ms = ?
             WHERE instance_id = ? AND status = 'committed'",
            params![now_ms, instance_id],
        )?;
        if finalized > 0 && can_reset_health {
            reset_health_if_generation(&transaction, instance_id, current_generation, now_ms)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn finalize_committed_delivery(
        &self,
        instance_id: &str,
        event_id: &str,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let health_generation = transaction
            .query_row(
                "SELECT health_generation FROM plugin_deliveries
                 WHERE instance_id = ? AND event_id = ? AND status = 'committed'",
                params![instance_id, event_id],
                |row| row.get::<_, u64>(0),
            )
            .optional()?;
        let updated = transaction.execute(
            "UPDATE plugin_deliveries SET status = 'succeeded', next_attempt_ms = 0,
             last_error = NULL, updated_at_ms = ?
             WHERE instance_id = ? AND event_id = ? AND status = 'committed'",
            params![now_ms, instance_id, event_id],
        )?;
        if updated == 1 {
            if let Some(health_generation) = health_generation {
                reset_health_if_generation(&transaction, instance_id, health_generation, now_ms)?;
            }
        }
        transaction.commit()?;
        Ok(updated == 1)
    }

    pub(crate) fn enqueue_delivery(
        &self,
        instance_id: &str,
        envelope: &PluginEventEnvelope,
        origin: &OutboxOrigin,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "INSERT OR IGNORE INTO plugin_deliveries
                (instance_id, event_id, delivery_id, invocation_id, attempt, envelope, origin,
                 status, next_attempt_ms, last_error, updated_at_ms)
             VALUES (?, ?, ?, ?, 0, ?, ?, 'pending', ?, NULL, ?)",
            params![
                instance_id,
                envelope.event_id,
                envelope.delivery_id,
                envelope.invocation_id,
                serde_json::to_vec(envelope)?,
                serde_json::to_vec(origin)?,
                now_ms,
                now_ms,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn requeue_running_deliveries(
        &self,
        instance_id: &str,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "UPDATE plugin_deliveries SET status = 'pending', next_attempt_ms = ?, updated_at_ms = ?
             WHERE instance_id = ? AND status = 'running'",
            params![now_ms, now_ms, instance_id],
        )?;
        Ok(())
    }

    pub(crate) fn pending_deliveries(
        &self,
        instance_id: &str,
        now_ms: i64,
    ) -> Result<Vec<PendingDelivery>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT event_id, delivery_id, invocation_id, attempt, next_attempt_ms, envelope, origin
             FROM plugin_deliveries
             WHERE instance_id = ? AND status IN ('pending', 'running') AND next_attempt_ms <= ?
             ORDER BY updated_at_ms, event_id",
        )?;
        let rows = statement.query_map(params![instance_id, now_ms], |row| {
            let envelope = decode_json_column(row, 5)?;
            let origin = decode_json_column(row, 6)?;
            Ok(PendingDelivery {
                instance_id: instance_id.to_owned(),
                event_id: row.get(0)?,
                delivery_id: row.get(1)?,
                invocation_id: row.get(2)?,
                attempt: row.get(3)?,
                next_attempt_ms: row.get(4)?,
                envelope,
                origin,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn dead_letters(&self, instance_id: Option<&str>) -> Result<Vec<DeadLetter>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut output = Vec::new();
        if let Some(instance_id) = instance_id {
            let mut statement = connection.prepare(
                "SELECT instance_id, event_id, attempt, COALESCE(last_error, ''), updated_at_ms
                 FROM plugin_deliveries WHERE status = 'dead_letter' AND instance_id = ?
                 ORDER BY updated_at_ms DESC",
            )?;
            let rows = statement.query_map([instance_id], dead_letter_row)?;
            output.extend(rows.collect::<Result<Vec<_>, _>>()?);
        } else {
            let mut statement = connection.prepare(
                "SELECT instance_id, event_id, attempt, COALESCE(last_error, ''), updated_at_ms
                 FROM plugin_deliveries WHERE status = 'dead_letter'
                 ORDER BY updated_at_ms DESC",
            )?;
            let rows = statement.query_map([], dead_letter_row)?;
            output.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        Ok(output)
    }

    pub fn recover_dead_letters(&self, instance_id: &str, now_ms: i64) -> Result<u64, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE plugin_deliveries SET status = 'pending', attempt = 0,
             next_attempt_ms = ?, last_error = NULL, updated_at_ms = ?
             WHERE instance_id = ? AND status = 'dead_letter'",
            params![now_ms, now_ms, instance_id],
        )?;
        transaction.execute(
            "INSERT INTO plugin_health
                (instance_id, consecutive_failures, circuit_open, failure_generation, updated_at_ms)
             VALUES (?, 0, 0, 0, ?)
             ON CONFLICT(instance_id) DO UPDATE SET
                consecutive_failures = 0, circuit_open = 0,
                failure_generation = failure_generation + 1,
                updated_at_ms = excluded.updated_at_ms",
            params![instance_id, now_ms],
        )?;
        transaction.commit()?;
        Ok(u64::try_from(updated).unwrap_or(u64::MAX))
    }

    pub(crate) fn circuit_open(&self, instance_id: &str) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                "SELECT circuit_open FROM plugin_health WHERE instance_id = ?",
                [instance_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .map(|value| value.unwrap_or(false))
            .map_err(StoreError::from)
    }

    pub fn upsert_installation(&self, installation: &PluginInstallation) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection.execute(
            "INSERT INTO plugin_installations
                (instance_id, plugin_id, metadata, version, package_path, package_sha256, source,
                 trust_level, signature_status, requested_permissions, granted_permissions,
                 config, admins_explicit, enabled, installed_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(instance_id) DO UPDATE SET
                plugin_id = excluded.plugin_id,
                metadata = excluded.metadata,
                version = excluded.version,
                package_path = excluded.package_path,
                package_sha256 = excluded.package_sha256,
                source = excluded.source,
                trust_level = excluded.trust_level,
                signature_status = excluded.signature_status,
                requested_permissions = excluded.requested_permissions,
                granted_permissions = excluded.granted_permissions,
                config = excluded.config,
                admins_explicit = excluded.admins_explicit,
                enabled = excluded.enabled,
                updated_at_ms = excluded.updated_at_ms",
            params![
                installation.instance_id,
                installation.plugin_id,
                serde_json::to_vec(&installation.metadata)?,
                installation.version,
                installation.package_path,
                installation.package_sha256,
                installation.source,
                installation.trust_level,
                installation.signature_status,
                serde_json::to_vec(&installation.requested_permissions)?,
                serde_json::to_vec(&installation.granted_permissions)?,
                serde_json::to_vec(&installation.config)?,
                installation.admins_explicit,
                installation.enabled,
                installation.installed_at_ms,
                installation.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn write_installation(
        &self,
        installation: &PluginInstallation,
        expected_updated_at_ms: Option<i64>,
    ) -> Result<(), StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let metadata = serde_json::to_vec(&installation.metadata)?;
        let requested = serde_json::to_vec(&installation.requested_permissions)?;
        let granted = serde_json::to_vec(&installation.granted_permissions)?;
        let config = serde_json::to_vec(&installation.config)?;
        let updated = if let Some(expected_updated_at_ms) = expected_updated_at_ms {
            connection.execute(
                "UPDATE plugin_installations SET
                    plugin_id = ?, metadata = ?, version = ?, package_path = ?,
                    package_sha256 = ?, source = ?, trust_level = ?, signature_status = ?,
                    requested_permissions = ?, granted_permissions = ?, config = ?, admins_explicit = ?, enabled = ?,
                    installed_at_ms = ?, updated_at_ms = ?
                 WHERE instance_id = ? AND updated_at_ms = ? AND ? > updated_at_ms",
                params![
                    installation.plugin_id,
                    metadata,
                    installation.version,
                    installation.package_path,
                    installation.package_sha256,
                    installation.source,
                    installation.trust_level,
                    installation.signature_status,
                    requested,
                    granted,
                    config,
                    installation.admins_explicit,
                    installation.enabled,
                    installation.installed_at_ms,
                    installation.updated_at_ms,
                    installation.instance_id,
                    expected_updated_at_ms,
                    installation.updated_at_ms,
                ],
            )?
        } else {
            connection.execute(
                "INSERT OR IGNORE INTO plugin_installations
                    (instance_id, plugin_id, metadata, version, package_path, package_sha256,
                     source, trust_level, signature_status, requested_permissions,
                     granted_permissions, config, admins_explicit, enabled, installed_at_ms, updated_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    installation.instance_id,
                    installation.plugin_id,
                    metadata,
                    installation.version,
                    installation.package_path,
                    installation.package_sha256,
                    installation.source,
                    installation.trust_level,
                    installation.signature_status,
                    requested,
                    granted,
                    config,
                    installation.admins_explicit,
                    installation.enabled,
                    installation.installed_at_ms,
                    installation.updated_at_ms,
                ],
            )?
        };
        if updated != 1 {
            return Err(StoreError::InstallationConflict);
        }
        Ok(())
    }

    pub fn installations(&self) -> Result<Vec<PluginInstallation>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT plugin_id, metadata, instance_id, version, package_path, package_sha256, source,
             trust_level, signature_status, requested_permissions, granted_permissions, config,
             admins_explicit, enabled, installed_at_ms, updated_at_ms FROM plugin_installations
             ORDER BY plugin_id, instance_id",
        )?;
        let rows = statement.query_map([], installation_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn enabled_installations(&self) -> Result<Vec<PluginInstallation>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT plugin_id, metadata, instance_id, version, package_path, package_sha256, source,
             trust_level, signature_status, requested_permissions, granted_permissions, config,
             admins_explicit, enabled, installed_at_ms, updated_at_ms FROM plugin_installations
             WHERE enabled = 1
             ORDER BY plugin_id, instance_id",
        )?;
        let rows = statement.query_map([], installation_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn installation(
        &self,
        instance_id: &str,
    ) -> Result<Option<PluginInstallation>, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .query_row(
                "SELECT plugin_id, metadata, instance_id, version, package_path, package_sha256, source,
                 trust_level, signature_status, requested_permissions, granted_permissions, config,
                 admins_explicit, enabled, installed_at_ms, updated_at_ms FROM plugin_installations
                 WHERE instance_id = ?",
                [instance_id],
                installation_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn set_installation_enabled(
        &self,
        instance_id: &str,
        enabled: bool,
        now_ms: i64,
    ) -> Result<bool, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let current_updated_at_ms = transaction
            .query_row(
                "SELECT updated_at_ms FROM plugin_installations WHERE instance_id = ?",
                [instance_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(current_updated_at_ms) = current_updated_at_ms else {
            return Ok(false);
        };
        let next_updated_at_ms = now_ms.max(current_updated_at_ms.saturating_add(1));
        let updated = transaction.execute(
            "UPDATE plugin_installations SET enabled = ?, updated_at_ms = ?
             WHERE instance_id = ? AND updated_at_ms = ?",
            params![
                enabled,
                next_updated_at_ms,
                instance_id,
                current_updated_at_ms
            ],
        )?;
        transaction.commit()?;
        Ok(updated == 1)
    }

    pub fn remove_installation(&self, instance_id: &str) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .execute(
                "DELETE FROM plugin_installations WHERE instance_id = ?",
                [instance_id],
            )
            .map(|updated| updated == 1)
            .map_err(StoreError::from)
    }

    pub fn remove_installation_if_updated(
        &self,
        instance_id: &str,
        expected_updated_at_ms: i64,
    ) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        connection
            .execute(
                "DELETE FROM plugin_installations WHERE instance_id = ? AND updated_at_ms = ?",
                params![instance_id, expected_updated_at_ms],
            )
            .map(|updated| updated == 1)
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

fn reset_health_if_generation(
    transaction: &Transaction<'_>,
    instance_id: &str,
    health_generation: u64,
    now_ms: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO plugin_health
            (instance_id, consecutive_failures, circuit_open, failure_generation, updated_at_ms)
         VALUES (?, 0, 0, ?, ?)
         ON CONFLICT(instance_id) DO UPDATE SET
            consecutive_failures = 0, circuit_open = 0, updated_at_ms = excluded.updated_at_ms
         WHERE plugin_health.failure_generation = excluded.failure_generation",
        params![instance_id, health_generation, now_ms],
    )?;
    Ok(())
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|existing| existing == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

fn migrate_installation_metadata(connection: &Connection) -> Result<(), StoreError> {
    let columns = table_columns(connection, "plugin_installations")?;
    let has_legacy_name = columns.iter().any(|column| column == "name");
    let has_legacy_description = columns.iter().any(|column| column == "description");
    let select = match (has_legacy_name, has_legacy_description) {
        (true, true) => {
            "SELECT instance_id, plugin_id, name, description FROM plugin_installations WHERE metadata IS NULL"
        }
        (true, false) => {
            "SELECT instance_id, plugin_id, name, '' FROM plugin_installations WHERE metadata IS NULL"
        }
        (false, true) => {
            "SELECT instance_id, plugin_id, plugin_id, description FROM plugin_installations WHERE metadata IS NULL"
        }
        (false, false) => {
            "SELECT instance_id, plugin_id, plugin_id, '' FROM plugin_installations WHERE metadata IS NULL"
        }
    };
    let records = {
        let mut statement = connection.prepare(select)?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (instance_id, plugin_id, name, description) in records {
        let name = if name.is_empty() { plugin_id } else { name };
        let metadata = PluginMetadata::single_locale("en", name, description);
        connection.execute(
            "UPDATE plugin_installations SET metadata = ? WHERE instance_id = ?",
            params![serde_json::to_vec(&metadata)?, instance_id],
        )?;
    }
    if has_legacy_name || has_legacy_description {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE plugin_installations RENAME TO plugin_installations_legacy;
             CREATE TABLE plugin_installations (
                 instance_id TEXT PRIMARY KEY,
                 plugin_id TEXT NOT NULL,
                 metadata BLOB NOT NULL,
                 version TEXT NOT NULL,
                 package_path TEXT NOT NULL,
                 package_sha256 TEXT NOT NULL,
                 source TEXT NOT NULL,
                 trust_level TEXT NOT NULL,
                 signature_status TEXT NOT NULL,
                 requested_permissions BLOB NOT NULL,
                 granted_permissions BLOB NOT NULL,
                 config BLOB NOT NULL,
                 enabled INTEGER NOT NULL,
                 installed_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO plugin_installations
                 (instance_id, plugin_id, metadata, version, package_path, package_sha256,
                  source, trust_level, signature_status, requested_permissions,
                  granted_permissions, config, enabled, installed_at_ms, updated_at_ms)
             SELECT instance_id, plugin_id, metadata, version, package_path, package_sha256,
                    source, trust_level, signature_status, requested_permissions,
                    granted_permissions, config, enabled, installed_at_ms, updated_at_ms
             FROM plugin_installations_legacy;
             DROP TABLE plugin_installations_legacy;
             COMMIT;",
        )?;
    }
    Ok(())
}

fn migrate_installation_admins_explicit(connection: &Connection) -> Result<(), StoreError> {
    let columns = table_columns(connection, "plugin_installations")?;
    if columns.iter().any(|column| column == "admins_explicit") {
        return Ok(());
    }

    let transaction = connection.unchecked_transaction()?;
    transaction.execute_batch(
        "ALTER TABLE plugin_installations
         ADD COLUMN admins_explicit INTEGER NOT NULL DEFAULT 0;",
    )?;
    let records = {
        let mut statement =
            transaction.prepare("SELECT instance_id, config FROM plugin_installations")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (instance_id, config) in records {
        // Legacy rows do not record whether `admins` came from global owners or an explicit
        // plugin override. Preserve every stored value as explicit rather than risk silently
        // replacing an intentional authorization list; rows without `admins` remain inherited.
        let explicit = serde_json::from_slice::<BTreeMap<String, serde_json::Value>>(&config)
            .map_or(true, |config| config.contains_key("admins"));
        if explicit {
            transaction.execute(
                "UPDATE plugin_installations SET admins_explicit = 1 WHERE instance_id = ?",
                [instance_id],
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>, StoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn decode_json_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<T, rusqlite::Error> {
    let payload = row.get::<_, Vec<u8>>(index)?;
    serde_json::from_slice(&payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(error),
        )
    })
}

fn dead_letter_row(row: &rusqlite::Row<'_>) -> Result<DeadLetter, rusqlite::Error> {
    Ok(DeadLetter {
        instance_id: row.get(0)?,
        event_id: row.get(1)?,
        attempts: row.get(2)?,
        last_error: row.get(3)?,
        updated_at_ms: row.get(4)?,
    })
}

fn installation_row(row: &rusqlite::Row<'_>) -> Result<PluginInstallation, rusqlite::Error> {
    Ok(PluginInstallation {
        plugin_id: row.get(0)?,
        metadata: decode_json_column(row, 1)?,
        instance_id: row.get(2)?,
        version: row.get(3)?,
        package_path: row.get(4)?,
        package_sha256: row.get(5)?,
        source: row.get(6)?,
        trust_level: row.get(7)?,
        signature_status: row.get(8)?,
        requested_permissions: decode_json_column(row, 9)?,
        granted_permissions: decode_json_column(row, 10)?,
        config: decode_json_column(row, 11)?,
        admins_explicit: row.get(12)?,
        enabled: row.get(13)?,
        installed_at_ms: row.get(14)?,
        updated_at_ms: row.get(15)?,
    })
}

fn mark_delivery_committed(
    transaction: &rusqlite::Transaction<'_>,
    instance_id: &str,
    delivery: Option<(&str, &str)>,
) -> Result<(), StoreError> {
    let Some((event_id, delivery_id)) = delivery else {
        return Ok(());
    };
    let updated = transaction.execute(
        "UPDATE plugin_deliveries SET status = 'committed'
         WHERE instance_id = ? AND event_id = ? AND delivery_id = ? AND status = 'running'",
        params![instance_id, event_id, delivery_id],
    )?;
    if updated != 1 {
        return Err(StoreError::StaleDelivery);
    }
    Ok(())
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
    use std::collections::BTreeMap;

    use plugin_api::{
        ActionCompleted, ActionStatus, PluginCommand, PluginEventEnvelope, PluginMetadata, StateOp,
    };
    use rusqlite::params;
    use serde_json::json;
    use uuid::Uuid;

    use bot_core::MessageTarget;

    use super::{
        CommitOptions, DeliveryFailurePolicy, OutboxOrigin, PluginInstallation, PluginStore,
        ScheduledTask, StoreError,
    };

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

    fn envelope(event_id: &str, delivery_id: &str, invocation_id: &str) -> PluginEventEnvelope {
        PluginEventEnvelope {
            protocol_version: "1.0.0".to_owned(),
            event_id: event_id.to_owned(),
            delivery_id: delivery_id.to_owned(),
            invocation_id: invocation_id.to_owned(),
            occurred_at_ms: None,
            received_at_ms: 1,
            adapter_id: "mock".to_owned(),
            event_type: "message.created".to_owned(),
            trace_id: None,
            payload: json!({"text":"/ping"}),
            extensions: Vec::new(),
        }
    }

    #[test]
    fn delivery_retries_dead_letter_and_recovery_are_persistent() {
        let store = PluginStore::in_memory().unwrap();
        let first = store
            .begin_delivery(
                "instance",
                &envelope("event", "delivery-1", "invoke-1"),
                &origin(),
                1,
            )
            .unwrap()
            .unwrap();
        assert_eq!(first.attempt, 1);
        let failure = store
            .mark_delivery_failed(
                "instance",
                "event",
                "delivery-1",
                "temporary",
                DeliveryFailurePolicy {
                    next_attempt_ms: 2,
                    max_attempts: 3,
                    circuit_threshold: 3,
                    counts_toward_circuit: true,
                    now_ms: 1,
                },
            )
            .unwrap();
        assert!(failure.updated);
        assert!(!failure.dead_letter);
        assert!(!failure.circuit_open);
        assert!(store.pending_deliveries("instance", 1).unwrap().is_empty());
        assert_eq!(
            store.pending_deliveries("instance", 2).unwrap()[0].next_attempt_ms,
            2
        );
        let second = store
            .begin_delivery(
                "instance",
                &envelope("event", "delivery-2", "invoke-2"),
                &origin(),
                2,
            )
            .unwrap()
            .unwrap();
        assert_eq!(second.attempt, 2);
        store
            .mark_delivery_failed(
                "instance",
                "event",
                "delivery-2",
                "temporary",
                DeliveryFailurePolicy {
                    next_attempt_ms: 3,
                    max_attempts: 3,
                    circuit_threshold: 3,
                    counts_toward_circuit: true,
                    now_ms: 2,
                },
            )
            .unwrap();
        let third = store
            .begin_delivery(
                "instance",
                &envelope("event", "delivery-3", "invoke-3"),
                &origin(),
                3,
            )
            .unwrap()
            .unwrap();
        assert_eq!(third.attempt, 3);
        let failure = store
            .mark_delivery_failed(
                "instance",
                "event",
                "delivery-3",
                "exhausted",
                DeliveryFailurePolicy {
                    next_attempt_ms: 4,
                    max_attempts: 3,
                    circuit_threshold: 3,
                    counts_toward_circuit: true,
                    now_ms: 3,
                },
            )
            .unwrap();
        assert!(failure.updated);
        assert!(failure.dead_letter);
        assert!(failure.circuit_open);
        assert!(store.circuit_open("instance").unwrap());
        assert_eq!(store.dead_letters(Some("instance")).unwrap().len(), 1);
        assert_eq!(store.recover_dead_letters("instance", 5).unwrap(), 1);
        assert!(!store.circuit_open("instance").unwrap());
        assert_eq!(store.pending_deliveries("instance", 5).unwrap().len(), 1);
    }

    #[test]
    fn pending_delivery_cannot_bypass_persisted_backoff() {
        let store = PluginStore::in_memory().unwrap();
        store
            .begin_delivery(
                "instance",
                &envelope("event", "delivery-1", "invoke-1"),
                &origin(),
                1,
            )
            .unwrap()
            .unwrap();
        store
            .mark_delivery_failed(
                "instance",
                "event",
                "delivery-1",
                "temporary",
                DeliveryFailurePolicy {
                    next_attempt_ms: 10,
                    max_attempts: 3,
                    circuit_threshold: 3,
                    counts_toward_circuit: true,
                    now_ms: 1,
                },
            )
            .unwrap();

        assert!(
            store
                .begin_delivery(
                    "instance",
                    &envelope("event", "delivery-early", "invoke-early"),
                    &origin(),
                    9,
                )
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .begin_delivery(
                    "instance",
                    &envelope("event", "delivery-due", "invoke-due"),
                    &origin(),
                    10,
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn concurrent_success_does_not_clear_newer_failure_generation() {
        let store = PluginStore::in_memory().unwrap();
        for (event, delivery) in [("failed-1", "delivery-1"), ("success", "delivery-2")] {
            store
                .begin_delivery(
                    "instance",
                    &envelope(event, delivery, &format!("invoke-{event}")),
                    &origin(),
                    1,
                )
                .unwrap()
                .unwrap();
        }
        store
            .mark_delivery_failed(
                "instance",
                "failed-1",
                "delivery-1",
                "failure",
                DeliveryFailurePolicy {
                    next_attempt_ms: 2,
                    max_attempts: 1,
                    circuit_threshold: 3,
                    counts_toward_circuit: true,
                    now_ms: 2,
                },
            )
            .unwrap();
        assert!(
            store
                .mark_delivery_succeeded("instance", "success", "delivery-2", 3)
                .unwrap()
        );

        for index in 2..=3 {
            let event = format!("failed-{index}");
            let delivery = format!("delivery-{}", index + 1);
            store
                .begin_delivery(
                    "instance",
                    &envelope(&event, &delivery, &format!("invoke-{index}")),
                    &origin(),
                    i64::from(index),
                )
                .unwrap()
                .unwrap();
            let failure = store
                .mark_delivery_failed(
                    "instance",
                    &event,
                    &delivery,
                    "failure",
                    DeliveryFailurePolicy {
                        next_attempt_ms: i64::from(index + 1),
                        max_attempts: 1,
                        circuit_threshold: 3,
                        counts_toward_circuit: true,
                        now_ms: i64::from(index + 1),
                    },
                )
                .unwrap();
            assert_eq!(failure.circuit_open, index == 3);
        }
    }

    #[test]
    fn running_delivery_is_single_owner_and_finalization_is_fenced() {
        let store = PluginStore::in_memory().unwrap();
        store
            .begin_delivery(
                "instance",
                &envelope("event", "delivery-1", "invoke-1"),
                &origin(),
                1,
            )
            .unwrap()
            .unwrap();
        assert!(
            store
                .begin_delivery(
                    "instance",
                    &envelope("event", "delivery-2", "invoke-2"),
                    &origin(),
                    2,
                )
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .mark_delivery_succeeded("instance", "event", "delivery-2", 2)
                .unwrap()
        );
        assert!(
            store
                .mark_delivery_succeeded("instance", "event", "delivery-1", 2)
                .unwrap()
        );
    }

    #[test]
    fn output_commit_durably_fences_parent_delivery_and_recovers_completion() {
        let store = PluginStore::in_memory().unwrap();
        store
            .begin_delivery(
                "instance",
                &envelope("event", "delivery-1", "invoke-1"),
                &origin(),
                1,
            )
            .unwrap()
            .unwrap();
        let command = PluginCommand {
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            idempotency_key: None,
            deadline_ms: None,
            payload: json!({"content":"pong"}),
        };
        store
            .commit(
                "instance",
                "invoke-1",
                &[],
                std::slice::from_ref(&command),
                CommitOptions::new(0)
                    .with_origin(&origin())
                    .with_delivery("event", "delivery-1"),
            )
            .unwrap();
        assert!(
            store
                .pending_deliveries("instance", i64::MAX)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.pending_commands("instance").unwrap().len(), 1);

        let completion = ActionCompleted {
            source_event_id: "event".to_owned(),
            source_invocation_id: "invoke-1".to_owned(),
            command_id: "reply".to_owned(),
            kind: "message.reply".to_owned(),
            status: ActionStatus::Unknown,
            retryable: false,
            result: None,
            error_code: Some("host_restarted".to_owned()),
            error_message: Some("result unknown".to_owned()),
        };
        store
            .mark_command(
                "instance",
                "invoke-1",
                "reply",
                "unknown",
                &serde_json::to_value(&completion).unwrap(),
            )
            .unwrap();
        let recovered = store.committed_command_results("instance").unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].1, completion);
        store.finalize_committed_deliveries("instance", 2).unwrap();
        assert!(
            store
                .begin_delivery(
                    "instance",
                    &envelope("event", "delivery-2", "invoke-2"),
                    &origin(),
                    3,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn installation_records_round_trip_and_toggle() {
        let store = PluginStore::in_memory().unwrap();
        let installation = PluginInstallation {
            plugin_id: "dev.bkm.example".to_owned(),
            metadata: PluginMetadata::single_locale("en", "Example", "Example plugin"),
            instance_id: "dev.bkm.example/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "/tmp/example.bkm-plugin".to_owned(),
            package_sha256: "abc".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: vec!["message.reply".to_owned()],
            granted_permissions: vec!["message.reply".to_owned()],
            config: BTreeMap::from([("greeting".to_owned(), json!("hello"))]),
            admins_explicit: false,
            enabled: true,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        let mut legacy_json = serde_json::to_value(&installation).unwrap();
        legacy_json
            .as_object_mut()
            .unwrap()
            .remove("admins_explicit");
        let legacy_installation: PluginInstallation = serde_json::from_value(legacy_json).unwrap();
        assert!(legacy_installation.admins_explicit);
        store.upsert_installation(&installation).unwrap();
        assert_eq!(
            store.installation(&installation.instance_id).unwrap(),
            Some(installation.clone())
        );
        assert!(
            store
                .set_installation_enabled(&installation.instance_id, false, 2)
                .unwrap()
        );
        assert!(
            !store
                .installation(&installation.instance_id)
                .unwrap()
                .unwrap()
                .enabled
        );
        assert!(
            store
                .remove_installation(&installation.instance_id)
                .unwrap()
        );
        assert!(store.installations().unwrap().is_empty());
    }

    #[test]
    fn enabled_installations_skip_malformed_disabled_rows() {
        let store = PluginStore::in_memory().unwrap();
        let installation = PluginInstallation {
            plugin_id: "dev.bkm.disabled".to_owned(),
            metadata: PluginMetadata::single_locale("en", "Disabled", ""),
            instance_id: "dev.bkm.disabled/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "/tmp/disabled.bkm-plugin".to_owned(),
            package_sha256: "abc".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: Vec::new(),
            granted_permissions: Vec::new(),
            config: BTreeMap::new(),
            admins_explicit: false,
            enabled: false,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        store.upsert_installation(&installation).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE plugin_installations SET config = x'ff' WHERE instance_id = ?",
                [&installation.instance_id],
            )
            .unwrap();

        assert!(store.enabled_installations().unwrap().is_empty());
        assert!(store.installations().is_err());
    }

    #[test]
    fn conditional_installation_write_rejects_stale_updates() {
        let store = PluginStore::in_memory().unwrap();
        let mut installation = PluginInstallation {
            plugin_id: "dev.bkm.concurrent".to_owned(),
            metadata: PluginMetadata::single_locale("en", "Concurrent", ""),
            instance_id: "dev.bkm.concurrent/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "/tmp/concurrent.bkm-plugin".to_owned(),
            package_sha256: "one".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: Vec::new(),
            granted_permissions: Vec::new(),
            config: BTreeMap::new(),
            admins_explicit: false,
            enabled: true,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        store.write_installation(&installation, None).unwrap();
        installation.package_sha256 = "two".to_owned();
        installation.updated_at_ms = 2;
        store.write_installation(&installation, Some(1)).unwrap();
        installation.package_sha256 = "stale".to_owned();
        installation.updated_at_ms = 3;
        assert!(matches!(
            store.write_installation(&installation, Some(1)),
            Err(StoreError::InstallationConflict)
        ));
        assert_eq!(
            store
                .installation("dev.bkm.concurrent/default")
                .unwrap()
                .unwrap()
                .package_sha256,
            "two"
        );
    }

    #[test]
    fn legacy_installation_table_migrates_display_metadata() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE plugin_installations (
                    instance_id TEXT PRIMARY KEY,
                    plugin_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    description TEXT NOT NULL,
                    version TEXT NOT NULL,
                    package_path TEXT NOT NULL,
                    package_sha256 TEXT NOT NULL,
                    source TEXT NOT NULL,
                    trust_level TEXT NOT NULL,
                    signature_status TEXT NOT NULL,
                    requested_permissions BLOB NOT NULL,
                    granted_permissions BLOB NOT NULL,
                    config BLOB NOT NULL,
                    enabled INTEGER NOT NULL,
                    installed_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );",
            )
            .unwrap();
        let insert = "INSERT INTO plugin_installations
                 (instance_id, plugin_id, name, description, version, package_path,
                  package_sha256, source, trust_level, signature_status,
                  requested_permissions, granted_permissions, config, enabled,
                  installed_at_ms, updated_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let insert_legacy = |instance_id: &str, plugin_id: &str, config: &[u8]| {
            connection.execute(
                insert,
                params![
                    instance_id,
                    plugin_id,
                    "Migrated Plugin",
                    "display metadata migration",
                    "1.0.0",
                    "/tmp/migrated.bkm-plugin",
                    "abc",
                    "local",
                    "local-wasm",
                    "unsigned",
                    b"[]",
                    b"[]",
                    config,
                    true,
                    1,
                    1,
                ],
            )
        };
        insert_legacy(
            "dev.bkm.migrated/default",
            "dev.bkm.migrated",
            br#"{"admins":["legacy-admin"]}"#,
        )
        .unwrap();
        insert_legacy("dev.bkm.inherited/default", "dev.bkm.inherited", b"{}").unwrap();
        insert_legacy(
            "dev.bkm.malformed/default",
            "dev.bkm.malformed",
            b"not-json",
        )
        .unwrap();
        let store = PluginStore::from_connection(connection).unwrap();
        let installation = store
            .installation("dev.bkm.migrated/default")
            .unwrap()
            .unwrap();
        let metadata = installation.metadata.resolve("en").unwrap();
        assert_eq!(metadata.name, "Migrated Plugin");
        assert_eq!(metadata.description, "display metadata migration");
        assert!(installation.admins_explicit);
        assert!(
            !store
                .installation("dev.bkm.inherited/default")
                .unwrap()
                .unwrap()
                .admins_explicit
        );
        let malformed_explicit = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT admins_explicit FROM plugin_installations WHERE instance_id = ?",
                ["dev.bkm.malformed/default"],
                |row| row.get::<_, bool>(0),
            )
            .unwrap();
        assert!(malformed_explicit);
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
                CommitOptions::new(1024).with_origin(&origin()),
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
                CommitOptions::new(1024),
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
                    CommitOptions::new(0).with_origin(&origin()),
                )
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .commit(
                    "instance",
                    "invoke-2",
                    &[],
                    &[command],
                    CommitOptions::new(0).with_origin(&origin()),
                )
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
