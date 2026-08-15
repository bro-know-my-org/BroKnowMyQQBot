//! `bkmqb` command-line management interface.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fmt::Write as _,
    fs,
    fs::{File, OpenOptions},
    io::{self, IsTerminal as _, Read as _, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt as _;
use plugin_host::{
    PluginHostError, PluginInstallation, PluginStore, ValidatedPluginPackage, WasmPlugin,
    validate_plugin_config,
};
use serde::de::{MapAccess, Visitor};
use serde_json::Value;

use crate::{
    browser,
    config::BotConfig,
    plugin_dev,
    plugin_marketplace::{MarketplaceClient, MarketplacePlugin, validate_selector},
    plugins,
};

const MAX_PLUGIN_PACKAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_PLUGIN_CONFIG_FILE_BYTES: usize = 256 * 1024;

pub(crate) async fn run(arguments: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };
    match command {
        "help" | "--help" | "-h" => print_help(),
        "version" | "--version" | "-V" => println!("bkmqb {}", env!("CARGO_PKG_VERSION")),
        "config" => run_config(&arguments[1..])?,
        "browser" => browser::run(&arguments[1..]).await?,
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
    if command == "marketplace" {
        run_marketplace(&arguments[1..]).await?;
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
            install_or_update(&store, &config, arguments, false, locale).await?;
        }
        "update" => {
            install_or_update(&store, &config, arguments, true, locale).await?;
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

async fn run_marketplace(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(command) = arguments.first().map(String::as_str) else {
        print_marketplace_help();
        return Ok(());
    };
    if matches!(command, "help" | "--help" | "-h") {
        print_marketplace_help();
        return Ok(());
    }
    if !matches!(command, "list" | "info") {
        return Err(
            format!("unknown marketplace command `{command}`; use `list` or `info`").into(),
        );
    }
    let (positionals, override_url) = parse_marketplace_arguments(&arguments[1..])?;
    match command {
        "list" if positionals.len() <= 1 => {}
        "list" => {
            return Err("usage: bkmqb plugin marketplace list [query] [--url URL]".into());
        }
        "info" if positionals.len() == 1 => {}
        "info" => {
            return Err("usage: bkmqb plugin marketplace info <plugin> [--url URL]".into());
        }
        _ => unreachable!("marketplace command was validated above"),
    }
    if command == "info" {
        validate_selector(&positionals[0])?;
    }
    let marketplace_url = if let Some(url) = override_url {
        url
    } else {
        BotConfig::load()?.plugins.marketplace_url
    };
    let client = MarketplaceClient::new(&marketplace_url)?;
    let index = client.fetch_index().await?;
    match command {
        "list" => {
            let query = positionals.first().map_or("", String::as_str);
            let plugins = index.search(query);
            if plugins.is_empty() {
                println!("No marketplace plugins matched `{}`.", terminal_safe(query));
            }
            for plugin in plugins {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    terminal_safe(&plugin.slug),
                    terminal_safe(&plugin.name),
                    terminal_safe(&plugin.latest.version),
                    terminal_safe(&plugin.trust),
                    terminal_safe(&plugin.summary)
                );
            }
        }
        "info" => {
            print_marketplace_plugin(index.find(&positionals[0])?);
        }
        _ => unreachable!("marketplace command was validated above"),
    }
    Ok(())
}

fn parse_marketplace_arguments(
    arguments: &[String],
) -> Result<(Vec<String>, Option<String>), Box<dyn std::error::Error>> {
    let mut positionals = Vec::new();
    let mut url = None;
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--url" {
            index += 1;
            if url.is_some() {
                return Err("--url may only be specified once".into());
            }
            url = Some(
                arguments
                    .get(index)
                    .ok_or("--url requires a value")?
                    .clone(),
            );
        } else if arguments[index].starts_with('-') {
            return Err(format!("unknown marketplace option `{}`", arguments[index]).into());
        } else {
            positionals.push(arguments[index].clone());
        }
        index += 1;
    }
    Ok((positionals, url))
}

