//! Structured, non-secret application configuration.

mod model;
pub(crate) mod setup;

use std::{
    env, fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

pub(crate) use model::{BotConfig, LoggingConfig, ManagementConfig};

const DEFAULT_CONFIG_PATH: &str = "config/bot.toml";
const DEFAULT_SECRETS_PATH: &str = "config/secrets.env";
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_SECRETS_BYTES: usize = 64 * 1024;
const MAX_WEBHOOK_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS: u64 = 3600;
const MAX_WEBHOOK_REQUEST_CONCURRENCY: usize = 1024;
const MAX_WEBHOOK_REQUEST_TIMEOUT_SECONDS: u64 = 60;
const MAX_WEBHOOK_BUFFERED_BODY_BYTES: usize = 256 * 1024 * 1024;
const WEBHOOK_REQUEST_MEMORY_MULTIPLIER: usize = 4;
const MAX_RUNTIME_QUEUE_CAPACITY: usize = 16_384;
const MAX_RUNTIME_QUEUE_PAYLOAD_BYTES: usize = 512 * 1024 * 1024;
// An envelope retains raw protocol data plus mapped event fields and message
// segments. Budget four payload-sized copies to cover those owned values and
// allocator/container overhead conservatively.
const EVENT_ENVELOPE_MEMORY_MULTIPLIER: usize = 4;
const MAX_RETAINED_EVENT_ENVELOPES_PER_QUEUE_SLOT: usize = 2;
const MAX_RUNTIME_DEDUP_CAPACITY: usize = 262_144;
const MAX_RUNTIME_DEDUP_MEMORY_BYTES: usize = 128 * 1024 * 1024;
const DEDUP_ENTRY_MEMORY_ESTIMATE_BYTES: usize = 2 * 1024 + 256;
const DEDUP_AUXILIARY_MEMORY_ESTIMATE_BYTES: usize = 3 * 1024;
const CONFIG_TEMPLATE: &str = include_str!("../../config/examples/bot.toml");
const SECRETS_TEMPLATE: &str = include_str!("../../config/examples/secrets.env");
const SMOKE_PLUGINS_TEMPLATE: &str = include_str!("../../config/examples/plugins.qq-smoke.toml");
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl BotConfig {
    pub(crate) fn load() -> Result<Self, ConfigError> {
        let explicit_path = env::var_os("BKMQB_CONFIG").map(PathBuf::from);
        let path = explicit_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let exists = path.try_exists().map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let mut config = if exists {
            Self::from_path(&path)?
        } else if explicit_path.is_some() {
            return Err(ConfigError::Missing(path));
        } else {
            create_file_once(&path, CONFIG_TEMPLATE.as_bytes(), false)?;
            Self::from_path(&path)?
        };
        config.apply_environment_overrides()?;
        config.validate()?;
        Ok(config)
    }

    /// Re-reads and validates the effective configuration without creating files.
    pub(crate) fn check() -> Result<Self, ConfigError> {
        let path = env::var_os("BKMQB_CONFIG")
            .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
        if !path.try_exists().map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })? {
            return Err(ConfigError::Missing(path));
        }
        let mut config = Self::from_path(&path)?;
        config.apply_environment_overrides()?;
        config.validate()?;
        Ok(config)
    }

    /// Validates a proposed configuration against the current environment.
    pub(crate) fn check_source(source: &str) -> Result<Self, ConfigError> {
        let path = PathBuf::from("<request>");
        if source.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge {
                path,
                size: source.len(),
            });
        }
        let mut config: Self =
            toml::from_str(source).map_err(|source| ConfigError::Parse { path, source })?;
        config.apply_environment_overrides()?;
        config.validate()?;
        Ok(config)
    }

    fn from_path(path: &PathBuf) -> Result<Self, ConfigError> {
        let metadata = fs::metadata(path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(ConfigError::NotRegularFile(path.clone()));
        }
        let mut file = fs::File::open(path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let mut bytes = Vec::with_capacity(MAX_CONFIG_BYTES + 1);
        std::io::Read::by_ref(&mut file)
            .take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(ConfigError::TooLarge {
                path: path.clone(),
                size: bytes.len(),
            });
        }
        let source = std::str::from_utf8(&bytes).map_err(|source| ConfigError::Utf8 {
            path: path.clone(),
            source,
        })?;
        toml::from_str(source).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })
    }

    fn apply_environment_overrides(&mut self) -> Result<(), ConfigError> {
        apply_bool_override("BKMQB_QQ_ENABLED", &mut self.qq.enabled)?;
        if let Ok(value) = env::var("BKMQB_QQ_ENVIRONMENT") {
            self.qq.environment = value;
        }
        if let Ok(value) = env::var("BKMQB_QQ_TRANSPORT") {
            self.qq.transport = value;
        }
        apply_bool_override(
            "BKMQB_QQ_PUBLIC_GUILD_MESSAGES",
            &mut self.qq.public_guild_messages,
        )?;
        apply_bool_override(
            "BKMQB_QQ_PRIVATE_GUILD_MESSAGES",
            &mut self.qq.private_guild_messages,
        )?;
        apply_bool_override("BKMQB_QQ_DIRECT_MESSAGES", &mut self.qq.direct_messages)?;
        apply_bool_override("BKMQB_QQ_EXTENDED_EVENTS", &mut self.qq.extended_events.0)?;
        apply_bool_override("BKMQB_QQ_CHECK_ONLY", &mut self.qq.check_only)?;
        if let Ok(value) = env::var("BKMQB_QQ_WEBHOOK_LISTEN") {
            self.qq.webhook.listen = value;
        }
        if let Ok(value) = env::var("BKMQB_QQ_WEBHOOK_PATH") {
            self.qq.webhook.path = value;
        }
        apply_bool_override("BKMQB_ONEBOT11_ENABLED", &mut self.onebot11.enabled)?;
        if let Ok(value) = env::var("BKMQB_ONEBOT11_LISTEN") {
            self.onebot11.listen = value;
        }
        apply_bool_override(
            "BKMQB_ONEBOT11_ALLOW_INSECURE_REMOTE",
            &mut self.onebot11.allow_insecure_remote,
        )?;
        apply_parse_override(
            "BKMQB_ONEBOT11_ACTION_TIMEOUT_SECONDS",
            &mut self.onebot11.action_timeout_seconds,
        )?;
        apply_parse_override(
            "BKMQB_ONEBOT11_MAX_MESSAGE_BYTES",
            &mut self.onebot11.max_message_bytes,
        )?;
        apply_parse_override(
            "BKMQB_ONEBOT11_MAX_PENDING_ACTIONS",
            &mut self.onebot11.max_pending_actions,
        )?;
        if let Some(value) = env::var_os("BKMQB_PLUGIN_DB") {
            self.plugins.database = PathBuf::from(value);
        }
        if let Some(value) = env::var_os("BKMQB_PLUGIN_INSTALLATIONS") {
            self.plugins.installations = Some(PathBuf::from(value));
        }
        if let Ok(value) = env::var("BKMQB_EVENT_CONCURRENCY") {
            self.runtime.event_concurrency =
                value.parse().map_err(|_| ConfigError::InvalidEnvironment {
                    name: "BKMQB_EVENT_CONCURRENCY",
                    value,
                })?;
        }
        apply_parse_override("BKMQB_QUEUE_CAPACITY", &mut self.runtime.queue_capacity)?;
        apply_parse_override(
            "BKMQB_HANDLER_TIMEOUT_SECONDS",
            &mut self.runtime.handler_timeout_seconds,
        )?;
        apply_parse_override(
            "BKMQB_SHUTDOWN_TIMEOUT_SECONDS",
            &mut self.runtime.shutdown_timeout_seconds,
        )?;
        apply_parse_override("BKMQB_DEDUP_CAPACITY", &mut self.runtime.dedup_capacity)?;
        apply_bool_override("BKMQB_MANAGEMENT_ENABLED", &mut self.management.enabled)?;
        if let Ok(value) = env::var("BKMQB_MANAGEMENT_LISTEN") {
            self.management.listen = value;
        }
        if let Ok(value) = env::var("RUST_LOG") {
            self.logging.console.filter.clone_from(&value);
            self.logging.files.filter = value;
        }
        if let Ok(value) = env::var("BKMQB_LOG_LANGUAGE") {
            self.logging.console.language = value;
        }
        if let Some(value) = env::var_os("BKMQB_LOG_DIRECTORY") {
            self.logging.files.directory = PathBuf::from(value);
        }
        if let Ok(value) = env::var("BKMQB_LOG_MESSAGE_CONTENT") {
            let enabled = parse_bool_environment("BKMQB_LOG_MESSAGE_CONTENT", &value)?;
            self.logging.console.message_content = enabled;
            self.logging.files.message_content = enabled;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !self.qq.enabled && !self.onebot11.enabled {
            return Err(ConfigError::InvalidValue(
                "at least one Adapter must be enabled".to_owned(),
            ));
        }
        if self.qq.enabled && self.qq.check_only && self.onebot11.enabled {
            return Err(ConfigError::InvalidValue(
                "qq.check_only cannot be combined with an enabled OneBot 11 adapter".to_owned(),
            ));
        }
        if self.qq.enabled && !matches!(self.qq.environment.as_str(), "production" | "sandbox") {
            return Err(ConfigError::InvalidValue(
                "qq.environment must be `production` or `sandbox`".to_owned(),
            ));
        }
        if self.qq.enabled && self.qq.environment == "sandbox" && self.qq.direct_messages {
            return Err(ConfigError::InvalidValue(
                "qq.direct_messages is unavailable in the QQ sandbox environment".to_owned(),
            ));
        }
        if self.qq.enabled && !matches!(self.qq.transport.as_str(), "websocket" | "webhook") {
            return Err(ConfigError::InvalidValue(
                "qq.transport must be `websocket` or `webhook`".to_owned(),
            ));
        }
        if self.qq.enabled && self.qq.transport == "webhook" {
            validate_webhook_config(&self.qq.webhook)?;
        }
        if self.onebot11.enabled {
            validate_onebot11_config(&self.onebot11)?;
        }
        validate_runtime_config(self)?;
        if self.management.enabled {
            validate_management_config(&self.management)?;
        }
        validate_listener_conflicts(self)?;
        crate::plugin_marketplace::validate_index_url(&self.plugins.marketplace_url)
            .map_err(ConfigError::InvalidValue)?;
        if self.logging.console.filter.trim().is_empty() {
            return Err(ConfigError::InvalidValue(
                "logging.console.filter must not be empty".to_owned(),
            ));
        }
        if !matches!(self.logging.console.language.as_str(), "en" | "zh-CN") {
            return Err(ConfigError::InvalidValue(
                "logging.console.language must be `en` or `zh-CN`".to_owned(),
            ));
        }
        if self.logging.files.filter.trim().is_empty() {
            return Err(ConfigError::InvalidValue(
                "logging.files.filter must not be empty".to_owned(),
            ));
        }
        if self.logging.files.directory.as_os_str().is_empty() {
            return Err(ConfigError::InvalidValue(
                "logging.files.directory must not be empty".to_owned(),
            ));
        }
        if !(-7..=22).contains(&self.logging.files.zstd_level) {
            return Err(ConfigError::InvalidValue(
                "logging.files.zstd_level must be between -7 and 22".to_owned(),
            ));
        }
        if self.logging.files.buffer_lines == 0 {
            return Err(ConfigError::InvalidValue(
                "logging.files.buffer_lines must be greater than zero".to_owned(),
            ));
        }
        validate_log_capacity(
            "runtime",
            self.logging.files.runtime_max_file_mb,
            self.logging.files.runtime_max_total_mb,
        )?;
        validate_log_capacity(
            "messages",
            self.logging.files.messages_max_file_mb,
            self.logging.files.messages_max_total_mb,
        )?;
        Ok(())
    }

    pub(crate) fn save(&self) -> Result<PathBuf, ConfigError> {
        let path = application_config_path();
        let encoded = toml::to_string_pretty(self).map_err(ConfigError::Encode)?;
        atomic_write(&path, encoded.as_bytes(), 0o600)?;
        Ok(path)
    }
}

