//! Read-only restore planning. Archive authority never becomes host authority.
use crate::{
    backup::{self, Result, invalid},
    config,
    plugin_management::ManagementError,
};
use plugin_host::{
    LogicalArchive, LogicalSnapshot, PluginInstallation, PluginInstanceSnapshot,
    ValidatedPluginPackage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub instance_id: String,
    pub manifest: plugin_api::PluginManifest,
    config: BTreeMap<String, Value>,
    grants: BTreeSet<String>,
}
#[derive(Clone, Serialize)]
pub(crate) struct Impact {
    instance_id: String,
    plugin_id: String,
    version: String,
    declaration_owned: bool,
    requested: BTreeSet<String>,
    retained_grants: BTreeSet<String>,
}
#[derive(Clone, Serialize)]
pub(crate) struct Inspection {
    pub operation_id: String,
    pub boot_id: String,
    pub expires_in_seconds: u64,
    pub manifest: plugin_host::LogicalArchiveManifest,
    pub plugins: Vec<Impact>,
    pub affected_instances: Vec<String>,
    pub changed_settings: Vec<String>,
    pub settings_before: Value,
    pub settings_after: Value,
}
#[allow(dead_code)] // The following offline-application stage consumes retained CAS data.
pub(crate) struct Prepared {
    pub archive: LogicalArchive,
    pub inspection: Inspection,
    pub old_config: String,
    pub new_config: String,
    pub expected: Vec<PluginInstallation>,
    pub declarations: Vec<PluginInstallation>,
    pub imported: LogicalSnapshot,
    pub declaration_revision: Option<String>,
    pub current_bindings: Value,
}

impl Prepared {
    pub(crate) fn new(
        archive: LogicalArchive,
        source: String,
        expected: Vec<PluginInstallation>,
        declarations: Vec<PluginInstallation>,
        runtime: &[PluginInstanceSnapshot],
    ) -> Result<Self> {
        if archive.configuration.len() != 2 {
            return Err(ManagementError::Backup);
        }
        let settings = archive
            .configuration
            .get("bot")
            .ok_or(ManagementError::Backup)?;
        let saved: config::BotConfig = toml::from_str(&source).map_err(invalid)?;
        let patch = toml::Value::try_from(settings).map_err(invalid)?;
        let new_config = config::patch_document(&source, &patch).map_err(invalid)?;
        let proposed: config::BotConfig = toml::from_str(&new_config).map_err(invalid)?;
        if backup::selected(&proposed) != *settings {
            return Err(ManagementError::Backup);
        }
        if proposed.logging.console.enabled
            && !saved.logging.console.enabled
            && saved.logging.console.message_content
            || proposed.logging.files.enabled
                && !saved.logging.files.enabled
                && saved.logging.files.message_content
        {
            return Err(ManagementError::Backup);
        }
        config::BotConfig::check_source(&new_config).map_err(invalid)?;
        let mut changed_settings = vec![];
        changes(
            "",
            &backup::selected(&saved),
            settings,
            &mut changed_settings,
        );
        let (mut affected, mut plugins) = inspect_bindings(&archive, &expected, runtime)?;
        let mut imported = archive.snapshot.clone();
        let manifests: BTreeMap<_, _> = archive
            .packages
            .iter()
            .map(|(digest, bytes)| {
                ValidatedPluginPackage::from_bytes(bytes)
                    .map(|package| (digest.clone(), package.manifest().clone()))
                    .map_err(invalid)
            })
            .collect::<Result<_>>()?;
        let mut preserved = BTreeSet::new();
        for item in &mut imported.installations {
            if !affected.insert(item.instance_id.clone()) {
                return Err(ManagementError::Backup);
            }
            let manifest = manifests
                .get(&item.package_sha256)
                .ok_or(ManagementError::Backup)?;
            let impact = inspect_package(
                item,
                &archive.snapshot,
                manifest,
                &expected,
                &declarations,
                runtime,
            )?;
            if impact.declaration_owned {
                preserved.insert(item.instance_id.clone());
            }
            plugins.push(impact);
        }
        imported
            .installations
            .retain(|i| !preserved.contains(&i.instance_id));
        for entry in &imported.state {
            // A package-free namespace has no verifiable identity or quota.
            if !affected.contains(&entry.instance_id) {
                return Err(ManagementError::Backup);
            }
        }
        if affected.len() > 512 {
            return Err(ManagementError::Backup);
        }
        imported.validate().map_err(invalid)?;
        Ok(Self {
            inspection: Inspection {
                operation_id: uuid::Uuid::new_v4().to_string(),
                boot_id: crate::restart::boot_id().into(),
                expires_in_seconds: 900,
                manifest: archive.manifest.clone(),
                plugins,
                affected_instances: affected.into_iter().collect(),
                changed_settings,
                settings_before:backup::selected(&saved),settings_after:settings.clone(),
            },
            archive,
            old_config: source,
            new_config,
            current_bindings:serde_json::json!(runtime.iter().filter(|b| !expected.iter().chain(&declarations).any(|i| i.instance_id == b.instance_id)).map(|b|serde_json::json!({"instance_id":b.instance_id,"manifest":b.manifest,"config":b.config,"grants":b.granted_capabilities})).collect::<Vec<_>>()),
            expected,
            declarations,
            imported,
            declaration_revision:None,
        })
    }

