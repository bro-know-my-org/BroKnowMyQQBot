//! File-owned settings: saving never starts, stops or replays a plugin.
use super::{ManagementError, PluginManager, state_name};
use crate::{
    config::{self, ConfigFileLock},
    plugins::bundled::{self, Settings},
};
use plugin_host::validate_plugin_config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, io::Read as _, path::Path};

#[derive(Serialize)]
pub(crate) struct Catalogue {
    revision: String,
    plugins: Vec<BundledView>,
}

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct BundledView {
    key: &'static str,
    instance_id: &'static str,
    plugin_id: String,
    name: String,
    description: String,
    version: String,
    diagnostic: bool,
    enabled: bool,
    config: BTreeMap<String, Value>,
    schema: Option<Value>,
    requested_permissions: Vec<String>,
    loaded: bool,
    runtime_state: &'static str,
    pending: bool,
    restart_required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    name: String,
    expected_revision: String,
    confirm: bool,
    enabled: Option<bool>,
    config: Option<BTreeMap<String, Value>>,
}

impl PluginManager {
    pub(crate) async fn bundled_catalogue(&self) -> Result<Catalogue, ManagementError> {
        let path = self
            .marketplace
            .as_ref()
            .and_then(|context| context.declarations.clone());
        let source = tokio::task::spawn_blocking(move || {
            path.map_or_else(|| Ok(String::new()), |path| read_source(&path))
        })
        .await
        .map_err(|_| ManagementError::BundledUnavailable)??;
        self.bundled_view(&source)
    }

    fn bundled_view(&self, source: &str) -> Result<Catalogue, ManagementError> {
        let settings = Settings::parse(source).map_err(|()| ManagementError::BundledUnavailable)?;
        let snapshots = self
            .snapshot()?
            .host
            .instance_snapshots()
            .map_err(|_| ManagementError::Store)?;
        let mut plugins = Vec::new();
        for definition in bundled::catalogue() {
            let manifest = definition.plugin.manifest();
            let metadata = manifest
                .metadata
                .resolve("zh-CN")
                .map_err(|_| ManagementError::Store)?;
            let live = snapshots
                .iter()
                .find(|s| s.instance_id == definition.instance_id);
            let config = settings
                .configs
                .get(definition.key)
                .cloned()
                .unwrap_or_default();
            let enabled = settings.selected.iter().any(|key| key == definition.key);
            let pending =
                enabled != live.is_some() || (enabled && live.is_some_and(|s| s.config != config));
            plugins.push(BundledView {
                key: definition.key,
                instance_id: definition.instance_id,
                plugin_id: manifest.id.to_string(),
                name: metadata.name.clone(),
                description: metadata.description.clone(),
                version: manifest.version.clone(),
                diagnostic: super::super::plugins::is_diagnostic_bundled(definition.key),
                enabled,
                config,
                schema: definition.plugin.config_schema().cloned(),
                requested_permissions: manifest.requested_capabilities().into_iter().collect(),
                loaded: live.is_some(),
                runtime_state: live.map_or("not_loaded", |s| state_name(s.state)),
                pending,
                restart_required: pending,
            });
        }
        Ok(Catalogue {
            revision: config::config_revision(source.as_bytes()),
            plugins,
        })
    }

    pub(crate) async fn save_bundled(&self, change: Change) -> Result<Catalogue, ManagementError> {
        if !change.confirm
            || (change.enabled.is_none() && change.config.is_none())
            || change.expected_revision.len() != 64
            || !change
                .expected_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        {
            return Err(ManagementError::InvalidOperation);
        }
        bundled::definition(&change.name).map_err(|_| ManagementError::InvalidOperation)?;
        let manager = self.clone();
        self.operation(async move { manager.save_bundled_inner(change).await })
            .await
    }

