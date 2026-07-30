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

pub(crate) use model::{BotConfig, LoggingConfig};

const DEFAULT_CONFIG_PATH: &str = "config/bot.toml";
const DEFAULT_SECRETS_PATH: &str = "config/secrets.env";
const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_SECRETS_BYTES: usize = 64 * 1024;
const CONFIG_TEMPLATE: &str = include_str!("../../config/examples/bot.toml");
const SECRETS_TEMPLATE: &str = include_str!("../../config/examples/secrets.env");
const SMOKE_PLUGINS_TEMPLATE: &str = include_str!("../../config/examples/plugins.qq-smoke.toml");
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl BotConfig {
    pub(crate) fn load() -> Result<Self, ConfigError> {
        let explicit_path = env::var_os("BKM_CONFIG").map(PathBuf::from);
        let path = explicit_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let mut config = if path.exists() {
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
        if let Ok(value) = env::var("BKM_QQ_ENVIRONMENT") {
            self.qq.environment = value;
        }
        apply_bool_override(
            "BKM_QQ_PUBLIC_GUILD_MESSAGES",
            &mut self.qq.public_guild_messages,
        )?;
        apply_bool_override("BKM_QQ_CHECK_ONLY", &mut self.qq.check_only)?;
        if let Some(value) = env::var_os("BKM_PLUGIN_DB") {
            self.plugins.database = PathBuf::from(value);
        }
        if let Some(value) = env::var_os("BKM_PLUGIN_INSTALLATIONS") {
            self.plugins.installations = Some(PathBuf::from(value));
        }
        if let Ok(value) = env::var("BKM_EVENT_CONCURRENCY") {
            self.runtime.event_concurrency =
                value.parse().map_err(|_| ConfigError::InvalidEnvironment {
                    name: "BKM_EVENT_CONCURRENCY",
                    value,
                })?;
        }
        if let Ok(value) = env::var("RUST_LOG") {
            self.logging.console.filter.clone_from(&value);
            self.logging.files.filter = value;
        }
        if let Ok(value) = env::var("BKM_LOG_LANGUAGE") {
            self.logging.console.language = value;
        }
        if let Some(value) = env::var_os("BKM_LOG_DIRECTORY") {
            self.logging.files.directory = PathBuf::from(value);
        }
        if let Ok(value) = env::var("BKM_LOG_MESSAGE_CONTENT") {
            let enabled = parse_bool_environment("BKM_LOG_MESSAGE_CONTENT", &value)?;
            self.logging.console.message_content = enabled;
            self.logging.files.message_content = enabled;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !matches!(self.qq.environment.as_str(), "production" | "sandbox") {
            return Err(ConfigError::InvalidValue(
                "qq.environment must be `production` or `sandbox`".to_owned(),
            ));
        }
        if self.runtime.event_concurrency == 0
            || self.runtime.event_concurrency > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(ConfigError::InvalidValue(format!(
                "runtime.event_concurrency must be between 1 and {}",
                tokio::sync::Semaphore::MAX_PERMITS
            )));
        }
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

fn validate_log_capacity(kind: &str, file_mb: u64, total_mb: u64) -> Result<(), ConfigError> {
    if file_mb == 0 || total_mb == 0 || file_mb > total_mb {
        return Err(ConfigError::InvalidValue(format!(
            "logging.files.{kind}_max_file_mb must be greater than zero and no larger than {kind}_max_total_mb"
        )));
    }
    Ok(())
}

pub(crate) fn application_config_path() -> PathBuf {
    env::var_os("BKM_CONFIG").map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from)
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
    let explicit_path = env::var_os("BKM_SECRETS_FILE").map(PathBuf::from);
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
    ["BKM_QQ_OFFICIAL_APP_ID", "BKM_QQ_OFFICIAL_APP_SECRET"]
        .into_iter()
        .all(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

pub(crate) fn write_secret_environment(
    path: &Path,
    app_id: &str,
    app_secret: &str,
) -> Result<(), ConfigError> {
    let contents = format!(
        "# Local QQ credentials. This file is ignored by Git.\nBKM_QQ_OFFICIAL_APP_ID={}\nBKM_QQ_OFFICIAL_APP_SECRET={}\n",
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
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.file_type().is_file() || metadata.file_type().is_symlink())
    {
        return Err(ConfigError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to replace a non-regular file or symbolic link",
            ),
        });
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
                check_only = true

                [runtime]
                event_concurrency = 8

                [plugins]
                database = "state/plugins.db"
                installations = "plugins.toml"

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
        assert_eq!(config.runtime.event_concurrency, 8);
        assert_eq!(config.plugins.database, PathBuf::from("state/plugins.db"));
        assert!(config.logging.console.message_content);
        assert!(config.logging.files.message_content);
        assert_eq!(BotConfig::default().logging.console.language, "en");
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