fn configured_max_event_bytes(config: &BotConfig) -> usize {
    let mut maximum = 0;
    if config.qq.enabled {
        maximum = if config.qq.transport == "webhook" {
            config.qq.webhook.max_body_bytes
        } else {
            1024 * 1024
        };
    }
    if config.onebot11.enabled {
        maximum = maximum.max(config.onebot11.max_message_bytes);
    }
    maximum.saturating_mul(EVENT_ENVELOPE_MEMORY_MULTIPLIER)
}

fn validate_runtime_config(config: &BotConfig) -> Result<(), ConfigError> {
    if config.runtime.event_concurrency == 0
        || config.runtime.event_concurrency > tokio::sync::Semaphore::MAX_PERMITS
    {
        return Err(ConfigError::InvalidValue(format!(
            "runtime.event_concurrency must be between 1 and {}",
            tokio::sync::Semaphore::MAX_PERMITS
        )));
    }
    if config.runtime.queue_capacity == 0
        || config.runtime.queue_capacity > MAX_RUNTIME_QUEUE_CAPACITY
    {
        return Err(ConfigError::InvalidValue(format!(
            "runtime.queue_capacity must be between 1 and {MAX_RUNTIME_QUEUE_CAPACITY}"
        )));
    }
    let max_event_bytes = configured_max_event_bytes(config);
    let adapter_buffer_slots = if config.onebot11.enabled {
        adapter_onebot11::MAX_PENDING_EVENTS_PER_CONNECTION
    } else {
        0
    };
    // Runtime dispatch caps task-owned envelopes at `queue_capacity`; during
    // shutdown it can additionally absorb at most the bounded channel's one
    // queue capacity while adapters drain.
    if config
        .runtime
        .queue_capacity
        .checked_mul(MAX_RETAINED_EVENT_ENVELOPES_PER_QUEUE_SLOT)
        .and_then(|slots| slots.checked_add(adapter_buffer_slots))
        .and_then(|slots| slots.checked_mul(max_event_bytes))
        .is_none_or(|bytes| bytes > MAX_RUNTIME_QUEUE_PAYLOAD_BYTES)
    {
        return Err(ConfigError::InvalidValue(format!(
            "runtime.queue_capacity, including channel, dispatched-event, and Adapter-local backlogs, multiplied by the estimated largest enabled Adapter envelope ({max_event_bytes} bytes) must not exceed {MAX_RUNTIME_QUEUE_PAYLOAD_BYTES} bytes"
        )));
    }
    if config.runtime.handler_timeout_seconds == 0 || config.runtime.handler_timeout_seconds > 3600
    {
        return Err(ConfigError::InvalidValue(
            "runtime.handler_timeout_seconds must be between 1 and 3600".to_owned(),
        ));
    }
    if config.runtime.shutdown_timeout_seconds == 0 || config.runtime.shutdown_timeout_seconds > 300
    {
        return Err(ConfigError::InvalidValue(
            "runtime.shutdown_timeout_seconds must be between 1 and 300".to_owned(),
        ));
    }
    if config.runtime.dedup_capacity == 0
        || config.runtime.dedup_capacity > MAX_RUNTIME_DEDUP_CAPACITY
    {
        return Err(ConfigError::InvalidValue(format!(
            "runtime.dedup_capacity must be between 1 and {MAX_RUNTIME_DEDUP_CAPACITY}"
        )));
    }
    if config
        .runtime
        .dedup_capacity
        .checked_mul(DEDUP_ENTRY_MEMORY_ESTIMATE_BYTES + DEDUP_AUXILIARY_MEMORY_ESTIMATE_BYTES)
        .is_none_or(|bytes| bytes > MAX_RUNTIME_DEDUP_MEMORY_BYTES)
    {
        return Err(ConfigError::InvalidValue(format!(
            "runtime.dedup_capacity estimated memory must not exceed {MAX_RUNTIME_DEDUP_MEMORY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_onebot11_config(config: &model::OneBot11Config) -> Result<(), ConfigError> {
    let listen = config.listen.parse::<std::net::SocketAddr>().map_err(|_| {
        ConfigError::InvalidValue("onebot11.listen must be an IP socket address".to_owned())
    })?;
    if listen.port() == 0 {
        return Err(ConfigError::InvalidValue(
            "onebot11.listen must use a non-zero port".to_owned(),
        ));
    }
    if !listen.ip().is_loopback() && !config.allow_insecure_remote {
        return Err(ConfigError::InvalidValue(
            "onebot11 non-loopback listen address requires allow_insecure_remote = true because the adapter does not terminate TLS"
                .to_owned(),
        ));
    }
    if config.action_timeout_seconds == 0 || config.action_timeout_seconds > 300 {
        return Err(ConfigError::InvalidValue(
            "onebot11.action_timeout_seconds must be between 1 and 300".to_owned(),
        ));
    }
    if config.max_message_bytes == 0 || config.max_message_bytes > 16 * 1024 * 1024 {
        return Err(ConfigError::InvalidValue(
            "onebot11.max_message_bytes must be between 1 and 16777216".to_owned(),
        ));
    }
    if config.max_pending_actions == 0 || config.max_pending_actions > 4096 {
        return Err(ConfigError::InvalidValue(
            "onebot11.max_pending_actions must be between 1 and 4096".to_owned(),
        ));
    }
    Ok(())
}

fn validate_management_config(config: &ManagementConfig) -> Result<(), ConfigError> {
    let listen = config.listen.parse::<std::net::SocketAddr>().map_err(|_| {
        ConfigError::InvalidValue("management.listen must be an IP socket address".to_owned())
    })?;
    if listen.port() == 0 {
        return Err(ConfigError::InvalidValue(
            "management.listen must use a non-zero port".to_owned(),
        ));
    }
    if !listen.ip().is_loopback() {
        return Err(ConfigError::InvalidValue(
            "management.listen must use a loopback address; expose it through an authenticated reverse proxy"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_listener_conflicts(config: &BotConfig) -> Result<(), ConfigError> {
    let mut listeners = Vec::new();
    if config.management.enabled {
        listeners.push((
            "management.listen",
            config.management.listen.parse::<std::net::SocketAddr>(),
        ));
    }
    if config.onebot11.enabled {
        listeners.push((
            "onebot11.listen",
            config.onebot11.listen.parse::<std::net::SocketAddr>(),
        ));
    }
    if config.qq.enabled && config.qq.transport == "webhook" {
        listeners.push((
            "qq.webhook.listen",
            config.qq.webhook.listen.parse::<std::net::SocketAddr>(),
        ));
    }
    let listeners = listeners
        .into_iter()
        .filter_map(|(name, listen)| listen.ok().map(|listen| (name, listen)))
        .collect::<Vec<_>>();
    for (index, (left_name, left)) in listeners.iter().enumerate() {
        for (right_name, right) in &listeners[index + 1..] {
            if socket_listeners_overlap(*left, *right) {
                return Err(ConfigError::InvalidValue(format!(
                    "{left_name} and {right_name} overlap at port {}",
                    left.port()
                )));
            }
        }
    }
    Ok(())
}

fn socket_listeners_overlap(left: std::net::SocketAddr, right: std::net::SocketAddr) -> bool {
    if left.port() != right.port() {
        return false;
    }
    if left.is_ipv4() == right.is_ipv4() {
        return left.ip() == right.ip()
            || left.ip().is_unspecified()
            || right.ip().is_unspecified();
    }
    // Use a conservative portable rule because listener creation does not set
    // IPV6_V6ONLY: an IPv6 wildcard may also occupy the IPv4 port.
    match (left, right) {
        (std::net::SocketAddr::V4(ipv4), std::net::SocketAddr::V6(ipv6))
        | (std::net::SocketAddr::V6(ipv6), std::net::SocketAddr::V4(ipv4)) => {
            ipv6.ip().is_unspecified()
                || ipv6
                    .ip()
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| ipv4.ip().is_unspecified() || mapped == *ipv4.ip())
        }
        _ => false,
    }
}

fn validate_log_capacity(kind: &str, file_mb: u64, total_mb: u64) -> Result<(), ConfigError> {
    if file_mb == 0 || total_mb == 0 || file_mb > total_mb {
        return Err(ConfigError::InvalidValue(format!(
            "logging.files.{kind}_max_file_mb must be greater than zero and no larger than {kind}_max_total_mb"
        )));
    }
    Ok(())
}

fn validate_webhook_config(config: &model::QqWebhookConfig) -> Result<(), ConfigError> {
    let listen = config.listen.parse::<std::net::SocketAddr>().map_err(|_| {
        ConfigError::InvalidValue("qq.webhook.listen must be an IP socket address".to_owned())
    })?;
    if listen.port() == 0 {
        return Err(ConfigError::InvalidValue(
            "qq.webhook.listen must use a non-zero port".to_owned(),
        ));
    }
    if !adapter_qqbot::is_literal_http_path(&config.path) {
        return Err(ConfigError::InvalidValue(
            "qq.webhook.path must be a literal absolute HTTP path".to_owned(),
        ));
    }
    if config.timestamp_tolerance_seconds == 0
        || config.timestamp_tolerance_seconds > MAX_WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS
    {
        return Err(ConfigError::InvalidValue(format!(
            "qq.webhook.timestamp_tolerance_seconds must be between 1 and {MAX_WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS}",
        )));
    }
    if config.max_body_bytes == 0 || config.max_body_bytes > MAX_WEBHOOK_BODY_BYTES {
        return Err(ConfigError::InvalidValue(format!(
            "qq.webhook.max_body_bytes must be between 1 and {MAX_WEBHOOK_BODY_BYTES}",
        )));
    }
    if config.max_request_concurrency == 0
        || config.max_request_concurrency > MAX_WEBHOOK_REQUEST_CONCURRENCY
    {
        return Err(ConfigError::InvalidValue(format!(
            "qq.webhook.max_request_concurrency must be between 1 and {MAX_WEBHOOK_REQUEST_CONCURRENCY}",
        )));
    }
    if config
        .max_body_bytes
        .checked_mul(config.max_request_concurrency)
        .and_then(|bytes| bytes.checked_mul(WEBHOOK_REQUEST_MEMORY_MULTIPLIER))
        .is_none_or(|bytes| bytes > MAX_WEBHOOK_BUFFERED_BODY_BYTES)
    {
        return Err(ConfigError::InvalidValue(format!(
            "qq.webhook estimated aggregate request memory must not exceed {MAX_WEBHOOK_BUFFERED_BODY_BYTES} bytes",
        )));
    }
    if config.request_timeout_seconds == 0
        || config.request_timeout_seconds > MAX_WEBHOOK_REQUEST_TIMEOUT_SECONDS
    {
        return Err(ConfigError::InvalidValue(format!(
            "qq.webhook.request_timeout_seconds must be between 1 and {MAX_WEBHOOK_REQUEST_TIMEOUT_SECONDS}",
        )));
    }
    Ok(())
}

pub(crate) fn application_config_path() -> PathBuf {
    env::var_os("BKMQB_CONFIG").map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from)
}

pub(crate) fn ensure_smoke_plugin_config() -> Result<PathBuf, ConfigError> {
    let application_path = application_config_path();
    let path = application_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plugins.toml");
    create_file_once(&path, SMOKE_PLUGINS_TEMPLATE.as_bytes(), false)?;
    Ok(path)
}

pub(crate) fn load_secret_environment() -> Result<Option<PathBuf>, ConfigError> {
    let explicit_path = env::var_os("BKMQB_SECRETS_FILE").map(PathBuf::from);
    let path = explicit_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SECRETS_PATH));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if explicit_path.is_some() {
                Err(ConfigError::MissingSecrets(path))
            } else if credentials_are_set() {
                Ok(None)
            } else {
                create_file_once(&path, SECRETS_TEMPLATE.as_bytes(), true)?;
                Ok(Some(path))
            };
        }
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.clone(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ConfigError::NotRegularFile(path));
    }
    let file = open_secrets_file(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ConfigError::NotRegularFile(path));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::InvalidValue(format!(
                "secrets environment file `{}` must not be accessible by group or other users",
                path.display()
            )));
        }
    }
    let mut contents = Vec::new();
    file.take((MAX_SECRETS_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
    if contents.len() > MAX_SECRETS_BYTES {
        return Err(ConfigError::TooLarge {
            path,
            size: contents.len(),
        });
    }
    dotenvy::from_read(std::io::Cursor::new(contents)).map_err(|source| ConfigError::Secrets {
        path: path.clone(),
        source,
    })?;
    Ok(Some(path))
}

#[cfg(unix)]
fn open_secrets_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_secrets_file(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

fn credentials_are_set() -> bool {
    ["BKMQB_QQ_OFFICIAL_APP_ID", "BKMQB_QQ_OFFICIAL_APP_SECRET"]
        .into_iter()
        .all(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

pub(crate) fn write_secret_environment(
    path: &Path,
    app_id: &str,
    app_secret: &str,
) -> Result<(), ConfigError> {
    let contents = format!(
        "# Local QQ credentials. This file is ignored by Git.\nBKMQB_QQ_OFFICIAL_APP_ID={}\nBKMQB_QQ_OFFICIAL_APP_SECRET={}\n",
        quote_environment_value(app_id),
        quote_environment_value(app_secret)
    );
    atomic_write(path, contents.as_bytes(), 0o600)
}

pub(crate) fn atomic_write(
    path: &Path,
    contents: &[u8],
    unix_mode: u32,
) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(ConfigError::Write {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "refusing to replace a non-regular file or symbolic link",
                ),
            });
        }
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let nonce = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(unix_mode);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        #[cfg(windows)]
        replace_file_preserving_destination(&temporary, path)?;
        #[cfg(not(windows))]
        fs::rename(&temporary, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file_preserving_destination(source: &Path, destination: &Path) -> std::io::Result<()> {
    if !destination.exists() {
        return fs::rename(source, destination);
    }
    let backup = destination.with_extension(format!(
        "bkm-backup-{}-{}",
        std::process::id(),
        TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::copy(destination, &backup)?;
    fs::remove_file(destination)?;
    if let Err(error) = fs::rename(source, destination) {
        let _ = fs::rename(&backup, destination);
        return Err(error);
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn quote_environment_value(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn create_file_once(path: &Path, contents: &[u8], secret: bool) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Create {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if secret { 0o600 } else { 0o644 });
    }
    match options.open(path) {
        Ok(mut file) => {
            if let Err(source) = file.write_all(contents).and_then(|()| file.sync_all()) {
                let _ = fs::remove_file(path);
                return Err(ConfigError::Create {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Ok(())
        }
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(ConfigError::Create {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn apply_bool_override(name: &'static str, target: &mut bool) -> Result<(), ConfigError> {
    let Ok(value) = env::var(name) else {
        return Ok(());
    };
    *target = parse_bool_environment(name, &value)?;
    Ok(())
}

fn apply_parse_override<T>(name: &'static str, target: &mut T) -> Result<(), ConfigError>
where
    T: std::str::FromStr,
{
    let Ok(value) = env::var(name) else {
        return Ok(());
    };
    *target = value
        .parse()
        .map_err(|_| ConfigError::InvalidEnvironment { name, value })?;
    Ok(())
}

fn parse_bool_environment(name: &'static str, value: &str) -> Result<bool, ConfigError> {
    let parsed = match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => {
            return Err(ConfigError::InvalidEnvironment {
                name,
                value: value.to_owned(),
            });
        }
    };
    Ok(parsed)
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("configured application file `{0}` does not exist")]
    Missing(PathBuf),
    #[error("configured application path `{0}` is not a regular file")]
    NotRegularFile(PathBuf),
    #[error("configured secrets environment file `{0}` does not exist")]
    MissingSecrets(PathBuf),
    #[error("failed to load secrets environment file `{path}`")]
    Secrets {
        path: PathBuf,
        #[source]
        source: dotenvy::Error,
    },
    #[error("failed to create generated configuration file `{path}`")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write configuration file `{path}`")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to encode application configuration")]
    Encode(#[source] toml::ser::Error),
    #[error("failed to read application configuration `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("application configuration `{path}` is too large ({size} bytes)")]
    TooLarge { path: PathBuf, size: usize },
    #[error("application configuration `{path}` is not UTF-8")]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("failed to parse application configuration `{path}`")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("environment variable `{name}` has invalid value `{value}`")]
    InvalidEnvironment { name: &'static str, value: String },
    #[error("invalid application configuration: {0}")]
    InvalidValue(String),
}

impl ConfigError {
    pub(crate) fn redacted_message(&self) -> String {
        match self {
            Self::InvalidEnvironment { name, .. } => {
                format!("environment variable `{name}` has an invalid value")
            }
            _ => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_non_secret_configuration() {
        let config: BotConfig = toml::from_str(
            r#"
                [qq]
                environment = "sandbox"
                public_guild_messages = true
                private_guild_messages = true
                direct_messages = false
                extended_events = true
                check_only = true

                [runtime]
                queue_capacity = 64
                event_concurrency = 8
                handler_timeout_seconds = 12
                shutdown_timeout_seconds = 9
                dedup_capacity = 2048

                [management]
                enabled = true
                listen = "127.0.0.1:9191"

                [plugins]
                database = "state/plugins.db"
                installations = "plugins.toml"
                marketplace_url = "https://example.github.io/marketplace/index.json"

                [logging.console]
                enabled = true
                filter = "info,adapter_qqbot=debug"
                message_content = true

                [logging.files]
                enabled = true
                directory = "logs-test"
                filter = "debug"
                message_content = true
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.qq.environment, "sandbox");
        assert!(config.qq.public_guild_messages);
        assert!(config.qq.private_guild_messages);
        assert!(!config.qq.direct_messages);
        assert!(config.qq.extended_events.is_enabled());
        assert_eq!(config.runtime.event_concurrency, 8);
        assert_eq!(config.runtime.queue_capacity, 64);
        assert_eq!(config.runtime.handler_timeout_seconds, 12);
        assert_eq!(config.runtime.shutdown_timeout_seconds, 9);
        assert_eq!(config.runtime.dedup_capacity, 2048);
        assert!(config.management.enabled);
        assert_eq!(config.plugins.database, PathBuf::from("state/plugins.db"));
        assert_eq!(
            config.plugins.marketplace_url,
            "https://example.github.io/marketplace/index.json"
        );
        assert!(config.logging.console.message_content);
        assert!(config.logging.files.message_content);
        assert_eq!(BotConfig::default().logging.console.language, "en");
    }

    #[test]
    fn rejects_guild_direct_messages_in_sandbox() {
        let mut config = BotConfig::default();
        config.qq.environment = "sandbox".to_owned();
        config.qq.direct_messages = true;
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("unavailable in the QQ sandbox"));
    }

    #[test]
    fn rejects_unknown_or_invalid_settings() {
        assert!(toml::from_str::<BotConfig>("unknown = true").is_err());
        let mut config = BotConfig::default();
        config.runtime.event_concurrency = 0;
        assert!(config.validate().is_err());
        let mut config = BotConfig::default();
        config.logging.console.language = "fr".to_owned();
        assert!(config.validate().is_err());
        let mut config = BotConfig::default();
        config.plugins.marketplace_url = "http://example.com/index.json".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn runtime_limits_enforce_a_combined_queue_memory_budget() {
        let mut config = BotConfig::default();
        config.runtime.queue_capacity = 65;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));

        let mut config = BotConfig::default();
        config.runtime.dedup_capacity = MAX_RUNTIME_DEDUP_CAPACITY + 1;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));

        let mut config = BotConfig::default();
        config.runtime.dedup_capacity = MAX_RUNTIME_DEDUP_MEMORY_BYTES
            / (DEDUP_ENTRY_MEMORY_ESTIMATE_BYTES + DEDUP_AUXILIARY_MEMORY_ESTIMATE_BYTES)
            + 1;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn accepts_onebot_only_and_rejects_no_adapters() {
        let mut config = BotConfig::default();
        config.qq.enabled = false;
        config.onebot11.enabled = true;
        config.validate().unwrap();

        config.onebot11.enabled = false;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn rejects_overlapping_enabled_listener_addresses() {
        let mut config = BotConfig::default();
        config.onebot11.enabled = true;
        config.management.enabled = true;
        config.onebot11.listen = "127.0.0.1:9090".to_owned();
        config.management.listen = "127.0.0.1:9090".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));

        let mut config = BotConfig::default();
        config.qq.transport = "webhook".to_owned();
        config.qq.webhook.listen = "0.0.0.0:9090".to_owned();
        config.management.enabled = true;
        config.management.listen = "127.0.0.1:9090".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));

        config.management.listen = "[::1]:9090".to_owned();
        config.validate().unwrap();

        assert!(socket_listeners_overlap(
            "[::]:9090".parse().unwrap(),
            "127.0.0.1:9090".parse().unwrap(),
        ));
        assert!(!socket_listeners_overlap(
            "0.0.0.0:9090".parse().unwrap(),
            "[::1]:9090".parse().unwrap(),
        ));
        assert!(socket_listeners_overlap(
            "127.0.0.1:9090".parse().unwrap(),
            "[::ffff:127.0.0.1]:9090".parse().unwrap(),
        ));
        assert!(socket_listeners_overlap(
            "0.0.0.0:9090".parse().unwrap(),
            "[::ffff:127.0.0.1]:9090".parse().unwrap(),
        ));
    }

    #[test]
    fn validates_onebot_limits_only_when_enabled() {
        let mut config = BotConfig::default();
        config.onebot11.listen = "not-a-socket".to_owned();
        config.validate().unwrap();

        config.qq.enabled = false;
        config.onebot11.enabled = true;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn remote_onebot_listener_requires_explicit_opt_in() {
        let mut config = BotConfig::default();
        config.qq.enabled = false;
        config.onebot11.enabled = true;
        config.onebot11.listen = "0.0.0.0:6700".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));

        config.onebot11.allow_insecure_remote = true;
        config.validate().unwrap();
    }

    #[test]
    fn management_listener_is_loopback_only() {
        let mut config = BotConfig::default();
        config.management.enabled = true;
        config.management.listen = "0.0.0.0:9090".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));

        config.management.listen = "[::1]:9090".to_owned();
        config.validate().unwrap();
    }

    #[test]
    fn qq_check_only_rejects_mixed_adapter_mode() {
        let mut config = BotConfig::default();
        config.qq.check_only = true;
        config.onebot11.enabled = true;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn generated_files_are_created_once_without_overwrite() {
        let directory = std::env::temp_dir().join(format!(
            "bkm-generated-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("bot.toml");
        create_file_once(&path, b"first", false).unwrap();
        create_file_once(&path, b"second", false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn generated_secret_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = std::env::temp_dir().join(format!(
            "bkm-generated-secret-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("secrets.env");
        create_file_once(&path, SECRETS_TEMPLATE.as_bytes(), true).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn secret_values_are_quoted_for_dotenv() {
        assert_eq!(quote_environment_value("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
