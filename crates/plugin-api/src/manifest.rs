//! BPP manifest types and compatibility validation.

use std::{collections::BTreeSet, time::Duration};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Host;

use crate::{BPP_VERSION, PluginId};

const MAX_PLUGIN_TIMEOUT_MS: u64 = 30_000;
const MAX_PLUGIN_MEMORY_MB: u32 = 256;
const MAX_PLUGIN_FUEL: u64 = 1_000_000_000_000;
const MAX_STORAGE_QUOTA_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub manifest_version: u32,
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub protocol: String,
    #[serde(default = "default_state_version")]
    pub state_version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default)]
    pub runtime: PluginRuntimeConfig,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    #[serde(default)]
    pub commands: Vec<CommandDeclaration>,
    #[serde(default)]
    pub permissions: PluginPermissions,
}

impl PluginManifest {
    pub fn from_toml(source: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(source).map_err(ManifestError::Toml)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.manifest_version != 1 {
            return Err(ManifestError::UnsupportedManifestVersion(
                self.manifest_version,
            ));
        }
        Version::parse(&self.version).map_err(ManifestError::PluginVersion)?;
        let requirement =
            VersionReq::parse(&self.protocol).map_err(ManifestError::ProtocolRequirement)?;
        let host = Version::parse(BPP_VERSION).expect("BPP_VERSION must be valid SemVer");
        if !requirement.matches(&host) {
            return Err(ManifestError::IncompatibleProtocol {
                required: self.protocol.clone(),
                host: BPP_VERSION.to_owned(),
            });
        }
        if self.runtime.max_concurrency == 0 {
            return Err(ManifestError::ZeroConcurrency);
        }
        if self.runtime.max_concurrency > 1024 {
            return Err(ManifestError::ExcessiveConcurrency(
                self.runtime.max_concurrency,
            ));
        }
        if self.runtime.timeout_ms == 0 || self.runtime.timeout_ms > MAX_PLUGIN_TIMEOUT_MS {
            return Err(ManifestError::InvalidTimeout(self.runtime.timeout_ms));
        }
        if self.runtime.memory_mb == 0 || self.runtime.memory_mb > MAX_PLUGIN_MEMORY_MB {
            return Err(ManifestError::InvalidMemoryLimit(self.runtime.memory_mb));
        }
        if self.runtime.fuel == 0 || self.runtime.fuel > MAX_PLUGIN_FUEL {
            return Err(ManifestError::InvalidFuelLimit(self.runtime.fuel));
        }
        if self.permissions.storage_quota_bytes > MAX_STORAGE_QUOTA_BYTES {
            return Err(ManifestError::InvalidStorageQuota(
                self.permissions.storage_quota_bytes,
            ));
        }
        let mut subscription_ids = BTreeSet::new();
        for subscription in &self.subscriptions {
            if subscription.id.is_empty() || subscription.event.is_empty() {
                return Err(ManifestError::InvalidSubscription);
            }
            if !subscription_ids.insert(&subscription.id) {
                return Err(ManifestError::DuplicateSubscription(
                    subscription.id.clone(),
                ));
            }
        }
        let mut command_names = BTreeSet::new();
        for command in &self.commands {
            for name in std::iter::once(&command.name).chain(&command.aliases) {
                if name.is_empty() || !command_names.insert(name) {
                    return Err(ManifestError::DuplicateCommand(name.clone()));
                }
            }
        }
        for permission in &self.permissions.http {
            permission.validate()?;
        }
        Ok(())
    }

    pub const fn timeout(&self) -> Duration {
        Duration::from_millis(self.runtime.timeout_ms)
    }
}

const fn default_state_version() -> u32 {
    1
}

