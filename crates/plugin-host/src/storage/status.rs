//! Bounded metadata queries. Config is compared in SQLite, never read into a status response.
use super::{PluginStore, StoreError};
use crate::PluginInstanceStatus;
use plugin_api::PluginMetadata;
use rusqlite::params;
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct PluginInstallationStatus {
    pub instance_id: String,
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub package_sha256: String,
    pub enabled: bool,
    pub updated_at_ms: i64,
    pub config_matches: Option<bool>,
    pub grants_match: Option<bool>,
}

impl PluginStore {
    pub fn installation_statuses(
        &self,
        loaded: &[PluginInstanceStatus],
        limit: usize,
    ) -> Result<Vec<PluginInstallationStatus>, StoreError> {
        let mut connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        let transaction = connection.transaction()?;
        let mut statement = transaction.prepare(
            "SELECT substr(instance_id,1,513), substr(plugin_id,1,129),
             substr(metadata,1,65537), substr(version,1,129), substr(package_sha256,1,65),
             enabled, updated_at_ms, substr(granted_permissions,1,16385)
             FROM plugin_installations ORDER BY instance_id LIMIT ?",
        )?;
        let rows = statement.query_map([i64::try_from(limit.min(257)).unwrap_or(257)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Vec<u8>>(7)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (
                instance_id,
                plugin_id,
                metadata,
                version,
                package_sha256,
                enabled,
                updated_at_ms,
                grants,
            ) = row?;
            if instance_id.len() > 512
                || plugin_id.len() > 128
                || metadata.len() > 65536
                || version.len() > 128
                || package_sha256.len() > 64
                || grants.len() > 16384
            {
                return Err(StoreError::InvalidStatus);
            }
            let metadata: PluginMetadata = serde_json::from_slice(&metadata)?;
            let grants: BTreeSet<String> = serde_json::from_slice(&grants)?;
            let runtime = loaded.iter().find(|item| item.instance_id == instance_id);
            let config_matches = if let Some(runtime) = runtime {
                Some(transaction.query_row(
                    "SELECT CASE WHEN length(config) = ? THEN config = ? ELSE 0 END FROM plugin_installations WHERE instance_id = ?",
                    params![runtime.config_json.len(), runtime.config_json, instance_id],
                    |row| row.get(0),
                )?)
            } else {
                None
            };
            result.push(PluginInstallationStatus {
                name: metadata
                    .resolve("zh-CN")
                    .map_or_else(|_| plugin_id.clone(), |m| m.name.clone()),
                instance_id,
                plugin_id,
                version,
                package_sha256,
                enabled,
                updated_at_ms,
                config_matches,
                grants_match: runtime.map(|item| item.grants == grants),
            });
        }
        Ok(result)
    }
}
