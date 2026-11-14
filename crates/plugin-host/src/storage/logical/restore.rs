//! Offline logical transaction. The application owns drain, package/config locks
//! and a durable resource journal before invoking this boundary.
use super::{LogicalSnapshot, read_snapshot};
use crate::{PluginInstallation, PluginStore, StoreError};
use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet};

impl PluginStore {
    /// Read the recovery marker without creating or initializing a missing/unknown DB.
    pub fn logical_restore_committed_at(
        path: &std::path::Path,
        operation_id: &str,
    ) -> Result<bool, StoreError> {
        let connection = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let id: Option<String> = connection
            .query_row(
                "SELECT operation_id FROM logical_restore_commit WHERE slot = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id.as_deref() == Some(operation_id))
    }
    pub fn logical_restore_committed(&self, operation_id: &str) -> Result<bool, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let id: Option<String> = connection
            .query_row(
                "SELECT operation_id FROM logical_restore_commit WHERE slot = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id.as_deref() == Some(operation_id))
    }

    /// Only call after every Runtime/guest handle has dropped. This never replaces
    /// the database file. `prepare` must durably retain old affected data and config;
    /// `apply_config` atomically replaces config, with that journal recovering crashes.
    /// The marker and all database changes commit together, so recovery can distinguish
    /// an old database from a fully committed new database without guessing.
    pub fn restore_logical_offline(
        &self,
        operation_id: &str,
        imported: &LogicalSnapshot,
        expected_installations: &[PluginInstallation],
        affected_instances: &[String],
        prepare: impl FnOnce(&LogicalSnapshot) -> Result<(), StoreError>,
        apply_config: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let affected = validate_scope(operation_id, imported, affected_instances)?;
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<String> = transaction
            .query_row(
                "SELECT operation_id FROM logical_restore_commit WHERE slot = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if previous.as_deref() == Some(operation_id) {
            return Err(StoreError::InstallationConflict);
        }
        let current = read_snapshot(&transaction, || {})?;
        if current.installations != expected_installations {
            return Err(StoreError::InstallationConflict);
        }
        let timestamp = chrono_timestamp();
        validate_merged(&current, imported, &affected, timestamp)?;
        let old: BTreeMap<_, _> = current
            .state
            .iter()
            .map(|e| ((e.instance_id.as_str(), e.key.as_str()), e.revision))
            .collect();
        let versions: BTreeMap<_, _> = current
            .installations
            .iter()
            .map(|i| (i.instance_id.as_str(), i.updated_at_ms))
            .collect();
        // Check overflow before journaling or mutating either resource.
        for entry in &imported.state {
            let next = entry
                .revision
                .max(
                    *old.get(&(entry.instance_id.as_str(), entry.key.as_str()))
                        .unwrap_or(&0),
                )
                .checked_add(1)
                .ok_or(StoreError::InvalidLogicalSnapshot)?;
            i64::try_from(next).map_err(|_| StoreError::InvalidLogicalSnapshot)?;
        }
        prepare(&current)?;
        for instance_id in affected {
            transaction.execute(
                "DELETE FROM plugin_state WHERE instance_id = ?",
                [instance_id],
            )?;
            // Preserve ledger keys and settled results. Retire only unfinished work
            // tied to changed logical state; nothing is made pending or retryable.
            transaction.execute(
                "UPDATE command_ledger SET status = 'cancelled'
                WHERE instance_id = ? AND status IN ('pending', 'running')",
                [instance_id],
            )?;
            transaction.execute(
                "UPDATE command_outbox SET status = 'cancelled'
                WHERE instance_id = ? AND status IN ('pending', 'running')",
                [instance_id],
            )?;
            transaction.execute("UPDATE plugin_deliveries SET status = 'retired'
                WHERE instance_id = ? AND status IN ('pending', 'running', 'dead_letter', 'committed')", [instance_id])?;
            transaction.execute(
                "UPDATE scheduled_tasks SET status = 'cancelled'
                WHERE instance_id = ? AND status IN ('pending', 'firing')",
                [instance_id],
            )?;
        }
        for entry in &imported.state {
            let next = entry.revision.max(
                *old.get(&(entry.instance_id.as_str(), entry.key.as_str()))
                    .unwrap_or(&0),
            ) + 1;
            transaction.execute(
                "INSERT INTO plugin_state (instance_id, key, value, revision) VALUES (?, ?, ?, ?)",
                params![entry.instance_id, entry.key, entry.value, next],
            )?;
        }
        for item in &imported.installations {
            let mut item = item.clone();
            item.updated_at_ms = timestamp.max(
                versions
                    .get(item.instance_id.as_str())
                    .copied()
                    .unwrap_or(0)
                    + 1,
            );
            super::super::upsert_installation(&transaction, &item)?;
        }
        transaction.execute(
            "INSERT INTO logical_restore_commit VALUES (1, ?)
            ON CONFLICT(slot) DO UPDATE SET operation_id = excluded.operation_id",
            [operation_id],
        )?;
        apply_config()?;
        transaction.commit()?;
        Ok(())
    }
}

fn validate_merged(
    current: &LogicalSnapshot,
    imported: &LogicalSnapshot,
    affected: &BTreeSet<&String>,
    timestamp: i64,
) -> Result<(), StoreError> {
    let mut merged = current.clone();
    merged
        .state
        .retain(|entry| !affected.contains(&entry.instance_id));
    merged.state.extend(imported.state.iter().cloned());
    merged.installations.retain(|item| {
        !imported
            .installations
            .iter()
            .any(|i| i.instance_id == item.instance_id)
    });
    for item in &imported.installations {
        let mut item = item.clone();
        let previous = current
            .installations
            .iter()
            .find(|i| i.instance_id == item.instance_id)
            .map_or(0, |i| i.updated_at_ms);
        if let Some(previous) = current
            .installations
            .iter()
            .find(|i| i.instance_id == item.instance_id)
        {
            item.installed_at_ms = previous.installed_at_ms;
        }
        item.updated_at_ms = timestamp.max(
            previous
                .checked_add(1)
                .ok_or(StoreError::InvalidLogicalSnapshot)?,
        );
        merged.installations.push(item);
    }
    merged.validate()
}

fn validate_scope<'a>(
    operation_id: &str,
    imported: &LogicalSnapshot,
    affected_instances: &'a [String],
) -> Result<BTreeSet<&'a String>, StoreError> {
    imported.validate()?;
    let affected: BTreeSet<_> = affected_instances.iter().collect();
    if uuid::Uuid::parse_str(operation_id).is_err()
        || affected.len() > super::MAX_NAMESPACES
        || affected.len() != affected_instances.len()
        || affected.iter().any(|id| !super::valid_instance(id))
        || imported
            .state
            .iter()
            .any(|e| !affected.contains(&e.instance_id))
        || imported
            .installations
            .iter()
            .any(|i| !affected.contains(&i.instance_id))
        || imported
            .installations
            .iter()
            .any(|i| i.enabled || i.trust_level != "local-wasm" || i.signature_status != "unsigned")
    {
        return Err(StoreError::InvalidLogicalSnapshot);
    }
    Ok(affected)
}

fn chrono_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests;