fn print_marketplace_plugin(plugin: &MarketplacePlugin) {
    println!(
        "Plugin: {} {}",
        terminal_safe(&plugin.name),
        terminal_safe(&plugin.latest.version)
    );
    println!("Slug: {}", terminal_safe(&plugin.slug));
    println!("Plugin ID: {}", terminal_safe(plugin.plugin_id.as_str()));
    println!("Summary: {}", terminal_safe(&plugin.summary));
    println!("Review label: {}", terminal_safe(&plugin.trust));
    println!("Repository: {}", terminal_safe(&plugin.repository));
    println!("Protocol: {}", terminal_safe(&plugin.latest.protocol));
    println!("Download: {}", terminal_safe(&plugin.latest.download));
    println!("SHA-256: {}", terminal_safe(&plugin.latest.sha256));
    println!("Requested capabilities:");
    for capability in &plugin.capabilities {
        println!("  - {}", terminal_safe(capability));
    }
}

async fn inspect_package(path: &Path, locale: &str) -> Result<(), Box<dyn std::error::Error>> {
    let package = ValidatedPluginPackage::from_path(path)?;
    let manifest = package.manifest();
    let metadata = manifest.metadata.resolve(locale)?;
    println!(
        "Plugin: {} {}",
        terminal_safe(&metadata.name),
        terminal_safe(&manifest.version)
    );
    if !metadata.description.is_empty() {
        println!("Description: {}", terminal_safe(&metadata.description));
    }
    println!("ID: {}", terminal_safe(manifest.id.as_str()));
    println!("Protocol: {}", terminal_safe(&manifest.protocol));
    println!("SHA-256: {}", package.package_sha256());
    println!("Trust: local-wasm (unsigned)");
    println!("Requested capabilities:");
    for capability in manifest.requested_capabilities() {
        println!("  - {}", terminal_safe(&capability));
    }
    WasmPlugin::from_package(package).await?;
    println!("Component ABI: valid");
    Ok(())
}

