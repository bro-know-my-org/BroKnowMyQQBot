//! Administrator-controlled WASM plugin installation declarations.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tracing::info;

#[cfg(feature = "coverage-probes")]
use builtin_plugins::{
    ActiveSendProbePlugin, BrowserProbePlugin, DevToolsPlugin, HttpProbePlugin,
    QqExtensionProbePlugin, SchedulerProbePlugin,
};
use builtin_plugins::{
    AdminPlugin, CounterPlugin, EchoPlugin, HelpPlugin, PingPlugin, ReminderPlugin,
};
#[cfg(feature = "coverage-probes")]
use plugin_host::TrustedPlatformQuery;
use plugin_host::{
    PluginStore, StaticPluginHost, TrustedAuthorityProvider, TrustedPluginCapabilities,
    ValidatedPluginPackage, WasmPlugin,
};

const MAX_INSTALLATION_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const DEFAULT_BUNDLED_PLUGINS: &[&str] =
    &["ping", "help", "echo", "counter", "reminder"];
const COVERAGE_PROBE_NAMES: &[&str] = &[
    "http-probe",
    "browser-probe",
    "scheduler-probe",
    "qq-extension-probe",
    "active-send-probe",
    "devtools",
];
const BUNDLED_PLUGIN_INSTANCES: &[(&str, &str)] = &[
    ("ping", "dev.bkm.ping/default"),
    ("help", "dev.bkm.help/default"),
    ("echo", "dev.bkm.echo/default"),
    ("counter", "dev.bkm.counter/default"),
    ("admin", "dev.bkm.admin/default"),
    ("reminder", "dev.bkm.reminder/default"),
    ("http-probe", "dev.bkm.http-probe/default"),
    ("browser-probe", "dev.bkm.browser-probe/default"),
    ("scheduler-probe", "dev.bkm.scheduler-probe/default"),
    ("qq-extension-probe", "dev.bkm.qq-extension-probe/default"),
    ("active-send-probe", "dev.bkm.active-send-probe/default"),
    ("devtools", "dev.bkm.devtools/default"),
];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginInstallations {
    bundled: Option<Vec<String>>,
    #[serde(default)]
    bundled_config: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    wasm: Vec<WasmPluginInstallation>,
}

