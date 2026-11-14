//! Logical data only. Execution history and queues deliberately have no representation here.

use super::{PluginInstallation, PluginStore, StoreError, installation_row, validate_key};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

struct ByteBudget(usize);
impl std::io::Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("logical record too large"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const MAX_INSTALLATIONS: usize = 256;
const MAX_NAMESPACES: usize = 512;
const MAX_ROWS: usize = 16_384;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 512 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

mod archive;
mod restore;
pub use archive::{
    LogicalArchive, LogicalArchiveError, LogicalArchiveManifest, MAX_LOGICAL_ARCHIVE_BYTES,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalStateEntry {
    pub instance_id: String,
    pub key: String,
    pub value: Vec<u8>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalSnapshot {
    pub installations: Vec<PluginInstallation>,
    pub state: Vec<LogicalStateEntry>,
}

fn valid_instance(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

impl LogicalSnapshot {
    /// Validate before use; callers must separately bound the encoded input before decoding.
    pub fn validate(&self) -> Result<(), StoreError> {
        let invalid = || StoreError::InvalidLogicalSnapshot;
        if self.installations.len() > MAX_INSTALLATIONS || self.state.len() > MAX_ROWS {
            return Err(invalid());
        }
        let mut namespaces = BTreeSet::new();
        let mut installations = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut bytes = 0;
        for item in &self.installations {
            if !valid_instance(&item.instance_id) || !installations.insert(&item.instance_id) {
                return Err(invalid());
            }
            let mut budget = ByteBudget(MAX_RECORD_BYTES.min(MAX_TOTAL_BYTES - bytes));
            let available = budget.0;
            serde_json::to_writer(&mut budget, item).map_err(|_| invalid())?;
            bytes += available - budget.0;
            namespaces.insert(&item.instance_id);
        }
        for entry in &self.state {
            if !valid_instance(&entry.instance_id)
                || entry.key.len() > 1024
                || entry.value.len() > MAX_VALUE_BYTES
                || entry.revision == 0
                || i64::try_from(entry.revision).is_err()
                || !keys.insert((&entry.instance_id, &entry.key))
            {
                return Err(invalid());
            }
            validate_key(&entry.key)?;
            namespaces.insert(&entry.instance_id);
            bytes += entry.instance_id.len() + entry.key.len() + entry.value.len() + 8;
            if bytes > MAX_TOTAL_BYTES {
                return Err(invalid());
            }
        }
        if namespaces.len() > MAX_NAMESPACES || bytes > MAX_TOTAL_BYTES {
            return Err(invalid());
        }
        Ok(())
    }
}

impl PluginStore {
    /// One real SQLite read transaction covers both tables, including other connections'
    /// writes. The connection mutex also excludes in-process commits until it completes.
    pub fn logical_snapshot(&self) -> Result<LogicalSnapshot, StoreError> {
        self.logical_snapshot_inner(|| {})
    }

    fn logical_snapshot_inner(
        &self,
        after_installations: impl FnOnce(),
    ) -> Result<LogicalSnapshot, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let output = read_snapshot(&transaction, after_installations)?;
        transaction.commit()?;
        Ok(output)
    }
}

fn read_snapshot(
    transaction: &rusqlite::Transaction<'_>,
    after_installations: impl FnOnce(),
) -> Result<LogicalSnapshot, StoreError> {
    let mut output = LogicalSnapshot {
        installations: Vec::new(),
        state: Vec::new(),
    };
    let mut bytes = 0;
    {
        let mut statement = transaction.prepare(
            "SELECT plugin_id, metadata, instance_id, version, package_path, package_sha256,
                 source, trust_level, signature_status, requested_permissions, granted_permissions,
                 config, enabled, installed_at_ms, updated_at_ms
                 FROM plugin_installations ORDER BY instance_id LIMIT 257",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if output.installations.len() == MAX_INSTALLATIONS {
                return Err(StoreError::InvalidLogicalSnapshot);
            }
            // Inspect SQLite's borrowed bytes before any string/BLOB allocation or JSON parse.
            let mut record_bytes = 0;
            for column in 0..12 {
                let raw = row.get_ref(column)?;
                record_bytes += match raw {
                    rusqlite::types::ValueRef::Text(value)
                    | rusqlite::types::ValueRef::Blob(value) => value.len(),
                    _ => return Err(StoreError::InvalidLogicalSnapshot),
                };
            }
            bytes += record_bytes;
            if record_bytes > MAX_RECORD_BYTES || bytes > MAX_TOTAL_BYTES {
                return Err(StoreError::InvalidLogicalSnapshot);
            }
            output.installations.push(installation_row(row)?);
        }
    }
    after_installations();
    {
        let mut statement = transaction.prepare(
            "SELECT instance_id, key, value, revision FROM plugin_state
                 ORDER BY instance_id, key LIMIT 16385",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let instance = row
                .get_ref(0)?
                .as_str()
                .map_err(|_| StoreError::InvalidLogicalSnapshot)?;
            let key = row
                .get_ref(1)?
                .as_str()
                .map_err(|_| StoreError::InvalidLogicalSnapshot)?;
            let value = row
                .get_ref(2)?
                .as_blob()
                .map_err(|_| StoreError::InvalidLogicalSnapshot)?;
            bytes += instance.len() + key.len() + value.len() + 8;
            if output.state.len() == MAX_ROWS
                || !valid_instance(instance)
                || key.len() > 1024
                || value.len() > MAX_VALUE_BYTES
                || bytes > MAX_TOTAL_BYTES
            {
                return Err(StoreError::InvalidLogicalSnapshot);
            }
            output.state.push(LogicalStateEntry {
                instance_id: instance.to_owned(),
                key: key.to_owned(),
                value: value.to_vec(),
                revision: row.get(3)?,
            });
        }
    }
    output.validate()?;
    Ok(output)
}

#[cfg(test)]
mod tests;