async fn install_or_update(
    store: &PluginStore,
    bot_config: &BotConfig,
    arguments: &[String],
    update: bool,
    locale: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let source_argument = required_argument(arguments, 1, "package path or marketplace plugin")?;
    let options = InstallOptions::parse(&arguments[2..])?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    validate_install_environment(&options, interactive)?;
    let resolved_source =
        resolve_plugin_source(source_argument, &bot_config.plugins.marketplace_url).await?;
    let package_bytes = resolved_source.bytes;
    let package = ValidatedPluginPackage::from_bytes(&package_bytes)?;
    if let Some(marketplace_plugin) = resolved_source.marketplace_plugin.as_ref() {
        marketplace_plugin.validate_package(&package)?;
    }
    let manifest = package.manifest().clone();
    let package_sha256 = package.package_sha256().to_owned();
    let instance_id = options
        .instance_id
        .clone()
        .unwrap_or_else(|| format!("{}/default", manifest.id));
    if plugins::is_reserved_bundled_instance_id(&instance_id) {
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
    let granted = resolve_grants(&requested, existing.as_ref(), &options, interactive)?;
    let mut instance_config = merged_installation_config(existing.as_ref(), &options, update);
    inherit_global_administrators(
        &package,
        &mut instance_config,
        &options,
        bot_config.plugins.installations.as_deref(),
    )?;
    let permissions_complete = requested.is_subset(&granted);
    print_install_summary(&InstallSummary {
        operation: if update { "Update" } else { "Install" },
        manifest: &manifest,
        instance_id: &instance_id,
        hash: &package_sha256,
        requested: &requested,
        granted: &granted,
        source: &resolved_source.source,
        locale,
    })?;
    if !options.yes && !confirm("Proceed with this unsigned plugin installation?")? {
        return Err("installation cancelled".into());
    }

    let config_error = validate_install_candidate(package, &instance_config).await?;
    let pending = PendingInstallation {
        manifest,
        instance_id,
        package_sha256,
        package_bytes,
        source: resolved_source.source,
        requested,
        granted,
        config: instance_config,
        permissions_complete,
        config_error,
        update,
    };
    finish_installation(
        store,
        &bot_config.plugins.database,
        existing.as_ref(),
        &pending,
    )
}

async fn validate_install_candidate(
    package: ValidatedPluginPackage,
    config: &BTreeMap<String, Value>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let plugin = WasmPlugin::from_package(package).await?;
    match validate_plugin_config(&plugin, config).await {
        Ok(()) => Ok(None),
        Err(PluginHostError::InvalidConfig { message, .. }) => Ok(Some(message)),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug)]
struct PendingInstallation {
    manifest: plugin_api::PluginManifest,
    instance_id: String,
    package_sha256: String,
    package_bytes: Vec<u8>,
    source: String,
    requested: BTreeSet<String>,
    granted: BTreeSet<String>,
    config: BTreeMap<String, Value>,
    permissions_complete: bool,
    config_error: Option<String>,
    update: bool,
}

fn finish_installation(
    store: &PluginStore,
    database: &Path,
    existing: Option<&PluginInstallation>,
    pending: &PendingInstallation,
) -> Result<(), Box<dyn std::error::Error>> {
    let _lifecycle_lock = lock_managed_package_lifecycle(database)?;
    let destination = persist_installed_package(
        database,
        &pending.manifest,
        &pending.package_sha256,
        &pending.package_bytes,
    )?;
    let now = next_installation_timestamp(existing);
    let requirements_complete = pending.permissions_complete && pending.config_error.is_none();
    let installation = PluginInstallation {
        plugin_id: pending.manifest.id.to_string(),
        metadata: pending.manifest.metadata.clone(),
        instance_id: pending.instance_id.clone(),
        version: pending.manifest.version.clone(),
        package_path: destination.display().to_string(),
        package_sha256: pending.package_sha256.clone(),
        source: pending.source.clone(),
        trust_level: "local-wasm".to_owned(),
        signature_status: "unsigned".to_owned(),
        requested_permissions: pending.requested.iter().cloned().collect(),
        granted_permissions: pending.granted.iter().cloned().collect(),
        config: pending.config.clone(),
        enabled: existing.is_none_or(|item| item.enabled) && requirements_complete,
        installed_at_ms: existing.map_or(now, |item| item.installed_at_ms),
        updated_at_ms: now,
    };
    write_installation_and_cleanup(
        store,
        database,
        &installation,
        existing.map(|item| item.updated_at_ms),
        existing,
    )?;
    print_installation_result(pending, requirements_complete, installation.enabled);
    Ok(())
}

fn print_installation_result(
    pending: &PendingInstallation,
    requirements_complete: bool,
    enabled: bool,
) {
    println!(
        "{} `{}`{}",
        if pending.update {
            "Updated"
        } else {
            "Installed"
        },
        terminal_safe(&pending.instance_id),
        if !requirements_complete {
            " as disabled because permissions or configuration are incomplete."
        } else if enabled {
            ". Restart bkmqb to load the plugin."
        } else {
            ". The installation remains disabled."
        }
    );
    if !pending.permissions_complete {
        let missing = pending
            .requested
            .difference(&pending.granted)
            .cloned()
            .collect::<Vec<_>>();
        println!(
            "Missing permissions: {}. Re-run with `--accept-permissions` or explicit `--grant` options.",
            terminal_safe(&missing.join(", "))
        );
    }
    if let Some(error) = &pending.config_error {
        println!("Configuration is incomplete: {}", terminal_safe(error));
        println!("Provide a JSON object with `--config-file <path>`, then enable the plugin.");
    }
    if requirements_complete && !enabled {
        println!(
            "Enable it with `bkmqb plugin enable {}`, then restart bkmqb.",
            terminal_safe(&pending.instance_id)
        );
    }
}

#[derive(Debug)]
struct ResolvedPluginSource {
    bytes: Vec<u8>,
    source: String,
    marketplace_plugin: Option<MarketplacePlugin>,
}

async fn resolve_plugin_source(
    source: &str,
    marketplace_url: &str,
) -> Result<ResolvedPluginSource, Box<dyn std::error::Error>> {
    if looks_like_plugin_path(source) {
        let path = PathBuf::from(source);
        return Ok(ResolvedPluginSource {
            bytes: read_bounded_plugin_package(&path)?,
            source: path.display().to_string(),
            marketplace_plugin: None,
        });
    }
    validate_selector(source)?;
    let client = MarketplaceClient::new(marketplace_url)?;
    let index = client.fetch_index().await?;
    let plugin = index.find(source)?.clone();
    let bytes = client.download(&plugin).await?;
    Ok(ResolvedPluginSource {
        bytes,
        source: plugin.latest.download.clone(),
        marketplace_plugin: Some(plugin),
    })
}

fn looks_like_plugin_path(source: &str) -> bool {
    source.to_ascii_lowercase().ends_with(".bkm-plugin")
        || source.contains('/')
        || source.contains('\\')
        || source.starts_with('.')
        || source
            .as_bytes()
            .get(1)
            .is_some_and(|separator| *separator == b':')
}

fn validate_install_environment(
    options: &InstallOptions,
    interactive: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if interactive {
        return Ok(());
    }
    if !options.yes {
        return Err("non-interactive plugin installation requires `-y`".into());
    }
    if !options.accept_permissions {
        return Err("non-interactive plugin installation requires `--accept-permissions`".into());
    }
    if !options.config_file_provided {
        return Err(
            "non-interactive installation of a configurable plugin requires `--config-file <JSON>`"
                .into(),
        );
    }
    Ok(())
}

fn read_bounded_plugin_package(source: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut file = open_regular_file_without_following_links(source, "plugin package")?;
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

fn inherit_global_administrators(
    package: &ValidatedPluginPackage,
    config: &mut BTreeMap<String, Value>,
    options: &InstallOptions,
    installation_file: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !schema_declares_property(package.config_schema(), "admins") {
        return Ok(());
    }
    if options.explicit_config_keys.contains("admins") || config.contains_key("admins") {
        return Ok(());
    }
    let administrators = plugins::global_administrator_ids(installation_file)?;
    if !administrators.is_empty() {
        config.insert(
            "admins".to_owned(),
            Value::Array(administrators.into_iter().map(Value::String).collect()),
        );
    }
    Ok(())
}

fn schema_declares_property(schema: Option<&Value>, property: &str) -> bool {
    let Some(root) = schema else {
        return false;
    };
    schema_node_declares_property(root, root, property, &mut BTreeSet::new())
}

fn schema_node_declares_property(
    root: &Value,
    node: &Value,
    property: &str,
    visited_references: &mut BTreeSet<String>,
) -> bool {
    if node
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(property))
    {
        return true;
    }
    if let Some(reference) = node.get("$ref").and_then(Value::as_str)
        && reference.starts_with('#')
        && visited_references.insert(reference.to_owned())
        && root
            .pointer(reference.strip_prefix('#').unwrap_or_default())
            .is_some_and(|target| {
                schema_node_declares_property(root, target, property, visited_references)
            })
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"]
        .into_iter()
        .filter_map(|keyword| node.get(keyword).and_then(Value::as_array))
        .flatten()
        .any(|child| schema_node_declares_property(root, child, property, visited_references))
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
    interactive: bool,
) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let mut granted = existing.map_or_else(BTreeSet::new, |existing| {
        existing
            .granted_permissions
            .iter()
            .filter(|capability| requested.contains(*capability))
            .cloned()
            .collect()
    });
    granted.extend(options.grants.iter().cloned());
    if let Some(unsupported) = granted.iter().find(|grant| !requested.contains(*grant)) {
        return Err(
            format!("capability `{unsupported}` was not requested by the plugin manifest").into(),
        );
    }
    if options.accept_permissions {
        granted.clone_from(requested);
    } else {
        let missing = requested.difference(&granted).cloned().collect::<Vec<_>>();
        if interactive && !missing.is_empty() {
            println!("Permissions requested by this plugin:");
            for capability in &missing {
                println!("  - {}", terminal_safe(capability));
            }
            if confirm("Accept all requested permissions?")? {
                granted.extend(missing);
            }
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
            terminal_safe(&metadata.name),
            terminal_safe(&plugin.instance_id),
            terminal_safe(&plugin.version),
            if plugin.enabled {
                "enabled"
            } else {
                "disabled"
            },
            terminal_safe(&plugin.trust_level)
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
    println!("Plugin ID: {}", terminal_safe(&plugin.plugin_id));
    println!("Name: {}", terminal_safe(&metadata.name));
    if !metadata.description.is_empty() {
        println!("Description: {}", terminal_safe(&metadata.description));
    }
    println!("Instance ID: {}", terminal_safe(&plugin.instance_id));
    println!("Version: {}", terminal_safe(&plugin.version));
    println!("Enabled: {}", plugin.enabled);
    println!(
        "Trust: {} ({})",
        terminal_safe(&plugin.trust_level),
        terminal_safe(&plugin.signature_status)
    );
    println!("Package: {}", terminal_safe(&plugin.package_path));
    println!("SHA-256: {}", plugin.package_sha256);
    println!("Granted capabilities:");
    for capability in plugin.granted_permissions {
        println!("  - {}", terminal_safe(&capability));
    }
    Ok(())
}

fn set_enabled(
    store: &PluginStore,
    instance_id: &str,
    enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if enabled && plugins::is_reserved_bundled_instance_id(instance_id) {
        return Err(
            format!("plugin instance `{instance_id}` is reserved for a bundled plugin").into(),
        );
    }
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

#[derive(Debug, Default)]
struct InstallOptions {
    instance_id: Option<String>,
    grants: BTreeSet<String>,
    config: BTreeMap<String, Value>,
    explicit_config_keys: BTreeSet<String>,
    config_file_provided: bool,
    accept_permissions: bool,
    yes: bool,
}

impl InstallOptions {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut options = Self::default();
        let mut config_file = None;
        let mut inline_config = BTreeMap::new();
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
                    inline_config.insert(key.to_owned(), serde_json::from_str(value)?);
                    options.explicit_config_keys.insert(key.to_owned());
                }
                "--config-file" => {
                    index += 1;
                    if config_file.is_some() {
                        return Err("--config-file may only be specified once".into());
                    }
                    config_file = Some(PathBuf::from(
                        arguments
                            .get(index)
                            .ok_or("--config-file requires a path")?,
                    ));
                }
                "--accept-permissions" => options.accept_permissions = true,
                "--yes" | "-y" => options.yes = true,
                unknown => return Err(format!("unknown install option `{unknown}`").into()),
            }
            index += 1;
        }
        if let Some(path) = config_file {
            options.config = read_plugin_config_file(&path)?;
            options.config_file_provided = true;
            options
                .explicit_config_keys
                .extend(options.config.keys().cloned());
        }
        if let Some(duplicate) = inline_config
            .keys()
            .find(|key| options.config.contains_key(*key))
        {
            return Err(format!(
                "configuration key `{duplicate}` is present in both --config-file and --config"
            )
            .into());
        }
        options.config.extend(inline_config);
        Ok(options)
    }
}