    pub(crate) async fn validate_guests(&self) -> Result<()> {
        // Validate ABI/schema/validate-config only: never init or receive events.
        for (digest, bytes) in &self.archive.packages {
            let bytes = bytes.clone();
            let package =
                tokio::task::spawn_blocking(move || ValidatedPluginPackage::from_bytes(&bytes))
                    .await
                    .map_err(invalid)?
                    .map_err(invalid)?;
            let plugin = plugin_host::WasmPlugin::from_package(package)
                .await
                .map_err(invalid)?;
            for item in self
                .archive
                .snapshot
                .installations
                .iter()
                .filter(|i| &i.package_sha256 == digest)
            {
                let config = self
                    .declarations
                    .iter()
                    .find(|i| i.instance_id == item.instance_id)
                    .map_or(&item.config, |i| &i.config);
                plugin_host::validate_plugin_config(&plugin, config)
                    .await
                    .map_err(|_| ManagementError::InvalidConfig)?;
            }
        }
        Ok(())
    }
}
fn check_quota(
    snapshot: &LogicalSnapshot,
    id: &str,
    manifest: &plugin_api::PluginManifest,
) -> Result<()> {
    let bytes: usize = snapshot
        .state
        .iter()
        .filter(|e| e.instance_id == id)
        .map(|e| e.key.len() + e.value.len())
        .sum();
    if bytes as u64 > manifest.permissions.storage_quota_bytes {
        return Err(ManagementError::Backup);
    }
    Ok(())
}
fn changes(path: &str, old: &Value, new: &Value, result: &mut Vec<String>) {
    if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
        for (key, value) in new {
            changes(
                &format!("{path}{key}."),
                old.get(key).unwrap_or(&Value::Null),
                value,
                result,
            );
        }
    } else if old != new {
        result.push(path.trim_end_matches('.').into());
    }
}

fn inspect_bindings(
    archive: &LogicalArchive,
    expected: &[PluginInstallation],
    runtime: &[PluginInstanceSnapshot],
) -> Result<(BTreeSet<String>, Vec<Impact>)> {
    let bindings: Vec<Binding> = serde_json::from_value(
        archive
            .configuration
            .get("bindings")
            .ok_or(ManagementError::Backup)?
            .clone(),
    )
    .map_err(invalid)?;
    if bindings.len() > 256 {
        return Err(ManagementError::Backup);
    }
    let mut affected = BTreeSet::new();
    let mut plugins = vec![];
    for binding in bindings {
        binding.manifest.validate().map_err(invalid)?;
        if binding.instance_id.len() > 192
            || binding.instance_id.is_empty()
            || !binding
                .instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
            || binding
                .instance_id
                .split('/')
                .any(|s| s.is_empty() || matches!(s, "." | ".."))
            || !affected.insert(binding.instance_id.clone())
            || archive
                .snapshot
                .installations
                .iter()
                .any(|i| i.instance_id == binding.instance_id)
        {
            return Err(ManagementError::Backup);
        }
        let current = runtime
            .iter()
            .find(|b| b.instance_id == binding.instance_id)
            .ok_or(ManagementError::Backup)?;
        if current.manifest.id != binding.manifest.id
            || current.manifest.version != binding.manifest.version
            || current.manifest.state_version != binding.manifest.state_version
            || expected
                .iter()
                .any(|i| i.instance_id == binding.instance_id)
        {
            return Err(ManagementError::Backup);
        }
        check_quota(&archive.snapshot, &binding.instance_id, &current.manifest)?;
        plugins.push(Impact {
            instance_id: binding.instance_id,
            plugin_id: current.manifest.id.to_string(),
            version: current.manifest.version.clone(),
            declaration_owned: true,
            requested: current.manifest.requested_capabilities(),
            retained_grants: current.granted_capabilities.clone(),
        });
    }
    Ok((affected, plugins))
}

fn inspect_package(
    item: &mut PluginInstallation,
    snapshot: &LogicalSnapshot,
    manifest: &plugin_api::PluginManifest,
    expected: &[PluginInstallation],
    declarations: &[PluginInstallation],
    runtime: &[PluginInstanceSnapshot],
) -> Result<Impact> {
    check_quota(snapshot, &item.instance_id, manifest)?;
    let declaration = declarations
        .iter()
        .find(|i| i.instance_id == item.instance_id);
    if let Some(current) = declaration {
        if current.package_sha256 != item.package_sha256 || current.plugin_id != item.plugin_id {
            return Err(ManagementError::Backup);
        }
        return Ok(Impact {
            instance_id: item.instance_id.clone(),
            plugin_id: current.plugin_id.clone(),
            version: current.version.clone(),
            declaration_owned: true,
            requested: manifest.requested_capabilities(),
            retained_grants: current.granted_permissions.iter().cloned().collect(),
        });
    }
    if crate::plugins::is_reserved_bundled_instance_id(&item.instance_id)
        || runtime.iter().any(|b| b.instance_id == item.instance_id)
            && !expected.iter().any(|i| i.instance_id == item.instance_id)
    {
        return Err(ManagementError::Backup);
    }
    let current = expected.iter().find(|i| i.instance_id == item.instance_id);
    if current.is_some_and(|i| i.plugin_id != item.plugin_id) {
        return Err(ManagementError::Backup);
    }
    let requested = manifest.requested_capabilities();
    item.granted_permissions = current
        .into_iter()
        .flat_map(|i| &i.granted_permissions)
        .filter(|p| requested.contains(*p))
        .cloned()
        .collect();
    item.requested_permissions = requested.iter().cloned().collect();
    item.metadata = manifest.metadata.clone();
    item.enabled = false;
    item.source = "logical-restore".into();
    item.trust_level = "local-wasm".into();
    item.signature_status = "unsigned".into();
    Ok(Impact {
        instance_id: item.instance_id.clone(),
        plugin_id: item.plugin_id.clone(),
        version: item.version.clone(),
        declaration_owned: false,
        requested,
        retained_grants: item.granted_permissions.iter().cloned().collect(),
    })
}

#[cfg(test)]
mod tests;
