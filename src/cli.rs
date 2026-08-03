//! `bkmqb` command-line management interface.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    fs::{File, OpenOptions},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt as _;
use plugin_host::{
    PluginInstallation, PluginStore, StaticPluginHost, ValidatedPluginPackage, WasmPlugin,
};
use serde_json::Value;

use crate::{config::BotConfig, plugin_dev};

const MAX_PLUGIN_PACKAGE_BYTES: usize = 32 * 1024 * 1024;

pub(crate) async fn run(arguments: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    match command {
        "help" | "--help" | "-h" => print_help(),
        "version" | "--version" | "-V" => println!("bkmqb {}", env!("CARGO_PKG_VERSION")),
        "config" => run_config(&arguments[1..])?,
        "plugin" => run_plugin(&arguments[1..]).await?,
        other => return Err(format!("unknown bkmqb command `{other}`; run `bkmqb help`").into()),
    }
    Ok(())
}

fn run_config(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().map(String::as_str) {
        Some("check") if arguments.len() == 1 => {
            // This command runs before any long-lived bot services start, so
            // keep synchronous filesystem work on the CLI path instead of
            // creating a non-cancellable Tokio blocking task.
            BotConfig::check()?;
            println!("configuration is valid; changes require a restart");
            Ok(())
        }
        Some("help" | "--help" | "-h") | None => {
            print_config_help();
            Ok(())
        }
        Some(other) => {
            Err(format!("unknown config command `{other}`; run `bkmqb config help`").into())
        }
    }
}

async fn run_plugin(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_plugin_help();
        return Ok(());
    };
    if command == "inspect" {
        require_argument_count(arguments, 2, "bkmqb plugin inspect <package>")?;
        let package = required_argument(arguments, 1, "package path")?;
        inspect_package(Path::new(package), &management_locale_from_environment()).await?;
        return Ok(());
    }
    if matches!(command, "new" | "check" | "build" | "package") {
        plugin_dev::run(command, &arguments[1..]).await?;
        return Ok(());
    }

    let config = BotConfig::load()?;
    if let Some(parent) = config.plugins.database.parent() {
        fs::create_dir_all(parent)?;
    }
    let store = PluginStore::open(&config.plugins.database)?;
    let locale = config.logging.console.language.as_str();
    match command {
        "install" => {
            install_or_update(&store, &config.plugins.database, arguments, false, locale).await?;
        }
        "update" => {
            install_or_update(&store, &config.plugins.database, arguments, true, locale).await?;
        }
        "list" => {
            require_argument_count(arguments, 1, "bkmqb plugin list")?;
            list_plugins(&store, locale)?;
        }
        "info" => {
            require_argument_count(arguments, 2, "bkmqb plugin info <instance-id>")?;
            info_plugin(
                &store,
                required_argument(arguments, 1, "instance ID")?,
                locale,
            )?;
        }
        "enable" => {
            require_argument_count(arguments, 2, "bkmqb plugin enable <instance-id>")?;
            set_enabled(
                &store,
                required_argument(arguments, 1, "instance ID")?,
                true,
            )?;
        }
        "disable" => {
            require_argument_count(arguments, 2, "bkmqb plugin disable <instance-id>")?;
            set_enabled(
                &store,
                required_argument(arguments, 1, "instance ID")?,
                false,
            )?;
        }
        "remove" => {
            require_argument_count(arguments, 2, "bkmqb plugin remove <instance-id>")?;
            remove_plugin(
                &store,
                &config.plugins.database,
                required_argument(arguments, 1, "instance ID")?,
            )?;
        }
        "recover" => {
            require_argument_count(arguments, 2, "bkmqb plugin recover <instance-id>")?;
            recover_plugin(&store, required_argument(arguments, 1, "instance ID")?)?;
        }
        "dead-letter" => dead_letter_command(&store, &arguments[1..])?,
        "help" | "--help" | "-h" => print_plugin_help(),
        other => {
            return Err(
                format!("unknown plugin command `{other}`; run `bkmqb plugin help`").into(),
            );
        }
    }
    Ok(())
}

