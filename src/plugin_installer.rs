//! Verified package persistence shared by CLI and Web management.

use fs2::FileExt as _;
use plugin_host::{
    PluginHostError, PluginInstallation, PluginStore, ValidatedPluginPackage, WasmPlugin,
    validate_plugin_config,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) async fn validate_install_candidate(
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
pub(crate) struct PendingInstallation {
    pub(crate) manifest: plugin_api::PluginManifest,
    pub(crate) instance_id: String,
    pub(crate) package_sha256: String,
    pub(crate) package_bytes: Vec<u8>,
    pub(crate) source: String,
    pub(crate) requested: BTreeSet<String>,
    pub(crate) granted: BTreeSet<String>,
    pub(crate) config: BTreeMap<String, Value>,
    pub(crate) permissions_complete: bool,
    pub(crate) config_error: Option<String>,
    pub(crate) update: bool,
}

pub(crate) fn finish_installation(
    store: &PluginStore,
    database: &Path,
    existing: Option<&PluginInstallation>,
    pending: &PendingInstallation,
) -> Result<PluginInstallation, Box<dyn std::error::Error>> {
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
    Ok(installation)
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

pub(crate) fn persist_managed_package(
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

pub(crate) fn lock_managed_package_lifecycle(database: &Path) -> Result<File, io::Error> {
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

pub(crate) fn cleanup_unreferenced_managed_package(
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}