    async fn save_bundled_inner(&self, change: Change) -> Result<Catalogue, ManagementError> {
        let _write = self.writes.lock().await;
        let path = self
            .marketplace
            .as_ref()
            .and_then(|context| context.declarations.clone())
            .ok_or(ManagementError::BundledUnavailable)?;
        let reading = path.clone();
        let (lock, source) = tokio::task::spawn_blocking(move || {
            let lock = ConfigFileLock::acquire(&reading).map_err(|e| file_error(&e))?;
            Ok::<_, ManagementError>((lock, read_source(&reading)?))
        })
        .await
        .map_err(|_| ManagementError::BundledUnavailable)??;
        if config::config_revision(source.as_bytes()) != change.expected_revision {
            return Err(ManagementError::Conflict);
        }
        let mut settings =
            Settings::parse(&source).map_err(|()| ManagementError::BundledUnavailable)?;
        if let Some(enabled) = change.enabled {
            let selected = settings.selected.iter().any(|name| name == &change.name);
            if enabled && !selected {
                settings.selected.push(change.name.clone());
            }
            if !enabled {
                settings.selected.retain(|name| name != &change.name);
            }
        }
        if let Some(value) = &change.config {
            settings.configs.insert(change.name.clone(), value.clone());
        }
        let mut document = source
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| ManagementError::BundledUnavailable)?;
        document["bundled"] = toml_edit::value(toml_edit::Array::from_iter(
            settings.selected.iter().map(String::as_str),
        ));
        if let Some(value) = change.config.as_ref() {
            // Replace the whole target config so deleted optional fields do not survive.
            let encoded = toml::to_string(value).map_err(|_| ManagementError::InvalidConfig)?;
            let replacement = encoded
                .parse::<toml_edit::DocumentMut>()
                .map_err(|_| ManagementError::InvalidConfig)?;
            if !document.as_table().contains_key("bundled_config") {
                document["bundled_config"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            document["bundled_config"]
                .as_table_like_mut()
                .ok_or(ManagementError::BundledUnavailable)?
                .insert(
                    &change.name,
                    toml_edit::Item::Value(toml_edit::Value::InlineTable(
                        replacement.as_table().clone().into_inline_table(),
                    )),
                );
        }
        let updated = document.to_string();
        let selected = Settings::parse(&updated).map_err(|()| ManagementError::InvalidConfig)?;
        for definition in bundled::catalogue() {
            if selected.selected.iter().any(|key| key == definition.key)
                || (change.config.is_some() && definition.key == change.name)
            {
                validate_plugin_config(
                    definition.plugin.as_ref(),
                    &selected
                        .configs
                        .get(definition.key)
                        .cloned()
                        .unwrap_or_default(),
                )
                .await
                .map_err(|_| ManagementError::InvalidConfig)?;
            }
        }
        bundled::validate_commands(&updated, &path, &self.store)
            .await
            .map_err(|()| ManagementError::InvalidConfig)?;
        if updated.len() > 1024 * 1024 {
            return Err(ManagementError::InvalidConfig);
        }
        let saved = updated.clone();
        tokio::task::spawn_blocking(move || {
            let result =
                config::atomic_write(&path, saved.as_bytes(), 0o600).map_err(|e| file_error(&e));
            drop(lock);
            result
        })
        .await
        .map_err(|_| ManagementError::BundledUnavailable)??;
        self.bundled_view(&updated)
    }
}

fn file_error(error: &config::ConfigError) -> ManagementError {
    if matches!(error, config::ConfigError::Conflict(_)) {
        ManagementError::Conflict
    } else {
        ManagementError::BundledUnavailable
    }
}

pub(super) fn read_source(path: &Path) -> Result<String, ManagementError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ManagementError::BundledUnavailable)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(ManagementError::BundledUnavailable);
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| ManagementError::BundledUnavailable)?;
    read_opened(file)
}

fn read_opened(file: fs::File) -> Result<String, ManagementError> {
    let metadata = file
        .metadata()
        .map_err(|_| ManagementError::BundledUnavailable)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(ManagementError::BundledUnavailable);
    }
    let mut source = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut source)
        .map_err(|_| ManagementError::BundledUnavailable)?;
    if source.len() > 1024 * 1024 {
        return Err(ManagementError::BundledUnavailable);
    }
    Ok(source)
}

#[cfg(test)]
#[path = "bundled_tests.rs"]
mod tests;
