//! BPP manifest types and compatibility validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

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
    pub metadata: PluginMetadata,
    pub version: String,
    pub protocol: String,
    #[serde(default = "default_state_version")]
    pub state_version: u32,
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
        let value: toml::Value = toml::from_str(source).map_err(ManifestError::Toml)?;
        let manifest: Self = if value
            .as_table()
            .is_some_and(|table| table.contains_key("metadata"))
        {
            value.try_into().map_err(ManifestError::Toml)?
        } else {
            LegacyPluginManifest::try_from(value)?.into()
        };
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
        self.metadata.validate()?;
        if self.entry != "component.wasm" {
            return Err(ManifestError::InvalidEntry(self.entry.clone()));
        }
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
        for action in &self.permissions.actions {
            if !matches!(action.as_str(), "message.reply" | "message.send") {
                return Err(ManifestError::UnsupportedAction(action.clone()));
            }
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

    pub fn requested_capabilities(&self) -> BTreeSet<String> {
        let mut capabilities = self.permissions.actions.clone();
        if !self.permissions.http.is_empty() {
            capabilities.insert("http.request".to_owned());
            capabilities.extend(self.permissions.http.iter().map(HttpPermission::capability));
        }
        if self.permissions.storage == StoragePermission::Private {
            capabilities.insert("storage.private".to_owned());
        }
        if self.permissions.stop_propagation {
            capabilities.insert("event.stop_propagation".to_owned());
        }
        if self.permissions.scheduler {
            capabilities.insert("schedule.create".to_owned());
            capabilities.insert("schedule.cancel".to_owned());
        }
        capabilities.extend(
            self.permissions
                .event_extensions
                .iter()
                .map(|extension| format!("event.extension.{extension}")),
        );
        capabilities
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPluginManifest {
    manifest_version: u32,
    id: PluginId,
    name: String,
    version: String,
    protocol: String,
    #[serde(default = "default_state_version")]
    state_version: u32,
    #[serde(default)]
    description: String,
    #[serde(default = "default_entry")]
    entry: String,
    #[serde(default)]
    runtime: PluginRuntimeConfig,
    #[serde(default)]
    subscriptions: Vec<Subscription>,
    #[serde(default)]
    commands: Vec<CommandDeclaration>,
    #[serde(default)]
    permissions: PluginPermissions,
}

impl TryFrom<toml::Value> for LegacyPluginManifest {
    type Error = ManifestError;

    fn try_from(value: toml::Value) -> Result<Self, Self::Error> {
        value.try_into().map_err(ManifestError::Toml)
    }
}

impl From<LegacyPluginManifest> for PluginManifest {
    fn from(legacy: LegacyPluginManifest) -> Self {
        Self {
            manifest_version: legacy.manifest_version,
            id: legacy.id,
            metadata: PluginMetadata::single_locale("en", legacy.name, legacy.description),
            version: legacy.version,
            protocol: legacy.protocol,
            state_version: legacy.state_version,
            entry: legacy.entry,
            runtime: legacy.runtime,
            subscriptions: legacy.subscriptions,
            commands: legacy.commands,
            permissions: legacy.permissions,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginMetadata {
    pub default_locale: String,
    pub locales: BTreeMap<String, LocalizedPluginMetadata>,
}

impl PluginMetadata {
    pub fn single_locale(
        locale: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        let locale = locale.into();
        Self {
            default_locale: locale.clone(),
            locales: BTreeMap::from([(
                locale,
                LocalizedPluginMetadata {
                    name: name.into(),
                    description: description.into(),
                },
            )]),
        }
    }

    pub fn resolve(
        &self,
        requested_locale: &str,
    ) -> Result<&LocalizedPluginMetadata, ManifestError> {
        self.validate()?;
        let canonical_locale = requested_locale
            .parse::<language_tags::LanguageTag>()
            .map_or_else(|_| requested_locale.to_owned(), |tag| tag.to_string());
        if let Some(metadata) = self.locales.get(&canonical_locale) {
            return Ok(metadata);
        }
        let mut candidate = canonical_locale.as_str();
        while let Some((prefix, _)) = candidate.rsplit_once('-') {
            candidate = prefix;
            if let Some(metadata) = self.locales.get(candidate) {
                return Ok(metadata);
            }
        }
        self.locales.get(&self.default_locale).ok_or_else(|| {
            ManifestError::InvalidMetadata(
                "metadata.default_locale must exist in metadata.locales".to_owned(),
            )
        })
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.locales.is_empty() || self.locales.len() > 16 {
            return Err(ManifestError::InvalidMetadata(
                "metadata must contain between 1 and 16 locales".to_owned(),
            ));
        }
        if !self.locales.contains_key(&self.default_locale) {
            return Err(ManifestError::InvalidMetadata(
                "metadata.default_locale must exist in metadata.locales".to_owned(),
            ));
        }
        for (locale, metadata) in &self.locales {
            if !is_canonical_locale(locale) {
                return Err(ManifestError::InvalidMetadata(format!(
                    "locale `{locale}` is not a supported canonical BCP 47 tag"
                )));
            }
            let name_length = metadata.name.chars().count();
            if name_length == 0 || name_length > 128 {
                return Err(ManifestError::InvalidMetadata(format!(
                    "locale `{locale}` name must contain between 1 and 128 characters"
                )));
            }
            if metadata.description.len() > 4096 {
                return Err(ManifestError::InvalidMetadata(format!(
                    "locale `{locale}` description exceeds 4096 bytes"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalizedPluginMetadata {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

fn is_canonical_locale(locale: &str) -> bool {
    locale
        .parse::<language_tags::LanguageTag>()
        .is_ok_and(|tag| tag.to_string() == locale)
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
    #[error("plugin entry `{0}` is unsupported; BPP 1.0 requires `component.wasm`")]
    InvalidEntry(String),
    #[error("plugin subscription must have non-empty id and event")]
    InvalidSubscription,
    #[error("duplicate plugin subscription `{0}`")]
    DuplicateSubscription(String),
    #[error("duplicate plugin command or alias `{0}`")]
    DuplicateCommand(String),
    #[error("invalid plugin HTTP permission: {0}")]
    InvalidHttpPermission(String),
    #[error("unsupported BPP 1.0 action capability `{0}`")]
    UnsupportedAction(String),
    #[error("invalid plugin display metadata: {0}")]
    InvalidMetadata(String),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{LocalizedPluginMetadata, PluginManifest, PluginMetadata, is_canonical_locale};

    #[test]
    fn accepts_canonical_bcp47_variants_and_extensions() {
        assert!(is_canonical_locale("sl-rozaj"));
        assert!(is_canonical_locale("en-US-u-ca-gregory"));
        assert!(!is_canonical_locale("en-us"));
    }

    #[test]
    fn parses_and_validates_manifest() {
        let manifest = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.ping"
                version = "0.1.0"
                protocol = ">=1.0,<2.0"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "Ping"

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
    fn migrates_legacy_v1_display_metadata() {
        let manifest = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.legacy"
                name = "Legacy"
                description = "Legacy metadata"
                version = "0.1.0"
                protocol = ">=1.0,<2.0"
            "#,
        )
        .unwrap();

        assert_eq!(manifest.metadata.default_locale, "en");
        assert_eq!(manifest.metadata.resolve("en").unwrap().name, "Legacy");
        assert_eq!(
            manifest.metadata.resolve("en").unwrap().description,
            "Legacy metadata"
        );
    }

    #[test]
    fn rejects_nonstandard_component_entry() {
        let error = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.entry"
                version = "0.1.0"
                protocol = ">=1.0,<2.0"
                entry = "guest.wasm"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "Entry"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("component.wasm"));
    }

    #[test]
    fn rejects_incompatible_protocol() {
        let error = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.future"
                version = "1.0.0"
                protocol = ">=2.0"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "Future"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("host provides"));
    }

    #[test]
    fn rejects_actions_outside_bpp_1_0_baseline() {
        let error = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.recall"
                version = "1.0.0"
                protocol = ">=1.0,<2.0"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "Recall"

                [permissions]
                actions = ["message.recall"]
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("BPP 1.0 action"));
    }

    #[test]
    fn validates_structured_http_permissions() {
        let manifest = PluginManifest::from_toml(
            r#"
                manifest_version = 1
                id = "dev.bkm.http"
                version = "1.0.0"
                protocol = ">=1.0,<2.0"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "HTTP"

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
                version = "1.0.0"
                protocol = ">=1.0,<2.0"

                [metadata]
                default_locale = "en"

                [metadata.locales.en]
                name = "HTTP"

                [[permissions.http]]
                host = "127.0.0.1"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("DNS name"));
    }

    #[test]
    fn resolves_exact_base_and_default_locales() {
        let metadata = PluginMetadata {
            default_locale: "zh-CN".to_owned(),
            locales: BTreeMap::from([
                (
                    "en".to_owned(),
                    LocalizedPluginMetadata {
                        name: "Weather".to_owned(),
                        description: String::new(),
                    },
                ),
                (
                    "en-GB".to_owned(),
                    LocalizedPluginMetadata {
                        name: "British Weather".to_owned(),
                        description: String::new(),
                    },
                ),
                (
                    "zh-CN".to_owned(),
                    LocalizedPluginMetadata {
                        name: "天气".to_owned(),
                        description: String::new(),
                    },
                ),
            ]),
        };
        metadata.validate().unwrap();
        assert_eq!(metadata.resolve("en-GB").unwrap().name, "British Weather");
        assert_eq!(metadata.resolve("en-gb").unwrap().name, "British Weather");
        assert_eq!(metadata.resolve("en-US").unwrap().name, "Weather");
        assert_eq!(metadata.resolve("ja").unwrap().name, "天气");
    }

    #[test]
    fn resolves_nearest_script_locale_and_rejects_invalid_metadata() {
        let metadata = PluginMetadata {
            default_locale: "en".to_owned(),
            locales: BTreeMap::from([
                (
                    "en".to_owned(),
                    LocalizedPluginMetadata {
                        name: "English".to_owned(),
                        description: String::new(),
                    },
                ),
                (
                    "zh-Hans".to_owned(),
                    LocalizedPluginMetadata {
                        name: "简体中文".to_owned(),
                        description: String::new(),
                    },
                ),
            ]),
        };
        assert_eq!(metadata.resolve("zh-Hans-CN").unwrap().name, "简体中文");

        let invalid = PluginMetadata {
            default_locale: "en".to_owned(),
            locales: BTreeMap::new(),
        };
        assert!(invalid.resolve("en").is_err());
    }

    #[test]
    fn rejects_missing_default_and_noncanonical_locale() {
        let missing_default = PluginMetadata {
            default_locale: "zh-CN".to_owned(),
            locales: BTreeMap::from([(
                "en".to_owned(),
                LocalizedPluginMetadata {
                    name: "Example".to_owned(),
                    description: String::new(),
                },
            )]),
        };
        assert!(
            missing_default
                .validate()
                .unwrap_err()
                .to_string()
                .contains("default_locale")
        );

        let noncanonical = PluginMetadata::single_locale("zh-cn", "示例", "");
        assert!(
            noncanonical
                .validate()
                .unwrap_err()
                .to_string()
                .contains("canonical BCP 47")
        );
    }

    #[test]
    fn enforces_display_metadata_size_limits() {
        let empty_name = PluginMetadata::single_locale("en", "", "description");
        assert!(empty_name.validate().is_err());

        let long_name = PluginMetadata::single_locale("en", "x".repeat(129), "");
        assert!(long_name.validate().is_err());

        let long_description = PluginMetadata::single_locale("en", "Example", "x".repeat(4097));
        assert!(long_description.validate().is_err());
    }
}