fn default_entry() -> String {
    "component.wasm".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeConfig {
    #[serde(default)]
    pub mode: RuntimeMode,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,
    #[serde(default = "default_fuel")]
    pub fuel: u64,
}

impl Default for PluginRuntimeConfig {
    fn default() -> Self {
        Self {
            mode: RuntimeMode::Serial,
            max_concurrency: default_max_concurrency(),
            timeout_ms: default_timeout_ms(),
            memory_mb: default_memory_mb(),
            fuel: default_fuel(),
        }
    }
}

const fn default_max_concurrency() -> u32 {
    1
}

const fn default_timeout_ms() -> u64 {
    3_000
}

const fn default_memory_mb() -> u32 {
    64
}

const fn default_fuel() -> u64 {
    10_000_000
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    #[default]
    Serial,
    Partitioned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub scopes: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandDeclaration {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginPermissions {
    #[serde(default)]
    pub actions: BTreeSet<String>,
    #[serde(default)]
    pub event_extensions: BTreeSet<String>,
    #[serde(default)]
    pub http: Vec<HttpPermission>,
    #[serde(default)]
    pub storage: StoragePermission,
    #[serde(default)]
    pub storage_quota_bytes: u64,
    #[serde(default)]
    pub scheduler: bool,
    #[serde(default)]
    pub stop_propagation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HttpPermission {
    pub host: String,
    #[serde(default = "default_https_port")]
    pub port: u16,
    #[serde(default = "default_http_methods")]
    pub methods: BTreeSet<String>,
    #[serde(default = "default_http_path_prefixes")]
    pub path_prefixes: BTreeSet<String>,
}

impl HttpPermission {
    pub fn capability(&self) -> String {
        format!("http.host.{}:{}", self.host, self.port)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let Ok(Host::Domain(normalized)) = Host::parse(&self.host) else {
            return Err(ManifestError::InvalidHttpPermission(
                "host must be a DNS name, not an IP literal".to_owned(),
            ));
        };
        if normalized != self.host
            || has_dns_suffix(&normalized, "localhost")
            || has_dns_suffix(&normalized, "local")
            || has_dns_suffix(&normalized, "internal")
            || has_dns_suffix(&normalized, "home.arpa")
        {
            return Err(ManifestError::InvalidHttpPermission(
                "host must be a normalized public DNS name".to_owned(),
            ));
        }
        if self.port == 0 {
            return Err(ManifestError::InvalidHttpPermission(
                "port must be non-zero".to_owned(),
            ));
        }
        if self.methods.is_empty()
            || self.methods.iter().any(|method| {
                !matches!(
                    method.as_str(),
                    "DELETE" | "GET" | "HEAD" | "PATCH" | "POST" | "PUT"
                )
            })
        {
            return Err(ManifestError::InvalidHttpPermission(
                "methods must use supported uppercase HTTP methods".to_owned(),
            ));
        }
        if self.path_prefixes.is_empty()
            || self.path_prefixes.iter().any(|path| {
                !path.starts_with('/')
                    || path.contains(['?', '#', '%'])
                    || path.split('/').any(|segment| matches!(segment, "." | ".."))
            })
        {
            return Err(ManifestError::InvalidHttpPermission(
                "path prefixes must be canonical absolute URL paths".to_owned(),
            ));
        }
        Ok(())
    }
}

fn has_dns_suffix(host: &str, suffix: &str) -> bool {
    host == suffix
        || host
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

const fn default_https_port() -> u16 {
    443
}

fn default_http_methods() -> BTreeSet<String> {
    BTreeSet::from(["GET".to_owned()])
}

fn default_http_path_prefixes() -> BTreeSet<String> {
    BTreeSet::from(["/".to_owned()])
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StoragePermission {
    #[default]
    None,
    Private,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("plugin manifest TOML is invalid")]
    Toml(#[source] toml::de::Error),
    #[error("unsupported plugin manifest version {0}")]
    UnsupportedManifestVersion(u32),
    #[error("plugin version is invalid")]
    PluginVersion(#[source] semver::Error),
    #[error("plugin protocol requirement is invalid")]
    ProtocolRequirement(#[source] semver::Error),
    #[error("plugin requires BPP `{required}`, host provides `{host}`")]
    IncompatibleProtocol { required: String, host: String },
    #[error("plugin max_concurrency must be greater than zero")]
    ZeroConcurrency,
    #[error("plugin max_concurrency {0} exceeds Host limit 1024")]
    ExcessiveConcurrency(u32),
    #[error("plugin timeout_ms {0} must be between 1 and 30000")]
    InvalidTimeout(u64),
    #[error("plugin memory_mb {0} must be between 1 and 256")]
    InvalidMemoryLimit(u32),
    #[error("plugin fuel {0} must be between 1 and 1000000000000")]
    InvalidFuelLimit(u64),
    #[error("plugin storage quota {0} exceeds Host limit 67108864")]
    InvalidStorageQuota(u64),
    #[error("plugin subscription must have non-empty id and event")]
    InvalidSubscription,
    #[error("duplicate plugin subscription `{0}`")]
    DuplicateSubscription(String),
    #[error("duplicate plugin command or alias `{0}`")]
    DuplicateCommand(String),
    #[error("invalid plugin HTTP permission: {0}")]
    InvalidHttpPermission(String),
}

#[cfg(test)]
mod tests {
    use super::PluginManifest;

    #[test]
    fn parses_and_validates_manifest() {
        let manifest = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.ping"
                name = "Ping"
                version = "0.1.0"
                protocol = ">=1.0,<2.0"

                [[subscriptions]]
                id = "messages"
                event = "message.created"

                [permissions]
                actions = ["message.reply"]
                storage = "private"
                storage_quota_bytes = 1024
            "#,
        )
        .unwrap();

        assert_eq!(manifest.id.as_str(), "dev.bkm.ping");
        assert_eq!(manifest.subscriptions.len(), 1);
    }

    #[test]
    fn rejects_incompatible_protocol() {
        let error = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.future"
                name = "Future"
                version = "1.0.0"
                protocol = ">=2.0"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("host provides"));
    }

    #[test]
    fn validates_structured_http_permissions() {
        let manifest = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.http"
                name = "HTTP"
                version = "1.0.0"
                protocol = ">=1.0,<2.0"

                [[permissions.http]]
                host = "api.example.com"
                methods = ["GET"]
                path_prefixes = ["/v1/"]
            "#,
        )
        .unwrap();
        assert_eq!(manifest.permissions.http[0].port, 443);

        let error = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.http"
                name = "HTTP"
                version = "1.0.0"
                protocol = ">=1.0,<2.0"

                [[permissions.http]]
                host = "127.0.0.1"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("DNS name"));
    }
}
