//! Allowlisted saved settings. Never serialize an effective, environment-overridden `BotConfig`.

use super::{plugins::ApiError, settings::source};
use crate::{
    config::{self, BotConfig, ConsoleLoggingConfig, OneBot11Config, QqConfig, RuntimeConfig},
    plugin_management::ManagementError,
};
use axum::{Json, extract::rejection::JsonRejection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Connections {
    qq: QqConfig,
    onebot11: OneBot11Config,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Global {
    runtime: RuntimeConfig,
    logging: Logging,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Logging {
    console: ConsoleLoggingConfig,
    files: Files,
}

// Deliberately exclude paths, compression internals, auth and management listeners.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Files {
    enabled: bool,
    filter: String,
    message_content: bool,
    runtime_max_file_mb: u64,
    runtime_max_total_mb: u64,
    messages_max_file_mb: u64,
    messages_max_total_mb: u64,
}

#[derive(Deserialize)]
#[serde(tag = "scope", rename_all = "lowercase", deny_unknown_fields)]
pub(super) enum Request {
    Connections {
        expected_revision: String,
        settings: Connections,
    },
    Global {
        expected_revision: String,
        settings: Global,
    },
}

pub(super) async fn get() -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(|| read_view(&config::application_config_path()))
        .await
        .map_err(|_| ManagementError::SettingsUnavailable)?
        .map(Json)
        .map_err(Into::into)
}

pub(super) async fn save(
    request: Result<Json<Request>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(request) = request.map_err(|_| ManagementError::InvalidSystemSettings)?;
    // A disconnected HTTP caller does not cancel an atomic save already in progress.
    tokio::task::spawn_blocking(move || update(&config::application_config_path(), request))
        .await
        .map_err(|_| ManagementError::SettingsUnavailable)?
        .map(Json)
        .map_err(Into::into)
}

fn selected(config: &BotConfig) -> (Connections, Global) {
    let files = &config.logging.files;
    (
        Connections {
            qq: config.qq.clone(),
            onebot11: config.onebot11.clone(),
        },
        Global {
            runtime: config.runtime.clone(),
            logging: Logging {
                console: config.logging.console.clone(),
                files: Files {
                    enabled: files.enabled,
                    filter: files.filter.clone(),
                    message_content: files.message_content,
                    runtime_max_file_mb: files.runtime_max_file_mb,
                    runtime_max_total_mb: files.runtime_max_total_mb,
                    messages_max_file_mb: files.messages_max_file_mb,
                    messages_max_total_mb: files.messages_max_total_mb,
                },
            },
        },
    )
}

fn safe_connections(settings: &Connections) -> bool {
    // Disabled adapters can contain unchecked strings in old files. Refuse to expose
    // URL userinfo/query material even in that case; no raw parse error is returned.
    [&settings.qq.webhook.listen, &settings.onebot11.listen]
        .into_iter()
        .all(|listen| listen.parse::<std::net::SocketAddr>().is_ok())
        && settings.qq.environment.len() <= 64
        && settings.qq.transport.len() <= 64
        && [&settings.qq.environment, &settings.qq.transport]
            .into_iter()
            .all(|value| {
                value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
        && settings.qq.webhook.path.len() <= 512
        && adapter_qqbot::is_literal_http_path(&settings.qq.webhook.path)
}

fn safe_global(settings: &Global) -> bool {
    settings.logging.console.language.len() <= 64
        && settings
            .logging
            .console
            .language
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && [
            &settings.logging.console.filter,
            &settings.logging.files.filter,
        ]
        .into_iter()
        .all(|filter| {
            !filter.trim().is_empty()
                && filter.len() <= 1024
                && tracing_subscriber::EnvFilter::try_new(filter.as_str()).is_ok()
        })
        && [
            settings.logging.files.runtime_max_file_mb,
            settings.logging.files.runtime_max_total_mb,
            settings.logging.files.messages_max_file_mb,
            settings.logging.files.messages_max_total_mb,
        ]
        .into_iter()
        .all(|size| size <= 1_048_576)
}

fn read_view(path: &Path) -> Result<Value, ManagementError> {
    let (text, revision) = source(path)?;
    view(&text, &revision)
}

fn view(text: &str, revision: &str) -> Result<Value, ManagementError> {
    let config: BotConfig =
        toml::from_str(text).map_err(|_| ManagementError::SettingsUnavailable)?;
    let (connections, global) = selected(&config);
    if !safe_connections(&connections) || !safe_global(&global) {
        return Err(ManagementError::SettingsUnavailable);
    }
    Ok(
        json!({"connections":connections,"global":global,"revision":revision,
        "apply":"restart_required","credential_presence":"unknown"}),
    )
}

fn update(config_file: &Path, request: Request) -> Result<Value, ManagementError> {
    let _lock = config::ConfigFileLock::acquire(config_file).map_err(|error| match error {
        config::ConfigError::Conflict(_) => ManagementError::Conflict,
        _ => ManagementError::SettingsUnavailable,
    })?;
    let (text, revision) = source(config_file)?;
    let connections = matches!(request, Request::Connections { .. });
    let (expected, patch) = match request {
        Request::Connections {
            expected_revision,
            settings,
        } => {
            if !safe_connections(&settings) {
                return Err(ManagementError::InvalidSystemSettings);
            }
            (expected_revision, toml::Value::try_from(settings))
        }
        Request::Global {
            expected_revision,
            settings,
        } => {
            if !safe_global(&settings) {
                return Err(ManagementError::InvalidSystemSettings);
            }
            (expected_revision, toml::Value::try_from(settings))
        }
    };
    if revision != expected {
        return Err(ManagementError::Conflict);
    }
    let encoded = config::patch_document(
        &text,
        &patch.map_err(|_| ManagementError::InvalidSystemSettings)?,
    )
    .map_err(|()| ManagementError::SettingsUnavailable)?;
    // Validate both the saved values and the full effective configuration. Only
    // the original file plus the typed patch is persisted, never environment values.
    let saved: BotConfig =
        toml::from_str(&encoded).map_err(|_| ManagementError::InvalidSystemSettings)?;
    let mut effective =
        BotConfig::check_source(&encoded).map_err(|_| ManagementError::InvalidSystemSettings)?;
    if connections {
        effective.qq = saved.qq;
        effective.onebot11 = saved.onebot11;
    } else {
        effective.runtime = saved.runtime;
        effective.logging.console = saved.logging.console;
        let directory = effective.logging.files.directory;
        effective.logging.files = saved.logging.files;
        effective.logging.files.directory = directory;
    }
    effective
        .validate()
        .map_err(|_| ManagementError::InvalidSystemSettings)?;
    let result = view(&encoded, &config::config_revision(encoded.as_bytes()))?;
    config::atomic_write(config_file, encoded.as_bytes(), 0o600)
        .map_err(|_| ManagementError::SettingsUnavailable)?;
    Ok(result)
}

#[cfg(test)]
#[path = "system_settings_tests.rs"]
mod tests;
