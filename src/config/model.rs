//! Serializable application configuration model.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct BotConfig {
    pub(crate) qq: QqConfig,
    pub(crate) runtime: RuntimeConfig,
    pub(crate) plugins: PluginsConfig,
    pub(crate) logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct QqConfig {
    pub(crate) environment: String,
    pub(crate) transport: String,
    pub(crate) public_guild_messages: bool,
    pub(crate) check_only: bool,
    pub(crate) webhook: QqWebhookConfig,
}

impl Default for QqConfig {
    fn default() -> Self {
        Self {
            environment: "production".to_owned(),
            transport: "websocket".to_owned(),
            public_guild_messages: false,
            check_only: false,
            webhook: QqWebhookConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct QqWebhookConfig {
    pub(crate) listen: String,
    pub(crate) path: String,
    pub(crate) timestamp_tolerance_seconds: u64,
    pub(crate) max_body_bytes: usize,
    pub(crate) max_request_concurrency: usize,
    pub(crate) request_timeout_seconds: u64,
}

impl Default for QqWebhookConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".to_owned(),
            path: "/callbacks/qq".to_owned(),
            timestamp_tolerance_seconds: 300,
            max_body_bytes: 1024 * 1024,
            max_request_concurrency: 64,
            request_timeout_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    pub(crate) event_concurrency: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_concurrency: 32,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PluginsConfig {
    pub(crate) database: PathBuf,
    pub(crate) installations: Option<PathBuf>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            database: PathBuf::from("data/plugins.db"),
            installations: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LoggingConfig {
    pub(crate) console: ConsoleLoggingConfig,
    pub(crate) files: FileLoggingConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ConsoleLoggingConfig {
    pub(crate) enabled: bool,
    pub(crate) language: String,
    pub(crate) ansi: bool,
    pub(crate) filter: String,
    pub(crate) message_content: bool,
}

impl Default for ConsoleLoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            language: "en".to_owned(),
            ansi: true,
            filter: "info".to_owned(),
            message_content: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileLoggingConfig {
    pub(crate) enabled: bool,
    pub(crate) directory: PathBuf,
    pub(crate) filter: String,
    pub(crate) message_content: bool,
    pub(crate) zstd_level: i32,
    pub(crate) buffer_lines: usize,
    pub(crate) runtime_max_file_mb: u64,
    pub(crate) runtime_max_total_mb: u64,
    pub(crate) messages_max_file_mb: u64,
    pub(crate) messages_max_total_mb: u64,
}

impl Default for FileLoggingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: PathBuf::from("logs"),
            filter: "debug".to_owned(),
            message_content: false,
            zstd_level: 3,
            buffer_lines: 8_192,
            runtime_max_file_mb: 64,
            runtime_max_total_mb: 512,
            messages_max_file_mb: 256,
            messages_max_total_mb: 2_048,
        }
    }
}
