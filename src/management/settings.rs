//! Version-checked updates to the existing Bot configuration file.

use super::plugins::{ApiError, ApiState};
use crate::{
    config::{self, BotConfig},
    plugin_management::{ManagementError, PluginManager},
};
use axum::{Json, extract::State};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Read as _,
    path::Path,
};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

pub(super) async fn get(State(state): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let manager = state.manager;
    let value = tokio::task::spawn_blocking(move || {
        read_view(&config::application_config_path(), &manager)
    })
    .await
    .map_err(|_| ManagementError::SettingsUnavailable)??;
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsRequest {
    prefix: String,
    expected_revision: String,
}

pub(super) async fn save(
    State(state): State<ApiState>,
    Json(request): Json<SettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    let manager = state.manager;
    let value = tokio::task::spawn_blocking(move || {
        update(
            &config::application_config_path(),
            &manager,
            &request.prefix,
            &request.expected_revision,
        )
    })
    .await
    .map_err(|_| ManagementError::SettingsUnavailable)??;
    Ok(Json(value))
}

pub(super) fn source(path: &Path) -> Result<(String, String), ManagementError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ManagementError::SettingsUnavailable)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ManagementError::SettingsUnavailable);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut bytes = Vec::new();
    options
        .open(path)
        .map_err(|_| ManagementError::SettingsUnavailable)?
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ManagementError::SettingsUnavailable)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ManagementError::SettingsUnavailable);
    }
    let revision = config::config_revision(&bytes);
    Ok((
        String::from_utf8(bytes).map_err(|_| ManagementError::SettingsUnavailable)?,
        revision,
    ))
}

fn read_view(path: &Path, manager: &PluginManager) -> Result<Value, ManagementError> {
    let (source, revision) = source(path)?;
    let saved: BotConfig =
        toml::from_str(&source).map_err(|_| ManagementError::SettingsUnavailable)?;
    let prefix = manager.command_prefix()?;
    Ok(
        json!({"prefix":prefix,"revision":revision,"saved_prefix":saved.commands.prefix,
        "pending":prefix != saved.commands.prefix}),
    )
}

