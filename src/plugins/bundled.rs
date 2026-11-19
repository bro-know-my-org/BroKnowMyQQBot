//! The loader and management API share the same compiled-in plugin catalogue.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use plugin_api::StaticPlugin;
use plugin_host::{TrustedAuthorityProvider, TrustedPluginCapabilities};
use serde_json::Value;

use super::{
    AdminPlugin, BUNDLED_PLUGIN_INSTANCES, COVERAGE_PROBE_NAMES, CounterPlugin,
    DEFAULT_BUNDLED_PLUGINS, EchoPlugin, HelpPlugin, PingPlugin, PluginConfigError,
    PluginInstallations, QqMarkdownPagesPlugin, ReminderPlugin,
};

pub(crate) struct Definition {
    pub(crate) key: &'static str,
    pub(crate) instance_id: &'static str,
    pub(crate) plugin: Arc<dyn StaticPlugin>,
    pub(crate) capabilities: TrustedPluginCapabilities,
}

pub(crate) fn definition(name: &str) -> Result<Definition, PluginConfigError> {
    let &(key, instance_id) = BUNDLED_PLUGIN_INSTANCES
        .iter()
        .find(|(key, _)| *key == name)
        .ok_or_else(|| PluginConfigError::UnknownBundled(name.to_owned()))?;
    let plugin: Arc<dyn StaticPlugin> = match key {
        "ping" => Arc::new(PingPlugin::default()),
        "help" => Arc::new(HelpPlugin::with_commands([])),
        "echo" => Arc::new(EchoPlugin::default()),
        "counter" => Arc::new(CounterPlugin::default()),
        "admin" => Arc::new(AdminPlugin::default()),
        "reminder" => Arc::new(ReminderPlugin::default()),
        "qq-markdown-pages" => Arc::new(QqMarkdownPagesPlugin::default()),
        #[cfg(feature = "coverage-probes")]
        "http-probe" => Arc::new(super::HttpProbePlugin::default()),
        #[cfg(feature = "coverage-probes")]
        "browser-probe" => Arc::new(super::BrowserProbePlugin::default()),
        #[cfg(feature = "coverage-probes")]
        "scheduler-probe" => Arc::new(super::SchedulerProbePlugin::default()),
        #[cfg(feature = "coverage-probes")]
        "qq-extension-probe" => Arc::new(super::QqExtensionProbePlugin::default()),
        #[cfg(feature = "coverage-probes")]
        "active-send-probe" => Arc::new(super::ActiveSendProbePlugin::default()),
        #[cfg(feature = "coverage-probes")]
        "devtools" => Arc::new(super::DevToolsPlugin::default()),
        #[cfg(not(feature = "coverage-probes"))]
        _ if COVERAGE_PROBE_NAMES.contains(&key) => {
            return Err(PluginConfigError::CoverageProbeUnavailable(name.to_owned()));
        }
        _ => return Err(PluginConfigError::UnknownBundled(name.to_owned())),
    };
    let capabilities = match key {
        "admin" => TrustedPluginCapabilities::default()
            .with_authority_provider(TrustedAuthorityProvider::Administration),
        #[cfg(feature = "coverage-probes")]
        "devtools" => TrustedPluginCapabilities::default()
            .with_platform_query(super::TrustedPlatformQuery::QqGuildApiPermissionList),
        _ => TrustedPluginCapabilities::default(),
    };
    Ok(Definition {
        key,
        instance_id,
        plugin,
        capabilities,
    })
}

pub(crate) fn catalogue() -> Vec<Definition> {
    BUNDLED_PLUGIN_INSTANCES
        .iter()
        .filter(|(key, _)| cfg!(feature = "coverage-probes") || !COVERAGE_PROBE_NAMES.contains(key))
        .map(|(key, _)| definition(key).expect("compiled catalogue entries have constructors"))
        .collect()
}

pub(crate) struct Settings {
    pub(crate) selected: Vec<String>,
    pub(crate) configs: BTreeMap<String, BTreeMap<String, Value>>,
}

impl Settings {
    pub(crate) fn parse(source: &str) -> Result<Self, ()> {
        let declarations: PluginInstallations = toml::from_str(source).map_err(|_| ())?;
        let selected = declarations.bundled.unwrap_or_else(|| {
            DEFAULT_BUNDLED_PLUGINS
                .iter()
                .map(|key| (*key).to_owned())
                .collect()
        });
        let mut seen = BTreeSet::new();
        for key in &selected {
            definition(key).map_err(|_| ())?;
            if !seen.insert(key) {
                return Err(());
            }
        }
        if selected
            .iter()
            .any(|key| COVERAGE_PROBE_NAMES.contains(&key.as_str()))
            && !selected.iter().any(|key| key == "admin")
        {
            return Err(());
        }
        Ok(Self {
            selected,
            configs: declarations.bundled_config,
        })
    }
}

/// Check the saved command namespace without initializing guests or replaying work.
pub(crate) async fn validate_commands(
    source: &str,
    path: &std::path::Path,
    store: &plugin_host::PluginStore,
) -> Result<(), ()> {
    fn add(manifest: &plugin_api::PluginManifest, names: &mut BTreeSet<String>) -> Result<(), ()> {
        for command in &manifest.commands {
            for name in std::iter::once(&command.name).chain(&command.aliases) {
                if !names.insert(name.clone()) {
                    return Err(());
                }
            }
        }
        Ok(())
    }
    let declarations: PluginInstallations = toml::from_str(source).map_err(|_| ())?;
    let settings = Settings::parse(source)?;
    let mut commands = BTreeSet::new();
    for key in settings.selected {
        add(
            definition(&key).map_err(|_| ())?.plugin.manifest(),
            &mut commands,
        )?;
    }
    let store = store.clone();
    let records = tokio::task::spawn_blocking(move || store.enabled_installations())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    if records.len() + declarations.wasm.len() > 256 {
        return Err(());
    }
    for record in records {
        let package = super::load_validated_package(record.package_path.clone().into())
            .await
            .map_err(|_| ())?;
        super::validate_managed_package(&record, &package).map_err(|_| ())?;
        add(package.manifest(), &mut commands)?;
    }
    for entry in declarations.wasm.into_iter().filter(|entry| entry.enabled) {
        let package = super::load_validated_package(entry.resolved_package(path))
            .await
            .map_err(|_| ())?;
        add(package.manifest(), &mut commands)?;
    }
    Ok(())
}