async fn inspect_package(path: &Path, locale: &str) -> Result<(), Box<dyn std::error::Error>> {
    let package = ValidatedPluginPackage::from_path(path)?;
    let manifest = package.manifest();
    let metadata = manifest.metadata.resolve(locale)?;
    println!("Plugin: {} {}", metadata.name, manifest.version);
    if !metadata.description.is_empty() {
        println!("Description: {}", metadata.description);
    }
    println!("ID: {}", manifest.id);
    println!("Protocol: {}", manifest.protocol);
    println!("SHA-256: {}", package.package_sha256());
    println!("Trust: local-wasm (unsigned)");
    println!("Requested capabilities:");
    for capability in manifest.requested_capabilities() {
        println!("  - {capability}");
    }
    WasmPlugin::from_package(package).await?;
    println!("Component ABI: valid");
    Ok(())
}

async fn install_or_update(
    store: &PluginStore,
    database: &Path,
    arguments: &[String],
    update: bool,
    locale: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let source = PathBuf::from(required_argument(arguments, 1, "package path")?);
    let options = InstallOptions::parse(&arguments[2..])?;
    let package_bytes = read_bounded_plugin_package(&source)?;
    let package = ValidatedPluginPackage::from_bytes(&package_bytes)?;
    let manifest = package.manifest().clone();
    let package_sha256 = package.package_sha256().to_owned();
    let instance_id = options
        .instance_id
        .clone()
        .unwrap_or_else(|| format!("{}/default", manifest.id));
    if is_reserved_bundled_instance_id(&instance_id) {
        return Err(format!(
            "plugin instance `{instance_id}` is reserved for a bundled plugin; use a different instance ID"
        )
        .into());
    }
    let existing = store.installation(&instance_id)?;
    if update && existing.is_none() {
        return Err(format!("plugin instance `{instance_id}` is not installed").into());
    }
    if !update && existing.is_some() {
        return Err(format!(
            "plugin instance `{instance_id}` is already installed; use `bkmqb plugin update`"
        )
        .into());
    }
    if existing
        .as_ref()
        .is_some_and(|installed| installed.plugin_id != manifest.id.as_str())
    {
        return Err("an update cannot change plugin ID".into());
    }

    let requested = manifest.requested_capabilities();
    let granted = resolve_grants(&requested, existing.as_ref(), &options)?;
    let instance_config = merged_installation_config(existing.as_ref(), &options, update);
    print_install_summary(
        if update { "Update" } else { "Install" },
        &manifest,
        &instance_id,
        &package_sha256,
        &requested,
        &granted,
        locale,
    )?;
    if !options.yes && !confirm("Proceed with this local unsigned plugin?")? {
        return Err("installation cancelled".into());
    }

    let plugin = Arc::new(WasmPlugin::from_package(package).await?);
    let mut validation_host = StaticPluginHost::new(PluginStore::in_memory()?);
    validation_host
        .register(
            plugin,
            instance_id.clone(),
            instance_config.clone(),
            granted.clone(),
        )
        .await?;
    validation_host.shutdown().await?;

    let _lifecycle_lock = lock_managed_package_lifecycle(database)?;
    let destination =
        persist_installed_package(database, &manifest, &package_sha256, &package_bytes)?;
    let now = next_installation_timestamp(existing.as_ref());
    let expected_updated_at_ms = existing.as_ref().map(|item| item.updated_at_ms);
    let installation = PluginInstallation {
        plugin_id: manifest.id.to_string(),
        metadata: manifest.metadata,
        instance_id: instance_id.clone(),
        version: manifest.version,
        package_path: destination.display().to_string(),
        package_sha256,
        source: source.display().to_string(),
        trust_level: "local-wasm".to_owned(),
        signature_status: "unsigned".to_owned(),
        requested_permissions: requested.into_iter().collect(),
        granted_permissions: granted.into_iter().collect(),
        config: instance_config,
        enabled: existing.as_ref().is_none_or(|item| item.enabled),
        installed_at_ms: existing.as_ref().map_or(now, |item| item.installed_at_ms),
        updated_at_ms: now,
    };
    write_installation_and_cleanup(
        store,
        database,
        &installation,
        expected_updated_at_ms,
        existing.as_ref(),
    )?;
    println!(
        "{} `{instance_id}`. Restart bkmqb to load the plugin.",
        if update { "Updated" } else { "Installed" }
    );
    Ok(())
}