pub(super) fn update(
    path: &Path,
    manager: &PluginManager,
    prefix: &str,
    expected: &str,
) -> Result<Value, ManagementError> {
    plugin_api::validate_command_prefix(prefix).map_err(|_| ManagementError::InvalidSettings)?;
    let _lock = config::ConfigFileLock::acquire(path).map_err(|error| match error {
        config::ConfigError::Conflict(_) => ManagementError::Conflict,
        _ => ManagementError::SettingsUnavailable,
    })?;
    let (source, revision) = source(path)?;
    if revision != expected {
        return Err(ManagementError::Conflict);
    }
    // Change only this setting; never serialize the environment-overridden configuration.
    let changes = toml::Value::try_from(json!({"commands":{"prefix":prefix}}))
        .map_err(|_| ManagementError::SettingsUnavailable)?;
    let encoded = config::patch_document(&source, &changes)
        .map_err(|()| ManagementError::SettingsUnavailable)?;
    let effective =
        BotConfig::check_source(&encoded).map_err(|_| ManagementError::SettingsUnavailable)?;
    if effective.commands.prefix != prefix {
        return Err(ManagementError::SettingsUnavailable);
    }
    config::atomic_write(path, encoded.as_bytes(), 0o600)
        .map_err(|_| ManagementError::SettingsUnavailable)?;
    manager.set_command_prefix(prefix)?;
    read_view(path, manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host::{PluginStore, StaticPluginHost};
    use std::collections::{BTreeMap, BTreeSet};

    fn manager() -> PluginManager {
        let store = PluginStore::in_memory().unwrap();
        PluginManager::new(
            StaticPluginHost::new(store.clone()),
            store,
            BTreeMap::new(),
            BTreeSet::new(),
        )
    }

    #[test]
    fn web_updates_cannot_be_overwritten_by_a_previously_loaded_setup_config() {
        let directory = std::env::temp_dir().join(format!("bkm-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bot.toml");
        fs::write(&path, "[runtime]\nevent_concurrency = 7\n").unwrap();
        let mut setup = BotConfig::load_from_path(&path, true).unwrap();
        setup.qq.environment = "sandbox".to_owned();
        let manager = manager();
        let original = read_view(&path, &manager).unwrap();
        update(
            &path,
            &manager,
            "!!",
            original["revision"].as_str().unwrap(),
        )
        .unwrap();
        let web_saved = fs::read(&path).unwrap();

        assert!(matches!(
            setup.save(),
            Err(config::ConfigError::Conflict(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), web_saved);
        assert_eq!(manager.command_prefix().unwrap(), "!!");

        // Reloading explicitly resolves the conflict and preserves the Web edit.
        let mut setup = BotConfig::load_from_path(&path, true).unwrap();
        setup.qq.environment = "sandbox".to_owned();
        setup.save().unwrap();
        let saved: BotConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.commands.prefix, "!!");
        assert_eq!(saved.qq.environment, "sandbox");
        assert_eq!(saved.runtime.event_concurrency, 7);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn setup_saves_invalidate_web_revisions_and_share_the_web_writer_lock() {
        let directory = std::env::temp_dir().join(format!("bkm-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bot.toml");
        fs::write(&path, "[qq]\nenvironment = 'production'\n").unwrap();
        let mut setup = BotConfig::load_from_path(&path, true).unwrap();
        setup.qq.environment = "sandbox".to_owned();
        let manager = manager();
        let original = read_view(&path, &manager).unwrap();
        let lock = config::ConfigFileLock::acquire(&path).unwrap();
        assert!(matches!(
            setup.save(),
            Err(config::ConfigError::Conflict(_))
        ));
        assert!(matches!(
            update(&path, &manager, "!", original["revision"].as_str().unwrap()),
            Err(ManagementError::Conflict)
        ));
        drop(lock);

        setup.save().unwrap();
        // A successful save must also refresh the snapshot for subsequent edits.
        setup.runtime.event_concurrency = 7;
        setup.save().unwrap();
        let setup_saved = fs::read(&path).unwrap();
        assert!(matches!(
            update(&path, &manager, "!", original["revision"].as_str().unwrap()),
            Err(ManagementError::Conflict)
        ));
        assert_eq!(fs::read(&path).unwrap(), setup_saved);
        let current = read_view(&path, &manager).unwrap();
        update(&path, &manager, "!", current["revision"].as_str().unwrap()).unwrap();
        let saved: BotConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.qq.environment, "sandbox");
        assert_eq!(saved.runtime.event_concurrency, 7);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn prefix_changes_persist_apply_and_reject_stale_revisions_without_replacing_other_settings() {
        let directory = std::env::temp_dir().join(format!("bkm-settings-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bot.toml");
        fs::write(
            &path,
            "[qq]\nenvironment = 'sandbox'\n[runtime]\nevent_concurrency = 7\n",
        )
        .unwrap();
        let store = PluginStore::in_memory().unwrap();
        let manager = PluginManager::new(
            StaticPluginHost::new(store.clone()),
            store,
            BTreeMap::new(),
            BTreeSet::new(),
        );
        let original = read_view(&path, &manager).unwrap();
        let changed = update(
            &path,
            &manager,
            "机器人",
            original["revision"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(changed["prefix"], "机器人");
        assert_eq!(changed["pending"], false);
        let saved: BotConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.commands.prefix, "机器人");
        assert_eq!(saved.qq.environment, "sandbox");
        assert_eq!(saved.runtime.event_concurrency, 7);
        assert!(matches!(
            update(&path, &manager, "!", original["revision"].as_str().unwrap()),
            Err(ManagementError::Conflict)
        ));
        assert_eq!(manager.command_prefix().unwrap(), "机器人");
        assert!(matches!(
            update(&path, &manager, "", changed["revision"].as_str().unwrap()),
            Err(ManagementError::InvalidSettings)
        ));
        fs::remove_dir_all(directory).unwrap();
    }
}
