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

use builtin_plugins::{
    ActiveSendProbePlugin, CounterPlugin, EchoPlugin, HelpPlugin, HttpProbePlugin, PingPlugin,
    QqExtensionProbePlugin, SchedulerProbePlugin,
};
use plugin_host::{StaticPluginHost, ValidatedPluginPackage, WasmPlugin};

const MAX_INSTALLATION_FILE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginInstallations {
    bundled: Option<Vec<String>>,
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

pub(crate) async fn load_plugins(
    plugins: &mut StaticPluginHost,
    installation_file: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let installations = installation_file.map_or_else(
        || Ok(PluginInstallations::default()),
        PluginInstallations::from_path,
    )?;
    let include_help = register_bundled_plugins(plugins, installations.bundled).await?;
    for installation in installations.wasm {
        if !installation.enabled {
            info!(
                instance_id = installation.instance_id,
                "WASM plugin is disabled"
            );
            continue;
        }
        let package_path = installation.resolved_package(
            installation_file.expect("WASM declarations require an installation file"),
        );
        let package = ValidatedPluginPackage::from_path(&package_path)?;
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
    if include_help {
        register_help_plugin(plugins).await?;
    }
    Ok(())
}

async fn register_help_plugin(
    plugins: &mut StaticPluginHost,
) -> Result<(), plugin_host::PluginHostError> {
    let help = HelpPlugin::with_commands(plugins.command_declarations());
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
) -> Result<bool, Box<dyn std::error::Error>> {
    let selected = selected.unwrap_or_else(|| {
        ["ping", "help", "echo", "counter"]
            .map(str::to_owned)
            .to_vec()
    });
    let mut seen = BTreeSet::new();
    let mut include_help = false;
    for name in selected {
        if !seen.insert(name.clone()) {
            return Err(Box::new(PluginConfigError::DuplicateBundled(name)));
        }
        if name == "help" {
            include_help = true;
            continue;
        }
        let (plugin, instance_id): (std::sync::Arc<dyn plugin_api::StaticPlugin>, &str) =
            match name.as_str() {
                "ping" => (
                    std::sync::Arc::new(PingPlugin::default()),
                    "dev.bkm.ping/default",
                ),
                "echo" => (
                    std::sync::Arc::new(EchoPlugin::default()),
                    "dev.bkm.echo/default",
                ),
                "counter" => (
                    std::sync::Arc::new(CounterPlugin::default()),
                    "dev.bkm.counter/default",
                ),
                "http-probe" => (
                    std::sync::Arc::new(HttpProbePlugin::default()),
                    "dev.bkm.http-probe/default",
                ),
                "scheduler-probe" => (
                    std::sync::Arc::new(SchedulerProbePlugin::default()),
                    "dev.bkm.scheduler-probe/default",
                ),
                "qq-extension-probe" => (
                    std::sync::Arc::new(QqExtensionProbePlugin::default()),
                    "dev.bkm.qq-extension-probe/default",
                ),
                "active-send-probe" => (
                    std::sync::Arc::new(ActiveSendProbePlugin::default()),
                    "dev.bkm.active-send-probe/default",
                ),
                _ => return Err(Box::new(PluginConfigError::UnknownBundled(name))),
            };
        plugins
            .register_trusted(plugin, instance_id, BTreeMap::new())
            .await?;
    }
    Ok(include_help)
}

#[derive(Debug, Error)]
enum PluginConfigError {
    #[error("unknown bundled plugin `{0}`")]
    UnknownBundled(String),
    #[error("bundled plugin `{0}` is listed more than once")]
    DuplicateBundled(String),
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

    #[tokio::test]
    async fn bundled_selection_can_replace_static_ping() {
        let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
        let include_help = register_bundled_plugins(
            &mut plugins,
            Some(vec![
                "help".to_owned(),
                "counter".to_owned(),
                "http-probe".to_owned(),
                "scheduler-probe".to_owned(),
                "qq-extension-probe".to_owned(),
                "active-send-probe".to_owned(),
            ]),
        )
        .await
        .unwrap();
        assert!(include_help);
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

        let mut plugins = StaticPluginHost::new(plugin_host::PluginStore::in_memory().unwrap());
        load_plugins(&mut plugins, Some(&installation_file))
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
}