#[derive(Debug)]
struct UniqueConfigMap(BTreeMap<String, Value>);

impl<'de> serde::Deserialize<'de> for UniqueConfigMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UniqueConfigVisitor;

        impl<'de> Visitor<'de> for UniqueConfigVisitor {
            type Value = UniqueConfigMap;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object with unique configuration keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut config = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if config.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate configuration key `{key}`"
                        )));
                    }
                }
                Ok(UniqueConfigMap(config))
            }
        }

        deserializer.deserialize_map(UniqueConfigVisitor)
    }
}

fn read_plugin_config_file(
    path: &Path,
) -> Result<BTreeMap<String, Value>, Box<dyn std::error::Error>> {
    let mut file = open_regular_file_without_following_links(path, "plugin configuration file")?;
    let mut bytes = Vec::with_capacity(MAX_PLUGIN_CONFIG_FILE_BYTES.min(16 * 1024));
    std::io::Read::by_ref(&mut file)
        .take(u64::try_from(MAX_PLUGIN_CONFIG_FILE_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PLUGIN_CONFIG_FILE_BYTES {
        return Err(format!(
            "plugin configuration file `{}` exceeds {} bytes",
            path.display(),
            MAX_PLUGIN_CONFIG_FILE_BYTES
        )
        .into());
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let config = <UniqueConfigMap as serde::Deserialize>::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(config)
}

fn open_regular_file_without_following_links(
    path: &Path,
    label: &str,
) -> Result<File, Box<dyn std::error::Error>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(_)
            if fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()) =>
        {
            return Err(format!(
                "{label} `{}` must be a regular file and not a symbolic link",
                path.display()
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_windows_reparse_point(&metadata) {
        return Err(format!(
            "{label} `{}` must be a regular file and not a symbolic link",
            path.display()
        )
        .into());
    }
    Ok(file)
}

#[cfg(windows)]
fn is_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

struct InstallSummary<'a> {
    operation: &'a str,
    manifest: &'a plugin_api::PluginManifest,
    instance_id: &'a str,
    hash: &'a str,
    requested: &'a BTreeSet<String>,
    granted: &'a BTreeSet<String>,
    source: &'a str,
    locale: &'a str,
}

fn print_install_summary(summary: &InstallSummary<'_>) -> Result<(), plugin_api::ManifestError> {
    let metadata = summary.manifest.metadata.resolve(summary.locale)?;
    println!(
        "{}: {} {}",
        summary.operation,
        terminal_safe(&metadata.name),
        terminal_safe(&summary.manifest.version)
    );
    println!("Plugin ID: {}", terminal_safe(summary.manifest.id.as_str()));
    println!("Instance ID: {}", terminal_safe(summary.instance_id));
    println!("Source: {}", terminal_safe(summary.source));
    println!("SHA-256: {}", summary.hash);
    println!("Source trust: local-wasm (unsigned)");
    println!("Requested capabilities:");
    for capability in summary.requested {
        println!(
            "  {} {}",
            if summary.granted.contains(capability) {
                "+"
            } else {
                "-"
            },
            terminal_safe(capability)
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

pub(crate) fn terminal_safe(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '\u{00ad}'
                    | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
        {
            let _ = write!(output, "\\u{{{:x}}}", u32::from(character));
        } else {
            output.push(character);
        }
    }
    output
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
        "BroKnowMyQQBot command line\n\nUsage:\n  bkmqb\n  bkmqb browser <command>\n  bkmqb config check\n  bkmqb plugin <command>\n  bkmqb version"
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
  bkmqb plugin marketplace list [query] [--url URL]\n\
  bkmqb plugin marketplace info <plugin> [--url URL]\n\
  bkmqb plugin install <package-or-marketplace-plugin> [--instance ID] [--accept-permissions | --grant CAP]... [--config-file FILE] [--config KEY=JSON]... [-y]\n\
  bkmqb plugin update <package-or-marketplace-plugin> [--instance ID] [--accept-permissions | --grant CAP]... [--config-file FILE] [--config KEY=JSON]... [-y]\n\
  bkmqb plugin list\n\
  bkmqb plugin info <instance-id>\n\
  bkmqb plugin enable <instance-id>\n\
  bkmqb plugin disable <instance-id>\n\
  bkmqb plugin remove <instance-id>\n\
  bkmqb plugin dead-letter list [instance-id]\n\
  bkmqb plugin recover <instance-id>"
    );
}

fn print_marketplace_help() {
    println!(
        "Plugin marketplace\n\n\
Usage:\n\
  bkmqb plugin marketplace list [query] [--url URL]\n\
  bkmqb plugin marketplace info <plugin> [--url URL]"
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

    fn test_config(database: PathBuf) -> BotConfig {
        let mut config = BotConfig::default();
        config.plugins.database = database;
        config.plugins.installations = None;
        config
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
        let config_path = directory.0.join("config.json");
        fs::write(&config_path, "{}").unwrap();
        let store = PluginStore::open(&database).unwrap();
        let arguments = vec![
            "install".to_owned(),
            package_path.display().to_string(),
            "--accept-permissions".to_owned(),
            "--config-file".to_owned(),
            config_path.display().to_string(),
            "--yes".to_owned(),
        ];
        let config = test_config(database.clone());

        install_or_update(&store, &config, &arguments, false, "en")
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
        let config_path = directory.0.join("config.json");
        fs::write(&config_path, "{}").unwrap();
        let store = PluginStore::open(&database).unwrap();
        let install_arguments = vec![
            "install".to_owned(),
            package_path.display().to_string(),
            "--accept-permissions".to_owned(),
            "--config-file".to_owned(),
            config_path.display().to_string(),
            "--yes".to_owned(),
        ];
        let config = test_config(database.clone());
        install_or_update(&store, &config, &install_arguments, false, "en")
            .await
            .unwrap();
        store
            .set_installation_enabled("dev.bkm.wasm-ping/default", false, now_ms())
            .unwrap();

        let update_arguments = vec![
            "update".to_owned(),
            package_path.display().to_string(),
            "--accept-permissions".to_owned(),
            "--config-file".to_owned(),
            config_path.display().to_string(),
            "--yes".to_owned(),
        ];
        install_or_update(&store, &config, &update_arguments, true, "en")
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
    fn update_leaves_new_permissions_ungranted_without_acceptance() {
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
        assert_eq!(
            resolve_grants(
                &requested,
                Some(&existing),
                &InstallOptions::default(),
                false,
            )
            .unwrap(),
            BTreeSet::from(["message.reply".to_owned()])
        );
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
            resolve_grants(&requested, Some(&existing), &options, false).unwrap(),
            requested
        );
    }

    #[test]
    fn update_keeps_unaccepted_permissions_missing() {
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
        assert_eq!(
            resolve_grants(&requested, Some(&existing), &options, false).unwrap(),
            BTreeSet::from(["message.reply".to_owned(), "message.send".to_owned()])
        );
    }

    #[test]
    fn non_interactive_install_requires_explicit_automation_flags() {
        let mut options = InstallOptions {
            yes: true,
            ..InstallOptions::default()
        };
        assert!(
            validate_install_environment(&options, false)
                .unwrap_err()
                .to_string()
                .contains("--accept-permissions")
        );
        options.accept_permissions = true;
        assert!(
            validate_install_environment(&options, false)
                .unwrap_err()
                .to_string()
                .contains("--config-file")
        );
        options.config_file_provided = true;
        validate_install_environment(&options, false).unwrap();
    }

    #[tokio::test]
    async fn marketplace_help_does_not_load_configuration_or_network() {
        run_marketplace(&[]).await.unwrap();
        run_marketplace(&["help".to_owned()]).await.unwrap();
        run_marketplace(&["--help".to_owned()]).await.unwrap();
        run_marketplace(&["-h".to_owned()]).await.unwrap();
    }

    #[tokio::test]
    async fn marketplace_rejects_invalid_usage_before_loading_configuration_or_network() {
        assert!(
            run_marketplace(&["unknown".to_owned()])
                .await
                .unwrap_err()
                .to_string()
                .contains("unknown marketplace command")
        );
        assert!(
            run_marketplace(&["info".to_owned()])
                .await
                .unwrap_err()
                .to_string()
                .contains("usage:")
        );
        assert!(
            run_marketplace(&["list".to_owned(), "one".to_owned(), "two".to_owned()])
                .await
                .unwrap_err()
                .to_string()
                .contains("usage:")
        );
    }

    #[test]
    fn administrator_property_detection_follows_local_references() {
        let schema = serde_json::json!({
            "$defs": {
                "authorization": {
                    "type": "object",
                    "properties": { "admins": { "type": "array" } }
                }
            },
            "allOf": [{ "$ref": "#/$defs/authorization" }]
        });
        assert!(schema_declares_property(Some(&schema), "admins"));
        assert!(!schema_declares_property(Some(&schema), "owners"));
    }

    #[test]
    fn config_file_rejects_duplicate_and_conflicting_keys() {
        let directory = TestDirectory::new();
        let duplicate = directory.0.join("duplicate.json");
        fs::write(&duplicate, r#"{"admins":[],"admins":["owner"]}"#).unwrap();
        assert!(read_plugin_config_file(&duplicate).is_err());

        let config = directory.0.join("config.json");
        fs::write(&config, r#"{"admins":["owner"]}"#).unwrap();
        let arguments = vec![
            "--config-file".to_owned(),
            config.display().to_string(),
            "--config".to_owned(),
            "admins=[\"other\"]".to_owned(),
        ];
        assert!(InstallOptions::parse(&arguments).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn plugin_inputs_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let config = directory.0.join("config.json");
        let config_link = directory.0.join("config-link.json");
        fs::write(&config, "{}").unwrap();
        symlink(&config, &config_link).unwrap();
        assert!(read_plugin_config_file(&config_link).is_err());

        let package = directory.0.join("plugin.bkm-plugin");
        let package_link = directory.0.join("plugin-link.bkm-plugin");
        fs::write(&package, b"not-a-package").unwrap();
        symlink(&package, &package_link).unwrap();
        assert!(read_bounded_plugin_package(&package_link).is_err());
    }

    #[test]
    fn locale_normalization_uses_canonical_bcp47_case() {
        assert_eq!(normalize_locale("zh_CN.UTF-8").as_deref(), Some("zh-CN"));
        assert_eq!(normalize_locale("en_us").as_deref(), Some("en-US"));
        assert_eq!(normalize_locale("ja").as_deref(), Some("ja"));
        assert_eq!(normalize_locale("C"), None);
    }

    #[test]
    fn terminal_output_escapes_controls_and_bidi_formatting() {
        assert_eq!(
            terminal_safe("safe\u{1b}[2J\u{200b}hidden\u{2028}line\u{202e}text\u{feff}"),
            "safe\\u{1b}[2J\\u{200b}hidden\\u{2028}line\\u{202e}text\\u{feff}"
        );
    }

    #[test]
    fn local_plugin_path_detection_handles_windows_and_extension_case() {
        assert!(looks_like_plugin_path("PLUGIN.BKM-PLUGIN"));
        assert!(looks_like_plugin_path("C:plugin"));
        assert!(looks_like_plugin_path("C:\\plugin"));
        assert!(!looks_like_plugin_path("github-issue"));
    }

    #[test]
    fn bundled_instance_ids_are_reserved_from_managed_installations() {
        assert!(plugins::is_reserved_bundled_instance_id(
            "dev.bkm.help/default"
        ));
        assert!(plugins::is_reserved_bundled_instance_id(
            "dev.bkm.admin/default"
        ));
        assert!(plugins::is_reserved_bundled_instance_id(
            "dev.bkm.reminder/default"
        ));
        assert!(!plugins::is_reserved_bundled_instance_id(
            "dev.bkm.example/default"
        ));
        let store = PluginStore::in_memory().unwrap();
        assert!(
            set_enabled(&store, "dev.bkm.admin/default", true)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
    }
}