impl PluginInstallations {
    fn from_path(path: &Path) -> Result<Self, PluginConfigError> {
        let metadata = fs::metadata(path).map_err(|source| PluginConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.is_file() {
            return Err(PluginConfigError::NotRegularFile(path.to_path_buf()));
        }
        let mut file = fs::File::open(path).map_err(|source| PluginConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if !file
            .metadata()
            .map_err(|source| PluginConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?
            .is_file()
        {
            return Err(PluginConfigError::NotRegularFile(path.to_path_buf()));
        }
        let mut bytes = Vec::with_capacity(MAX_INSTALLATION_FILE_BYTES + 1);
        file.by_ref()
            .take((MAX_INSTALLATION_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| PluginConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() > MAX_INSTALLATION_FILE_BYTES {
            return Err(PluginConfigError::TooLarge {
                path: path.to_path_buf(),
                size: bytes.len(),
            });
        }
        let source = std::str::from_utf8(&bytes).map_err(|source| PluginConfigError::Utf8 {
            path: path.to_path_buf(),
            source,
        })?;
        let installations = toml::from_str(source).map_err(|source| PluginConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(installations)
    }
}

pub(crate) fn is_reserved_bundled_instance_id(instance_id: &str) -> bool {
    BUNDLED_PLUGIN_INSTANCES
        .iter()
        .any(|(_, bundled_instance_id)| *bundled_instance_id == instance_id)
}

pub(crate) fn is_declared_instance_id(
    installation_file: Option<&Path>,
    instance_id: &str,
) -> Result<bool, PluginConfigError> {
    let Some(path) = installation_file else {
        return Ok(false);
    };
    Ok(PluginInstallations::from_path(path)?
        .wasm
        .iter()
        .any(|installation| installation.instance_id == instance_id))
}

fn bundled_plugin_instance_id(name: &str) -> Option<&'static str> {
    BUNDLED_PLUGIN_INSTANCES
        .iter()
        .find_map(|(bundled_name, instance_id)| (*bundled_name == name).then_some(*instance_id))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WasmPluginInstallation {
    package: PathBuf,
    instance_id: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    grants: BTreeSet<String>,
    #[serde(default)]
    config: BTreeMap<String, Value>,
}

impl WasmPluginInstallation {
    fn resolved_package(&self, installation_file: &Path) -> PathBuf {
        if self.package.is_absolute() {
            return self.package.clone();
        }
        installation_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&self.package)
    }
}

const fn enabled_by_default() -> bool {
    true
}

#[derive(Debug)]
pub(crate) struct LoadedPlugins {
    pub(crate) packages: BTreeMap<String, String>,
    pub(crate) file_instances: BTreeSet<String>,
}

pub(crate) async fn load_plugins(
    plugins: &mut StaticPluginHost,
    store: &PluginStore,
    installation_file: Option<&Path>,
) -> Result<LoadedPlugins, Box<dyn std::error::Error>> {
    let installations = installation_file.map_or_else(
        || Ok(PluginInstallations::default()),
        PluginInstallations::from_path,
    )?;
    let (include_help, mut declared_instances) = register_bundled_plugins(
        plugins,
        installations.bundled,
        &installations.bundled_config,
    )
    .await?;
    let file_instances: BTreeSet<_> = installations
        .wasm
        .iter()
        .map(|item| item.instance_id.clone())
        .collect();
    for installation in installations.wasm {
        if !installation.enabled {
            info!(
                instance_id = installation.instance_id,
                "WASM plugin is disabled"
            );
            continue;
        }
        if is_reserved_bundled_instance_id(&installation.instance_id) {
            return Err(Box::new(PluginConfigError::ReservedInstance(
                installation.instance_id,
            )));
        }
        if !declared_instances.insert(installation.instance_id.clone()) {
            return Err(Box::new(PluginConfigError::DuplicateInstance(
                installation.instance_id,
            )));
        }
        let package_path = installation.resolved_package(
            installation_file.expect("WASM declarations require an installation file"),
        );
        let package = load_validated_package(package_path.clone()).await?;
        let plugin_id = package.manifest().id.to_string();
        let package_sha256 = package.package_sha256().to_owned();
        let plugin = std::sync::Arc::new(WasmPlugin::from_package(package).await?);
        plugins
            .register(
                plugin,
                installation.instance_id.clone(),
                installation.config,
                installation.grants,
            )
            .await?;
        info!(
            plugin_id,
            instance_id = installation.instance_id,
            package = %package_path.display(),
            package_sha256,
            "loaded local WASM plugin"
        );
    }
    let installation_store = store.clone();
    let managed_installations =
        tokio::task::spawn_blocking(move || installation_store.enabled_installations()).await??;
    let mut loaded_packages = BTreeMap::new();
    for installation in managed_installations {
        if is_reserved_bundled_instance_id(&installation.instance_id) {
            return Err(Box::new(PluginConfigError::ReservedInstance(
                installation.instance_id,
            )));
        }
        if file_instances.contains(&installation.instance_id)
            || !declared_instances.insert(installation.instance_id.clone())
        {
            return Err(Box::new(PluginConfigError::DuplicateInstance(
                installation.instance_id,
            )));
        }
        let package_path = PathBuf::from(&installation.package_path);
        let package = load_validated_package(package_path.clone()).await?;
        validate_managed_package(&installation, &package)?;
        let plugin = std::sync::Arc::new(WasmPlugin::from_package(package).await?);
        let loaded_id = installation.instance_id.clone();
        let loaded_hash = installation.package_sha256.clone();
        plugins
            .register(
                plugin,
                installation.instance_id,
                installation.config,
                installation.granted_permissions.into_iter().collect(),
            )
            .await?;
        loaded_packages.insert(loaded_id, loaded_hash);
    }
    if include_help {
        register_help_plugin(plugins).await?;
    }
    Ok(LoadedPlugins {
        packages: loaded_packages,
        file_instances,
    })
}

fn validate_managed_package(
    installation: &plugin_host::PluginInstallation,
    package: &ValidatedPluginPackage,
) -> Result<(), PluginConfigError> {
    if package.package_sha256() != installation.package_sha256 {
        return Err(PluginConfigError::PackageHashChanged {
            path: PathBuf::from(&installation.package_path),
            expected: installation.package_sha256.clone(),
            actual: package.package_sha256().to_owned(),
        });
    }
    if package.manifest().id.as_str() != installation.plugin_id
        || package.manifest().version != installation.version
    {
        return Err(PluginConfigError::PackageIdentityChanged {
            instance_id: installation.instance_id.clone(),
        });
    }
    Ok(())
}

async fn load_validated_package(
    package_path: PathBuf,
) -> Result<ValidatedPluginPackage, Box<dyn std::error::Error>> {
    Ok(
        tokio::task::spawn_blocking(move || ValidatedPluginPackage::from_path(&package_path))
            .await??,
    )
}

async fn register_help_plugin(
    plugins: &mut StaticPluginHost,
) -> Result<(), plugin_host::PluginHostError> {
    let help = HelpPlugin::with_commands(plugins.command_declarations()?);
    plugins
        .register_trusted(
            std::sync::Arc::new(help),
            "dev.bkm.help/default",
            BTreeMap::new(),
        )
        .await
}

async fn register_bundled_plugins(
    plugins: &mut StaticPluginHost,
    selected: Option<Vec<String>>,
    configs: &BTreeMap<String, BTreeMap<String, Value>>,
) -> Result<(bool, BTreeSet<String>), Box<dyn std::error::Error>> {
    let selected = selected.unwrap_or_else(|| {
        DEFAULT_BUNDLED_PLUGINS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    });
    #[cfg(feature = "coverage-probes")]
    let selected = {
        let mut selected = selected;
        let requires_authority = selected
            .iter()
            .any(|name| COVERAGE_PROBE_NAMES.contains(&name.as_str()));
        if requires_authority {
            let admin = selected
                .iter()
                .position(|name| name == "admin")
                .ok_or(PluginConfigError::MissingDiagnosticAuthority)?;
            let provider = selected.remove(admin);
            selected.insert(0, provider);
        }
        selected
    };
    let mut seen = BTreeSet::new();
    let mut instance_ids = BTreeSet::new();
    let mut include_help = false;
    for name in selected {
        if !seen.insert(name.clone()) {
            return Err(Box::new(PluginConfigError::DuplicateBundled(name)));
        }
        let Some(instance_id) = bundled_plugin_instance_id(&name) else {
            return Err(Box::new(PluginConfigError::UnknownBundled(name)));
        };
        if name == "help" {
            include_help = true;
            instance_ids.insert(instance_id.to_owned());
            continue;
        }
        let plugin: std::sync::Arc<dyn plugin_api::StaticPlugin> = match name.as_str() {
            "ping" => std::sync::Arc::new(PingPlugin::default()),
            "echo" => std::sync::Arc::new(EchoPlugin::default()),
            "counter" => std::sync::Arc::new(CounterPlugin::default()),
            "admin" => std::sync::Arc::new(AdminPlugin::default()),
            "reminder" => std::sync::Arc::new(ReminderPlugin::default()),
            #[cfg(feature = "coverage-probes")]
            "http-probe" => std::sync::Arc::new(HttpProbePlugin::default()),
            #[cfg(feature = "coverage-probes")]
            "browser-probe" => std::sync::Arc::new(BrowserProbePlugin::default()),
            #[cfg(feature = "coverage-probes")]
            "scheduler-probe" => std::sync::Arc::new(SchedulerProbePlugin::default()),
            #[cfg(feature = "coverage-probes")]
            "qq-extension-probe" => std::sync::Arc::new(QqExtensionProbePlugin::default()),
            #[cfg(feature = "coverage-probes")]
            "active-send-probe" => std::sync::Arc::new(ActiveSendProbePlugin::default()),
            #[cfg(feature = "coverage-probes")]
            "devtools" => std::sync::Arc::new(DevToolsPlugin::default()),
            #[cfg(not(feature = "coverage-probes"))]
            _ if COVERAGE_PROBE_NAMES.contains(&name.as_str()) => {
                return Err(Box::new(PluginConfigError::CoverageProbeUnavailable(name)));
            }
            _ => return Err(Box::new(PluginConfigError::UnknownBundled(name))),
        };
        let trusted_capabilities = match name.as_str() {
            "admin" => TrustedPluginCapabilities::default()
                .with_authority_provider(TrustedAuthorityProvider::Administration),
            #[cfg(feature = "coverage-probes")]
            "devtools" => TrustedPluginCapabilities::default()
                .with_platform_query(TrustedPlatformQuery::QqGuildApiPermissionList),
            _ => TrustedPluginCapabilities::default(),
        };
        plugins
            .register_trusted_with_capabilities(
                plugin,
                instance_id,
                configs.get(&name).cloned().unwrap_or_default(),
                trusted_capabilities,
            )
            .await?;
        instance_ids.insert(instance_id.to_owned());
    }
    Ok((include_help, instance_ids))
}

#[derive(Debug, Error)]
pub(crate) enum PluginConfigError {
    #[error("unknown bundled plugin `{0}`")]
    UnknownBundled(String),
    #[cfg(not(feature = "coverage-probes"))]
    #[error(
        "bundled plugin `{0}` requires a bkmqb binary built with the `coverage-probes` feature"
    )]
    CoverageProbeUnavailable(String),
    #[error("bundled plugin `{0}` is listed more than once")]
    DuplicateBundled(String),
    #[cfg(feature = "coverage-probes")]
    #[error("diagnostic probes require bundled `admin` as the authority provider")]
    MissingDiagnosticAuthority,
    #[error("plugin instance `{0}` is declared by more than one installation source")]
    DuplicateInstance(String),
    #[error("plugin instance `{0}` is reserved for a bundled plugin")]
    ReservedInstance(String),
    #[error("installed plugin package `{path}` changed: expected SHA-256 {expected}, got {actual}")]
    PackageHashChanged {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("installed plugin package identity changed for instance `{instance_id}`")]
    PackageIdentityChanged { instance_id: String },
    #[error("failed to read plugin installation file `{path}`")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin installation path `{0}` is not a regular file")]
    NotRegularFile(PathBuf),
    #[error("plugin installation file `{path}` is too large ({size} bytes)")]
    TooLarge { path: PathBuf, size: usize },
    #[error("plugin installation file `{path}` is not UTF-8")]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },
    #[error("failed to parse plugin installation file `{path}`")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use plugin_api::{CallerAuthority, HostQueries, PluginEventEnvelope, StateValue, StaticPlugin};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bkm-plugin-installations-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct HelpQueries {
        capabilities: BTreeSet<String>,
    }

    impl HostQueries for HelpQueries {
        fn command_context(&self) -> Option<plugin_api::CommandContext> {
            plugin_api::CommandContext::parse("/", "/help")
        }

        fn config_get(&self, _key: &str) -> Option<&Value> {
            None
        }

        fn state_get(&self, _key: &str) -> Option<&StateValue> {
            None
        }

        fn state_scan(&self, _prefix: &str, _limit: usize) -> Vec<(&str, &StateValue)> {
            Vec::new()
        }

        fn granted_capabilities(&self) -> &BTreeSet<String> {
            &self.capabilities
        }

        fn invocation_time_ms(&self) -> i64 {
            0
        }

        fn caller_authority(&self) -> CallerAuthority {
            CallerAuthority::default()
        }
    }

    fn help_event() -> PluginEventEnvelope {
        PluginEventEnvelope {
            protocol_version: "1.0.0".to_owned(),
            event_id: "help-event".to_owned(),
            delivery_id: "help-delivery".to_owned(),
            invocation_id: "help-invocation".to_owned(),
            occurred_at_ms: None,
            received_at_ms: 0,
            adapter_id: "test".to_owned(),
            event_type: "message.created".to_owned(),
            trace_id: None,
            payload: serde_json::json!({
                "type": "message",
                "data": {
                    "text": "/help",
                    "sender": {"id": "user-id"},
                    "target": {"scope": "group", "group_id": "group-id"}
                }
            }),
            extensions: Vec::new(),
        }
    }

    #[test]
    fn parses_explicit_wasm_installation_and_resolves_relative_package() {
        let installations: PluginInstallations = toml::from_str(
            r#"
                [[wasm]]
                package = "plugins/example.bkm-plugin"
                instance_id = "dev.bkm.example/default"
                grants = ["message.reply"]

                [wasm.config]
                greeting = "hello"
            "#,
        )
        .unwrap();

        let installation = &installations.wasm[0];
        assert!(installation.enabled);
        assert!(installation.grants.contains("message.reply"));
        assert_eq!(installation.config["greeting"], "hello");
        assert_eq!(
            installation.resolved_package(Path::new("/srv/bkm/plugins.toml")),
            Path::new("/srv/bkm/plugins/example.bkm-plugin")
        );
    }

    #[test]
    fn default_bundled_plugin_order_is_stable() {
        assert_eq!(
            DEFAULT_BUNDLED_PLUGINS,
            ["ping", "help", "echo", "counter", "reminder"]
        );
    }

    #[tokio::test]
    async fn default_registration_contains_only_product_plugins() {
        let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
        let (include_help, instances) =
            register_bundled_plugins(&mut plugins, None, &BTreeMap::new())
                .await
                .unwrap();
        assert!(include_help);
        register_help_plugin(&mut plugins).await.unwrap();

        assert_eq!(
            instances,
            BTreeSet::from([
                "dev.bkm.ping/default".to_owned(),
                "dev.bkm.help/default".to_owned(),
                "dev.bkm.echo/default".to_owned(),
                "dev.bkm.counter/default".to_owned(),
                "dev.bkm.reminder/default".to_owned(),
            ])
        );
        let help = HelpPlugin::with_commands(plugins.command_declarations().unwrap());
        let output = help
            .on_event(
                &help_event(),
                &HelpQueries {
                    capabilities: BTreeSet::new(),
                },
            )
            .await
            .unwrap();
        let content = output.commands[0].payload["content"].as_str().unwrap();
        for command in ["/ping", "/help", "/echo", "/count", "/remind"] {
            assert!(content.contains(command), "default help omitted {command}");
        }
        for probe in ["/whoami", "/api-permissions", "/screenshot", "/active-send"] {
            assert!(!content.contains(probe), "default help exposed {probe}");
        }
    }

    #[tokio::test]
    #[cfg(feature = "coverage-probes")]
    async fn smoke_template_registers_every_selected_plugin() {
        let installations: PluginInstallations =
            toml::from_str(include_str!("../config/examples/plugins.qq-smoke.toml")).unwrap();
        let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
        let (include_help, instances) = register_bundled_plugins(
            &mut plugins,
            installations.bundled,
            &installations.bundled_config,
        )
        .await
        .unwrap();
        assert!(include_help);
        register_help_plugin(&mut plugins).await.unwrap();
        assert_eq!(instances.len(), BUNDLED_PLUGIN_INSTANCES.len());
        for (_, instance_id) in BUNDLED_PLUGIN_INSTANCES {
            assert!(
                instances.contains(*instance_id),
                "smoke profile omitted {instance_id}"
            );
        }
    }

    #[tokio::test]
    #[cfg(feature = "coverage-probes")]
    async fn diagnostic_probes_require_an_authority_provider() {
        for &name in COVERAGE_PROBE_NAMES {
            let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
            let error = register_bundled_plugins(
                &mut plugins,
                Some(vec![name.to_owned()]),
                &BTreeMap::new(),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("authority provider"),
                "standalone probe {name} did not report its authority dependency"
            );
        }
    }

    #[tokio::test]
    async fn admin_remains_explicitly_selectable_with_owner_config() {
        let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
        let configs = BTreeMap::from([(
            "admin".to_owned(),
            BTreeMap::from([("owners".to_owned(), serde_json::json!(["owner-id"]))]),
        )]);
        let (_, instances) =
            register_bundled_plugins(&mut plugins, Some(vec!["admin".to_owned()]), &configs)
                .await
                .unwrap();

        assert_eq!(
            instances,
            BTreeSet::from(["dev.bkm.admin/default".to_owned()])
        );
        assert_eq!(
            plugins
                .instance_manifest("dev.bkm.admin/default")
                .unwrap()
                .id
                .to_string(),
            "dev.bkm.admin"
        );
    }

    #[tokio::test]
    #[cfg(not(feature = "coverage-probes"))]
    async fn ordinary_build_rejects_coverage_probe_selection() {
        for name in COVERAGE_PROBE_NAMES {
            let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
            let error = register_bundled_plugins(
                &mut plugins,
                Some(vec![(*name).to_owned()]),
                &BTreeMap::new(),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("`coverage-probes` feature"),
                "ordinary build did not reject {name} with an actionable error"
            );
        }
    }

    #[tokio::test]
    #[cfg(feature = "coverage-probes")]
    async fn bundled_selection_can_replace_static_ping() {
        let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
        let configs = BTreeMap::from([(
            "admin".to_owned(),
            BTreeMap::from([("owners".to_owned(), serde_json::json!(["owner-id"]))]),
        )]);
        let (include_help, instance_ids) = register_bundled_plugins(
            &mut plugins,
            Some(vec![
                "help".to_owned(),
                "counter".to_owned(),
                "http-probe".to_owned(),
                "scheduler-probe".to_owned(),
                "qq-extension-probe".to_owned(),
                "active-send-probe".to_owned(),
                "admin".to_owned(),
                "reminder".to_owned(),
            ]),
            &configs,
        )
        .await
        .unwrap();
        assert!(include_help);
        assert!(instance_ids.contains("dev.bkm.help/default"));
        register_help_plugin(&mut plugins).await.unwrap();

        assert!(plugins.instance_manifest("dev.bkm.ping/default").is_none());
        assert!(plugins.instance_manifest("dev.bkm.help/default").is_some());
        assert!(
            plugins
                .instance_manifest("dev.bkm.counter/default")
                .is_some()
        );
        assert!(
            plugins
                .instance_manifest("dev.bkm.http-probe/default")
                .is_some()
        );
        assert!(
            plugins
                .instance_manifest("dev.bkm.scheduler-probe/default")
                .is_some()
        );
        assert!(
            plugins
                .instance_manifest("dev.bkm.qq-extension-probe/default")
                .is_some()
        );
        assert!(
            plugins
                .instance_manifest("dev.bkm.active-send-probe/default")
                .is_some()
        );
        assert!(plugins.instance_manifest("dev.bkm.admin/default").is_some());
        assert!(
            plugins
                .instance_manifest("dev.bkm.reminder/default")
                .is_some()
        );
    }

    #[tokio::test]
    #[cfg(feature = "coverage-probes")]
    async fn bundled_admin_registers_before_every_diagnostic_consumer() {
        let configs = BTreeMap::from([(
            "admin".to_owned(),
            BTreeMap::from([("owners".to_owned(), serde_json::json!(["admin-owner"]))]),
        )]);
        for &name in COVERAGE_PROBE_NAMES {
            let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
            register_bundled_plugins(
                &mut plugins,
                Some(vec![name.to_owned(), "admin".to_owned()]),
                &configs,
            )
            .await
            .unwrap();
            // Registration recovers pending deliveries immediately. The authority
            // provider must be available before any diagnostic consumer registers.
            let commands = plugins.command_declarations().unwrap();
            assert_eq!(commands[0].name, "admin", "{name} recovered before admin");
        }
    }

    #[tokio::test]
    async fn installation_file_loads_validated_wasm_component() {
        let directory = TestDirectory::new();
        let package_path = directory.0.join("wasm-ping.bkm-plugin");
        let package = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        fs::write(&package_path, package).unwrap();
        let installation_file = directory.0.join("plugins.toml");
        fs::write(
            &installation_file,
            r#"
                bundled = ["help", "echo", "counter"]

                [[wasm]]
                package = "wasm-ping.bkm-plugin"
                instance_id = "dev.bkm.wasm-ping/default"
                grants = ["message.reply"]
            "#,
        )
        .unwrap();

        let store = plugin_host::PluginStore::in_memory().unwrap();
        let mut plugins = StaticPluginHost::new(store.clone());
        load_plugins(&mut plugins, &store, Some(&installation_file))
            .await
            .unwrap();

        assert!(plugins.instance_manifest("dev.bkm.ping/default").is_none());
        assert_eq!(
            plugins
                .instance_manifest("dev.bkm.wasm-ping/default")
                .unwrap()
                .id
                .to_string(),
            "dev.bkm.wasm-ping"
        );
    }

    #[tokio::test]
    async fn reserved_bundled_instance_id_is_rejected_before_package_loading() {
        let directory = TestDirectory::new();
        let installation_file = directory.0.join("plugins.toml");
        fs::write(
            &installation_file,
            r#"
                bundled = ["help"]

                [[wasm]]
                package = "missing.bkm-plugin"
                instance_id = "dev.bkm.help/default"
            "#,
        )
        .unwrap();
        let store = PluginStore::in_memory().unwrap();
        let mut plugins = StaticPluginHost::new(store.clone());
        let error = load_plugins(&mut plugins, &store, Some(&installation_file))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("reserved for a bundled plugin"));
    }

    #[tokio::test]
    async fn managed_installation_rejects_package_hash_drift() {
        let directory = TestDirectory::new();
        let package_path = directory.0.join("wasm-ping.bkm-plugin");
        let package = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        fs::write(&package_path, package).unwrap();
        let store = plugin_host::PluginStore::in_memory().unwrap();
        store
            .upsert_installation(&plugin_host::PluginInstallation {
                plugin_id: "dev.bkm.wasm-ping".to_owned(),
                metadata: plugin_api::PluginMetadata::single_locale(
                    "en",
                    "WASM Ping",
                    "WASM ping fixture",
                ),
                instance_id: "dev.bkm.wasm-ping/default".to_owned(),
                version: "0.1.0".to_owned(),
                package_path: package_path.display().to_string(),
                package_sha256: "changed".to_owned(),
                source: "local".to_owned(),
                trust_level: "local-wasm".to_owned(),
                signature_status: "unsigned".to_owned(),
                requested_permissions: vec!["message.reply".to_owned()],
                granted_permissions: vec!["message.reply".to_owned()],
                config: BTreeMap::new(),
                enabled: true,
                installed_at_ms: 1,
                updated_at_ms: 1,
            })
            .unwrap();
        let mut plugins = StaticPluginHost::new(store.clone());
        let error = load_plugins(&mut plugins, &store, None).await.unwrap_err();
        assert!(error.to_string().contains("changed"));
    }

    #[tokio::test]
    async fn disabled_managed_installation_does_not_block_startup() {
        let store = plugin_host::PluginStore::in_memory().unwrap();
        store
            .upsert_installation(&plugin_host::PluginInstallation {
                plugin_id: "dev.bkm.incomplete".to_owned(),
                metadata: plugin_api::PluginMetadata::single_locale(
                    "en",
                    "Incomplete",
                    "Incomplete plugin",
                ),
                instance_id: "dev.bkm.incomplete/default".to_owned(),
                version: "0.1.0".to_owned(),
                package_path: "/missing/incomplete.bkm-plugin".to_owned(),
                package_sha256: "missing".to_owned(),
                source: "marketplace".to_owned(),
                trust_level: "local-wasm".to_owned(),
                signature_status: "unsigned".to_owned(),
                requested_permissions: vec!["message.reply".to_owned()],
                granted_permissions: Vec::new(),
                config: BTreeMap::new(),
                enabled: false,
                installed_at_ms: 1,
                updated_at_ms: 1,
            })
            .unwrap();
        let mut plugins = StaticPluginHost::new(store.clone());
        load_plugins(&mut plugins, &store, None).await.unwrap();
        assert!(
            plugins
                .instance_manifest("dev.bkm.incomplete/default")
                .is_none()
        );
    }

    #[tokio::test]
    async fn managed_installation_cannot_claim_a_bundled_instance_id() {
        let store = plugin_host::PluginStore::in_memory().unwrap();
        store
            .upsert_installation(&plugin_host::PluginInstallation {
                plugin_id: "dev.example.impostor".to_owned(),
                metadata: plugin_api::PluginMetadata::single_locale(
                    "en",
                    "Impostor",
                    "Impostor plugin",
                ),
                instance_id: "dev.bkm.admin/default".to_owned(),
                version: "0.1.0".to_owned(),
                package_path: "/missing/impostor.bkm-plugin".to_owned(),
                package_sha256: "missing".to_owned(),
                source: "marketplace".to_owned(),
                trust_level: "local-wasm".to_owned(),
                signature_status: "unsigned".to_owned(),
                requested_permissions: Vec::new(),
                granted_permissions: Vec::new(),
                config: BTreeMap::new(),
                enabled: true,
                installed_at_ms: 1,
                updated_at_ms: 1,
            })
            .unwrap();
        let mut plugins = StaticPluginHost::new(store.clone());

        let error = load_plugins(&mut plugins, &store, None).await.unwrap_err();

        assert!(error.to_string().contains("reserved for a bundled plugin"));
    }

    #[tokio::test]
    async fn managed_installation_loads_from_sqlite_registry() {
        let directory = TestDirectory::new();
        let package_path = directory.0.join("wasm-ping.bkm-plugin");
        let package = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        fs::write(&package_path, package).unwrap();
        let validated = ValidatedPluginPackage::from_path(&package_path).unwrap();
        let store = plugin_host::PluginStore::in_memory().unwrap();
        store
            .upsert_installation(&plugin_host::PluginInstallation {
                plugin_id: "dev.bkm.wasm-ping".to_owned(),
                metadata: plugin_api::PluginMetadata::single_locale(
                    "en",
                    "WASM Ping",
                    "WASM ping fixture",
                ),
                instance_id: "dev.bkm.wasm-ping/managed".to_owned(),
                version: "0.1.0".to_owned(),
                package_path: package_path.display().to_string(),
                package_sha256: validated.package_sha256().to_owned(),
                source: "local".to_owned(),
                trust_level: "local-wasm".to_owned(),
                signature_status: "unsigned".to_owned(),
                requested_permissions: vec!["message.reply".to_owned()],
                granted_permissions: vec!["message.reply".to_owned()],
                config: BTreeMap::new(),
                enabled: true,
                installed_at_ms: 1,
                updated_at_ms: 1,
            })
            .unwrap();
        let installation_file = directory.0.join("plugins.toml");
        fs::write(
            &installation_file,
            "bundled = [\"help\", \"echo\", \"counter\"]\n",
        )
        .unwrap();
        let mut plugins = StaticPluginHost::new(store.clone());
        load_plugins(&mut plugins, &store, Some(&installation_file))
            .await
            .unwrap();
        assert!(
            plugins
                .instance_manifest("dev.bkm.wasm-ping/managed")
                .is_some()
        );
    }
}