fn read_bounded_plugin_package(source: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut file = File::open(source)?;
    if !file.metadata()?.is_file() {
        return Err(format!(
            "plugin package `{}` must be a regular file",
            source.display()
        )
        .into());
    }
    let mut bytes = Vec::with_capacity(MAX_PLUGIN_PACKAGE_BYTES.min(64 * 1024));
    std::io::Read::by_ref(&mut file)
        .take(u64::try_from(MAX_PLUGIN_PACKAGE_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PLUGIN_PACKAGE_BYTES {
        return Err(format!(
            "plugin package `{}` exceeds {} bytes",
            source.display(),
            MAX_PLUGIN_PACKAGE_BYTES
        )
        .into());
    }
    Ok(bytes)
}

fn merged_installation_config(
    existing: Option<&PluginInstallation>,
    options: &InstallOptions,
    update: bool,
) -> BTreeMap<String, Value> {
    let mut config = if update {
        existing.map_or_else(BTreeMap::new, |installed| installed.config.clone())
    } else {
        BTreeMap::new()
    };
    config.extend(options.config.clone());
    config
}

fn next_installation_timestamp(existing: Option<&PluginInstallation>) -> i64 {
    existing.map_or_else(now_ms, |item| {
        now_ms().max(item.updated_at_ms.saturating_add(1))
    })
}

fn write_installation_and_cleanup(
    store: &PluginStore,
    database: &Path,
    installation: &PluginInstallation,
    expected_updated_at_ms: Option<i64>,
    previous: Option<&PluginInstallation>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(error) = store.write_installation(installation, expected_updated_at_ms) {
        let _ = cleanup_unreferenced_managed_package(
            store,
            database,
            Path::new(&installation.package_path),
        );
        return Err(error.into());
    }
    if let Some(previous) = previous {
        if previous.package_path != installation.package_path {
            let _ = cleanup_unreferenced_managed_package(
                store,
                database,
                Path::new(&previous.package_path),
            );
        }
    }
    Ok(())
}

fn persist_installed_package(
    database: &Path,
    manifest: &plugin_api::PluginManifest,
    package_sha256: &str,
    package_bytes: &[u8],
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let managed_directory = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plugins");
    fs::create_dir_all(&managed_directory)?;
    let destination = managed_directory.join(format!(
        "{}-{}-{package_sha256}.bkm-plugin",
        manifest.id, manifest.version,
    ));
    persist_managed_package(&destination, package_bytes, package_sha256)?;
    Ok(fs::canonicalize(destination)?)
}

fn persist_managed_package(
    destination: &Path,
    package_bytes: &[u8],
    expected_sha256: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "managed plugin destination `{}` must be a regular file",
                destination.display()
            )
            .into());
        }
    }
    let directory = destination.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = None;
    for nonce in 0..100_u32 {
        let candidate = directory.join(format!(
            ".bkmqb-install-{}-{}-{nonce}.tmp.bkm-plugin",
            std::process::id(),
            now_ms()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let Some((temporary_path, mut file)) = temporary else {
        return Err("could not allocate a temporary managed plugin path".into());
    };
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        file.write_all(package_bytes)?;
        file.sync_all()?;
        drop(file);
        let persisted = ValidatedPluginPackage::from_path(&temporary_path)?;
        if persisted.package_sha256() != expected_sha256 {
            return Err("managed plugin package hash changed while writing".into());
        }
        replace_managed_file(&temporary_path, destination)?;
        sync_directory(directory)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), io::Error> {
    File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), io::Error> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_managed_file(source: &Path, destination: &Path) -> Result<(), io::Error> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_managed_file(source: &Path, destination: &Path) -> Result<(), io::Error> {
    if !destination.exists() {
        return fs::rename(source, destination);
    }
    let backup =
        destination.with_extension(format!("bkmqb-backup-{}-{}", std::process::id(), now_ms()));
    fs::rename(destination, &backup)?;
    if let Err(error) = fs::rename(source, destination) {
        let _ = fs::rename(&backup, destination);
        return Err(error);
    }
    fs::remove_file(backup)
}

fn resolve_grants(
    requested: &BTreeSet<String>,
    existing: Option<&PluginInstallation>,
    options: &InstallOptions,
) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let mut granted = if let Some(existing) = existing {
        existing
            .granted_permissions
            .iter()
            .filter(|capability| requested.contains(*capability))
            .cloned()
            .collect()
    } else if options.grants.is_empty() {
        requested.clone()
    } else {
        BTreeSet::new()
    };
    granted.extend(options.grants.iter().cloned());
    if let Some(unsupported) = granted.iter().find(|grant| !requested.contains(*grant)) {
        return Err(
            format!("capability `{unsupported}` was not requested by the plugin manifest").into(),
        );
    }
    if let Some(existing) = existing {
        let old_requested = existing
            .requested_permissions
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let new_permissions = requested.difference(&old_requested).collect::<Vec<_>>();
        let unapproved_permissions = new_permissions
            .into_iter()
            .filter(|capability| !options.grants.contains(*capability))
            .collect::<Vec<_>>();
        if !unapproved_permissions.is_empty() {
            return Err(format!(
                "plugin update requests new capabilities: {}; approve them with repeated `--grant` options",
                unapproved_permissions
                    .iter()
                    .map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        }
    }
    Ok(granted)
}

fn list_plugins(store: &PluginStore, locale: &str) -> Result<(), Box<dyn std::error::Error>> {
    let installations = store.installations()?;
    if installations.is_empty() {
        println!("No external plugins installed.");
        return Ok(());
    }
    for plugin in installations {
        let metadata = plugin.metadata.resolve(locale)?;
        println!(
            "{}\t{}\t{}\t{}\t{}",
            metadata.name,
            plugin.instance_id,
            plugin.version,
            if plugin.enabled {
                "enabled"
            } else {
                "disabled"
            },
            plugin.trust_level
        );
    }
    Ok(())
}

fn info_plugin(
    store: &PluginStore,
    instance_id: &str,
    locale: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let plugin = store
        .installation(instance_id)?
        .ok_or_else(|| format!("plugin instance `{instance_id}` is not installed"))?;
    let metadata = plugin.metadata.resolve(locale)?;
    println!("Plugin ID: {}", plugin.plugin_id);
    println!("Name: {}", metadata.name);
    if !metadata.description.is_empty() {
        println!("Description: {}", metadata.description);
    }
    println!("Instance ID: {}", plugin.instance_id);
    println!("Version: {}", plugin.version);
    println!("Enabled: {}", plugin.enabled);
    println!(
        "Trust: {} ({})",
        plugin.trust_level, plugin.signature_status
    );
    println!("Package: {}", plugin.package_path);
    println!("SHA-256: {}", plugin.package_sha256);
    println!("Granted capabilities:");
    for capability in plugin.granted_permissions {
        println!("  - {capability}");
    }
    Ok(())
}

fn set_enabled(
    store: &PluginStore,
    instance_id: &str,
    enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !store.set_installation_enabled(instance_id, enabled, now_ms())? {
        return Err(format!("plugin instance `{instance_id}` is not installed").into());
    }
    println!(
        "Plugin `{instance_id}` {}. Restart bkmqb to apply.",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

fn remove_plugin(
    store: &PluginStore,
    database: &Path,
    instance_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let _lifecycle_lock = lock_managed_package_lifecycle(database)?;
    let installation = store
        .installation(instance_id)?
        .ok_or_else(|| format!("plugin instance `{instance_id}` is not installed"))?;
    if !store.remove_installation_if_updated(instance_id, installation.updated_at_ms)? {
        return Err(plugin_host::StoreError::InstallationConflict.into());
    }
    let _ = cleanup_unreferenced_managed_package(
        store,
        database,
        Path::new(&installation.package_path),
    );
    println!("Removed plugin installation `{instance_id}`; private state was retained.");
    Ok(())
}

fn lock_managed_package_lifecycle(database: &Path) -> Result<File, io::Error> {
    let lock_path = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".bkmqb-plugin-lifecycle.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn cleanup_unreferenced_managed_package(
    store: &PluginStore,
    database: &Path,
    package_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if store
        .installations()?
        .iter()
        .any(|installation| Path::new(&installation.package_path) == package_path)
    {
        return Ok(());
    }
    let managed_directory = database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plugins");
    let Ok(managed_directory) = fs::canonicalize(managed_directory) else {
        return Ok(());
    };
    let Some(parent) = package_path.parent() else {
        return Ok(());
    };
    if fs::canonicalize(parent)? != managed_directory {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(package_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(());
    }
    fs::remove_file(package_path)?;
    Ok(())
}

fn recover_plugin(
    store: &PluginStore,
    instance_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if store.installation(instance_id)?.is_none() {
        return Err(format!("plugin instance `{instance_id}` is not installed").into());
    }
    let recovered = store.recover_dead_letters(instance_id, now_ms())?;
    println!(
        "Recovered {recovered} dead-letter deliveries for `{instance_id}`. Restart bkmqb to retry them."
    );
    Ok(())
}

fn dead_letter_command(
    store: &PluginStore,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.first().map(String::as_str) != Some("list") || arguments.len() > 2 {
        return Err("usage: bkmqb plugin dead-letter list [instance-id]".into());
    }
    let instance_id = arguments.get(1).map(String::as_str);
    let letters = store.dead_letters(instance_id)?;
    if letters.is_empty() {
        println!("No dead-letter deliveries.");
    }
    for letter in letters {
        println!(
            "{}\t{}\tattempts={}\t{}",
            letter.instance_id, letter.event_id, letter.attempts, letter.last_error
        );
    }
    Ok(())
}

fn is_reserved_bundled_instance_id(instance_id: &str) -> bool {
    matches!(
        instance_id,
        "dev.bkm.ping/default"
            | "dev.bkm.help/default"
            | "dev.bkm.echo/default"
            | "dev.bkm.counter/default"
            | "dev.bkm.http-probe/default"
            | "dev.bkm.scheduler-probe/default"
            | "dev.bkm.qq-extension-probe/default"
            | "dev.bkm.active-send-probe/default"
    )
}

#[derive(Debug, Default)]
struct InstallOptions {
    instance_id: Option<String>,
    grants: BTreeSet<String>,
    config: BTreeMap<String, Value>,
    yes: bool,
}

impl InstallOptions {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            match arguments[index].as_str() {
                "--instance" => {
                    index += 1;
                    options.instance_id = Some(
                        arguments
                            .get(index)
                            .ok_or("--instance requires a value")?
                            .clone(),
                    );
                }
                "--grant" => {
                    index += 1;
                    options.grants.insert(
                        arguments
                            .get(index)
                            .ok_or("--grant requires a value")?
                            .clone(),
                    );
                }
                "--config" => {
                    index += 1;
                    let entry = arguments.get(index).ok_or("--config requires key=JSON")?;
                    let (key, value) = entry.split_once('=').ok_or("--config requires key=JSON")?;
                    if key.is_empty() {
                        return Err("configuration key cannot be empty".into());
                    }
                    options
                        .config
                        .insert(key.to_owned(), serde_json::from_str(value)?);
                }
                "--yes" | "-y" => options.yes = true,
                unknown => return Err(format!("unknown install option `{unknown}`").into()),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn print_install_summary(
    operation: &str,
    manifest: &plugin_api::PluginManifest,
    instance_id: &str,
    hash: &str,
    requested: &BTreeSet<String>,
    granted: &BTreeSet<String>,
    locale: &str,
) -> Result<(), plugin_api::ManifestError> {
    let metadata = manifest.metadata.resolve(locale)?;
    println!("{operation}: {} {}", metadata.name, manifest.version);
    println!("Plugin ID: {}", manifest.id);
    println!("Instance ID: {instance_id}");
    println!("SHA-256: {hash}");
    println!("Source trust: local-wasm (unsigned)");
    println!("Requested capabilities:");
    for capability in requested {
        println!(
            "  {} {capability}",
            if granted.contains(capability) {
                "+"
            } else {
                "-"
            }
        );
    }
    Ok(())
}

fn management_locale_from_environment() -> String {
    env::var("BKMQB_LOG_LANGUAGE")
        .ok()
        .and_then(|locale| normalize_locale(&locale))
        .or_else(|| {
            env::var("LANG")
                .ok()
                .and_then(|locale| normalize_locale(&locale))
        })
        .unwrap_or_else(|| "en".to_owned())
}

fn normalize_locale(locale: &str) -> Option<String> {
    let locale = locale
        .split(['.', '@'])
        .next()
        .unwrap_or(locale)
        .replace('_', "-");
    if locale.eq_ignore_ascii_case("c") || locale.eq_ignore_ascii_case("posix") {
        return None;
    }
    let mut segments = locale.split('-');
    let language = segments.next()?.to_ascii_lowercase();
    if !(2..=3).contains(&language.len()) || !language.bytes().all(|byte| byte.is_ascii_lowercase())
    {
        return None;
    }
    let mut normalized = vec![language];
    for segment in segments {
        let segment =
            if segment.len() == 4 && segment.bytes().all(|byte| byte.is_ascii_alphabetic()) {
                let mut characters = segment.chars();
                let first = characters.next()?.to_ascii_uppercase();
                format!("{first}{}", characters.as_str().to_ascii_lowercase())
            } else if (segment.len() == 2 && segment.bytes().all(|byte| byte.is_ascii_alphabetic()))
                || (segment.len() == 3 && segment.bytes().all(|byte| byte.is_ascii_digit()))
            {
                segment.to_ascii_uppercase()
            } else {
                return None;
            };
        normalized.push(segment);
    }
    Some(normalized.join("-"))
}

fn confirm(prompt: &str) -> Result<bool, io::Error> {
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn required_argument<'a>(
    arguments: &'a [String],
    index: usize,
    name: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    arguments
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {name}").into())
}

fn require_argument_count(
    arguments: &[String],
    expected: usize,
    usage: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.len() != expected {
        return Err(format!("usage: {usage}").into());
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn print_help() {
    println!(
        "BroKnowMyQQBot command line\n\nUsage:\n  bkmqb\n  bkmqb config check\n  bkmqb plugin <command>\n  bkmqb version"
    );
}

fn print_config_help() {
    println!("Configuration management\n\nUsage:\n  bkmqb config check");
}

fn print_plugin_help() {
    println!(
        "Plugin management\n\n\
Usage:\n\
  bkmqb plugin new [directory] [--id ID] [--name NAME] [--class TYPE] [--language LANG] [--region REGION] [--description TEXT] [--command NAME] [--no-interactive]\n\
  bkmqb plugin check [directory]\n\
  bkmqb plugin build [directory]\n\
  bkmqb plugin package [directory]\n\
  bkmqb plugin inspect <package>\n\
  bkmqb plugin install <package> [--instance ID] [--grant CAP]... [--config KEY=JSON]... [-y]\n\
  bkmqb plugin update <package> [--instance ID] [--grant CAP]... [--config KEY=JSON]... [-y]\n\
  bkmqb plugin list\n\
  bkmqb plugin info <instance-id>\n\
  bkmqb plugin enable <instance-id>\n\
  bkmqb plugin disable <instance-id>\n\
  bkmqb plugin remove <instance-id>\n\
  bkmqb plugin dead-letter list [instance-id]\n\
  bkmqb plugin recover <instance-id>"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "bkmqb-cli-{}-{}-{}",
                std::process::id(),
                now_ms(),
                TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
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

    #[tokio::test]
    async fn install_validates_component_and_persists_managed_record() {
        let directory = TestDirectory::new();
        let package_path = directory.0.join("source.bkm-plugin");
        let package = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        fs::write(&package_path, package).unwrap();
        let database = directory.0.join("plugins.db");
        let store = PluginStore::open(&database).unwrap();
        let arguments = vec![
            "install".to_owned(),
            package_path.display().to_string(),
            "--yes".to_owned(),
        ];

        install_or_update(&store, &database, &arguments, false, "en")
            .await
            .unwrap();

        let installation = store
            .installation("dev.bkm.wasm-ping/default")
            .unwrap()
            .unwrap();
        assert_eq!(installation.trust_level, "local-wasm");
        assert_eq!(installation.signature_status, "unsigned");
        assert_eq!(installation.granted_permissions, vec!["message.reply"]);
        let managed_package = PathBuf::from(&installation.package_path);
        assert!(managed_package.is_file());
        remove_plugin(&store, &database, "dev.bkm.wasm-ping/default").unwrap();
        assert!(!managed_package.exists());
    }

    #[tokio::test]
    async fn update_preserves_disabled_installation_state() {
        let directory = TestDirectory::new();
        let package_path = directory.0.join("source.bkm-plugin");
        let package = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
            )
            .unwrap();
        fs::write(&package_path, package).unwrap();
        let database = directory.0.join("plugins.db");
        let store = PluginStore::open(&database).unwrap();
        let install_arguments = vec![
            "install".to_owned(),
            package_path.display().to_string(),
            "--yes".to_owned(),
        ];
        install_or_update(&store, &database, &install_arguments, false, "en")
            .await
            .unwrap();
        store
            .set_installation_enabled("dev.bkm.wasm-ping/default", false, now_ms())
            .unwrap();

        let update_arguments = vec![
            "update".to_owned(),
            package_path.display().to_string(),
            "--yes".to_owned(),
        ];
        install_or_update(&store, &database, &update_arguments, true, "en")
            .await
            .unwrap();

        assert!(
            !store
                .installation("dev.bkm.wasm-ping/default")
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn update_rejects_new_permissions_without_explicit_grants() {
        let requested = BTreeSet::from(["message.reply".to_owned(), "message.send".to_owned()]);
        let existing = PluginInstallation {
            plugin_id: "dev.bkm.example".to_owned(),
            metadata: plugin_api::PluginMetadata::single_locale("en", "Example", "Example plugin"),
            instance_id: "dev.bkm.example/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "example.bkm-plugin".to_owned(),
            package_sha256: "abc".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: vec!["message.reply".to_owned()],
            granted_permissions: vec!["message.reply".to_owned()],
            config: BTreeMap::new(),
            enabled: true,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        let error =
            resolve_grants(&requested, Some(&existing), &InstallOptions::default()).unwrap_err();
        assert!(error.to_string().contains("new capabilities"));
    }

    #[test]
    fn update_merges_explicit_config_entries_with_stored_config() {
        let existing = PluginInstallation {
            plugin_id: "dev.bkm.example".to_owned(),
            metadata: plugin_api::PluginMetadata::single_locale("en", "Example", ""),
            instance_id: "dev.bkm.example/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "example.bkm-plugin".to_owned(),
            package_sha256: "abc".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: Vec::new(),
            granted_permissions: Vec::new(),
            config: BTreeMap::from([
                ("kept".to_owned(), serde_json::json!(1)),
                ("changed".to_owned(), serde_json::json!("old")),
            ]),
            enabled: true,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        let options = InstallOptions {
            config: BTreeMap::from([("changed".to_owned(), serde_json::json!("new"))]),
            ..InstallOptions::default()
        };

        assert_eq!(
            merged_installation_config(Some(&existing), &options, true),
            BTreeMap::from([
                ("kept".to_owned(), serde_json::json!(1)),
                ("changed".to_owned(), serde_json::json!("new")),
            ])
        );
    }

    #[test]
    fn update_adds_explicit_new_grant_without_dropping_old_grants() {
        let requested = BTreeSet::from(["message.reply".to_owned(), "message.send".to_owned()]);
        let existing = PluginInstallation {
            plugin_id: "dev.bkm.example".to_owned(),
            metadata: plugin_api::PluginMetadata::single_locale("en", "Example", "Example plugin"),
            instance_id: "dev.bkm.example/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "example.bkm-plugin".to_owned(),
            package_sha256: "abc".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: vec!["message.reply".to_owned()],
            granted_permissions: vec!["message.reply".to_owned()],
            config: BTreeMap::new(),
            enabled: true,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        let options = InstallOptions {
            grants: BTreeSet::from(["message.send".to_owned()]),
            ..InstallOptions::default()
        };
        assert_eq!(
            resolve_grants(&requested, Some(&existing), &options).unwrap(),
            requested
        );
    }

    #[test]
    fn update_requires_each_new_permission_to_be_explicitly_granted() {
        let requested = BTreeSet::from([
            "message.reply".to_owned(),
            "message.send".to_owned(),
            "storage.private".to_owned(),
        ]);
        let existing = PluginInstallation {
            plugin_id: "dev.bkm.example".to_owned(),
            metadata: plugin_api::PluginMetadata::single_locale("en", "Example", ""),
            instance_id: "dev.bkm.example/default".to_owned(),
            version: "0.1.0".to_owned(),
            package_path: "example.bkm-plugin".to_owned(),
            package_sha256: "abc".to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: vec!["message.reply".to_owned()],
            granted_permissions: vec!["message.reply".to_owned()],
            config: BTreeMap::new(),
            enabled: true,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        let options = InstallOptions {
            grants: BTreeSet::from(["message.send".to_owned()]),
            ..InstallOptions::default()
        };
        let error = resolve_grants(&requested, Some(&existing), &options).unwrap_err();
        assert!(error.to_string().contains("storage.private"));
    }

    #[test]
    fn locale_normalization_uses_canonical_bcp47_case() {
        assert_eq!(normalize_locale("zh_CN.UTF-8").as_deref(), Some("zh-CN"));
        assert_eq!(normalize_locale("en_us").as_deref(), Some("en-US"));
        assert_eq!(normalize_locale("ja").as_deref(), Some("ja"));
        assert_eq!(normalize_locale("C"), None);
    }

    #[test]
    fn bundled_instance_ids_are_reserved_from_managed_installations() {
        assert!(is_reserved_bundled_instance_id("dev.bkm.help/default"));
        assert!(!is_reserved_bundled_instance_id("dev.bkm.example/default"));
    }
}
