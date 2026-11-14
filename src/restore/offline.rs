//! Only the drained synchronous entry point and pre-runtime startup call this.
use super::{files::Files, plan::Prepared};
use crate::{
    backup::{self, Repository, Result, invalid},
    config, plugin_installer,
    plugin_management::ManagementError,
};
use plugin_host::{
    LogicalArchive, LogicalSnapshot, PluginInstallation, PluginStore, StoreError,
    ValidatedPluginPackage,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, path::PathBuf};

#[derive(Clone)]
pub(crate) struct Accepted {
    pub repository: Repository,
    pub database: PathBuf,
    pub declarations: Option<PathBuf>,
    pub operation_id: String,
    pub work_hash: String,
}
impl Accepted {
    pub(crate) fn record_status(&self, status: &str) -> Result<()> {
        let _config = config::ConfigFileLock::acquire(&self.repository.config).map_err(invalid)?;
        let lifecycle =
            plugin_installer::lock_managed_package_lifecycle(&self.database).map_err(invalid)?;
        let files = Files::new(&self.repository)?;
        let current: Outcome = files.read("outcome.json", 4096)?;
        let result = if current.operation_id == self.operation_id {
            outcome(&files, &self.operation_id, status)
        } else {
            Err(ManagementError::Conflict)
        };
        let _ = fs2::FileExt::unlock(&lifecycle);
        result
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Work {
    operation_id: String,
    archive_hash: String,
    old_config: String,
    new_config: String,
    expected: Vec<PluginInstallation>,
    declarations: Vec<PluginInstallation>,
    declaration_revision: Option<String>,
    imported: LogicalSnapshot,
    affected: Vec<String>,
    bindings: serde_json::Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    operation_id: String,
    old_config: String,
    new_config: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Outcome {
    pub operation_id: String,
    pub status: String,
}

pub(crate) fn stage(
    repository: Repository,
    database: PathBuf,
    declarations: Option<PathBuf>,
    prepared: Prepared,
) -> Result<Accepted> {
    let _config = config::ConfigFileLock::acquire(&repository.config).map_err(invalid)?;
    let lifecycle = plugin_installer::lock_managed_package_lifecycle(&database).map_err(invalid)?;
    let result = stage_locked(repository, database, declarations, prepared);
    let _ = fs2::FileExt::unlock(&lifecycle);
    result
}
fn stage_locked(
    repository: Repository,
    database: PathBuf,
    declarations: Option<PathBuf>,
    prepared: Prepared,
) -> Result<Accepted> {
    // Known existing database only; never manufacture an empty recovery target.
    PluginStore::logical_restore_committed_at(&database, &prepared.inspection.operation_id)
        .map_err(invalid)?;
    if config_text(&repository)? != prepared.old_config
        || super::declaration_revision(declarations.as_deref())? != prepared.declaration_revision
        || crate::plugins::backup_declarations(declarations.as_deref()).map_err(invalid)?
            != prepared.declarations
        || PluginStore::open(&database)
            .map_err(invalid)?
            .logical_snapshot()
            .map_err(invalid)?
            .installations
            != prepared.expected
    {
        return Err(ManagementError::Conflict);
    }
    let files = Files::new(&repository)?;
    files.cleanup_temporaries()?;
    if prepared.old_config.len() > 1024 * 1024 || prepared.new_config.len() > 1024 * 1024 {
        return Err(ManagementError::Backup);
    }
    if files.exists("journal.json")? {
        return Err(ManagementError::Busy);
    }
    if files.exists("outcome.json")? {
        let previous: Outcome = files.read("outcome.json", 4096)?;
        if previous.status == "pending" {
            return Err(ManagementError::Busy);
        }
    }
    let operation_id = prepared.inspection.operation_id.clone();
    let archive_hash = files.bytes(
        "archive.bkm-backup",
        &prepared.archive.to_bytes().map_err(invalid)?,
    )?;
    let work = Work {
        operation_id: operation_id.clone(),
        archive_hash,
        old_config: prepared.old_config,
        new_config: prepared.new_config,
        expected: prepared.expected,
        declarations: prepared.declarations,
        declaration_revision: prepared.declaration_revision,
        imported: prepared.imported,
        affected: prepared.inspection.affected_instances,
        bindings: prepared.current_bindings,
    };
    let work_hash = files.write("accepted.json", &work, backup::LIMIT)?;
    outcome(&files, &operation_id, "pending")?;
    Ok(Accepted {
        repository,
        database,
        declarations,
        operation_id,
        work_hash,
    })
}
pub(crate) fn outcome(files: &Files, id: &str, status: &str) -> Result<()> {
    files
        .write(
            "outcome.json",
            &Outcome {
                operation_id: id.into(),
                status: status.into(),
            },
            4096,
        )
        .map(|_| ())
}
fn replace_config(repository: &Repository, text: &str) -> Result<()> {
    config::atomic_write(&repository.config, text.as_bytes(), 0o600).map_err(invalid)?;
    File::open(
        repository
            .config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new(".")),
    )
    .map_err(invalid)?
    .sync_all()
    .map_err(invalid)
}
fn config_text(repository: &Repository) -> Result<String> {
    String::from_utf8(backup::read(&repository.config, 1024 * 1024)?).map_err(invalid)
}

pub(crate) fn apply(accepted: &Accepted) -> Result<()> {
    let _config = config::ConfigFileLock::acquire(&accepted.repository.config).map_err(invalid)?;
    let lifecycle =
        plugin_installer::lock_managed_package_lifecycle(&accepted.database).map_err(invalid)?;
    let files = Files::new(&accepted.repository)?;
    let current: Outcome = files.read("outcome.json", 4096)?;
    if current.operation_id != accepted.operation_id || current.status != "pending" {
        return Err(ManagementError::Conflict);
    }
    let result = apply_locked(accepted, &files);
    if result.is_err() {
        let files = Files::new(&accepted.repository)?;
        if files.exists("journal.json")? {
            recover_locked(&accepted.repository, &accepted.database, &files)?;
        } else {
            let committed = PluginStore::logical_restore_committed_at(
                &accepted.database,
                &accepted.operation_id,
            )
            .map_err(invalid)?;
            outcome(
                &files,
                &accepted.operation_id,
                if committed {
                    "applied_restart_failed"
                } else {
                    "rejected"
                },
            )?;
        }
    }
    let _ = fs2::FileExt::unlock(&lifecycle);
    result
}

fn apply_locked(accepted: &Accepted, files: &Files) -> Result<()> {
    if files.hash("accepted.json", backup::LIMIT)? != accepted.work_hash {
        return Err(ManagementError::Backup);
    }
    let mut work: Work = files.read("accepted.json", backup::LIMIT)?;
    if work.operation_id != accepted.operation_id
        || config_text(&accepted.repository)? != work.old_config
        || super::declaration_revision(accepted.declarations.as_deref())?
            != work.declaration_revision
        || crate::plugins::backup_declarations(accepted.declarations.as_deref()).map_err(invalid)?
            != work.declarations
    {
        return Err(ManagementError::Conflict);
    }
    config::BotConfig::check_source(&work.new_config).map_err(invalid)?;
    if files.hash("archive.bkm-backup", backup::LIMIT)? != work.archive_hash {
        return Err(ManagementError::Backup);
    }
    let archive = LogicalArchive::from_bytes(&backup::read(
        &files.root.join("archive.bkm-backup"),
        backup::LIMIT,
    )?)
    .map_err(invalid)?;
    PluginStore::logical_restore_committed_at(&accepted.database, &work.operation_id)
        .map_err(invalid)?;
    let store = PluginStore::open(&accepted.database).map_err(invalid)?;
    if store.logical_snapshot().map_err(invalid)?.installations != work.expected {
        return Err(ManagementError::Conflict);
    }
    for item in &mut work.imported.installations {
        let bytes = archive
            .packages
            .get(&item.package_sha256)
            .ok_or(ManagementError::Backup)?;
        let package = ValidatedPluginPackage::from_bytes(bytes).map_err(invalid)?;
        let path = plugin_installer::persist_installed_package(
            &accepted.database,
            package.manifest(),
            &item.package_sha256,
            bytes,
        )
        .map_err(invalid)?;
        path.to_str()
            .ok_or(ManagementError::Backup)?
            .clone_into(&mut item.package_path);
    }
    File::open(
        accepted
            .database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new(".")),
    )
    .map_err(invalid)?
    .sync_all()
    .map_err(invalid)?;
    store
        .restore_logical_offline(
            &work.operation_id,
            &work.imported,
            &work.expected,
            &work.affected,
            |current| {
                rollback(files, &work, current).map_err(|_| StoreError::InvalidLogicalSnapshot)?;
                files
                    .write(
                        "journal.json",
                        &Journal {
                            operation_id: work.operation_id.clone(),
                            old_config: work.old_config.clone(),
                            new_config: work.new_config.clone(),
                        },
                        3 * 1024 * 1024,
                    )
                    .map_err(|_| StoreError::InvalidLogicalSnapshot)?;
                fault("journal");
                Ok(())
            },
            || {
                replace_config(&accepted.repository, &work.new_config)
                    .map_err(|_| StoreError::InvalidLogicalSnapshot)?;
                fault("config");
                Ok(())
            },
        )
        .map_err(invalid)?;
    fault("commit");
    drop(store);
    recover_locked(&accepted.repository, &accepted.database, files)
}

fn rollback(files: &Files, work: &Work, current: &LogicalSnapshot) -> Result<()> {
    let mut snapshot = current.clone();
    snapshot.installations.extend(work.declarations.clone());
    snapshot.validate().map_err(invalid)?;
    let mut packages = BTreeMap::new();
    let mut used = 0;
    for item in &mut snapshot.installations {
        if !packages.contains_key(&item.package_sha256) {
            let bytes = backup::read(
                std::path::Path::new(&item.package_path),
                plugin_host::MAX_PLUGIN_PACKAGE_BYTES.min(backup::LIMIT - used),
            )?;
            used += bytes.len();
            packages.insert(item.package_sha256.clone(), bytes);
        }
        item.package_path = format!("packages/{}.bkm-plugin", item.package_sha256);
    }
    let old: config::BotConfig = toml::from_str(&work.old_config).map_err(invalid)?;
    let archive = LogicalArchive::create(
        BTreeMap::from([
            ("bot".into(), backup::selected(&old)),
            ("bindings".into(), work.bindings.clone()),
        ]),
        snapshot,
        packages,
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(invalid)?;
    files.bytes("rollback.bkm-backup", &archive.to_bytes().map_err(invalid)?)?;
    Ok(())
}

pub(crate) fn recover(repository: &Repository, database: &std::path::Path) -> Result<()> {
    // Avoid creating private backup directories on ordinary startup.
    if !repository
        .root
        .join("restore")
        .try_exists()
        .map_err(invalid)?
    {
        return Ok(());
    }
    let _config = config::ConfigFileLock::acquire(&repository.config).map_err(invalid)?;
    let lifecycle = plugin_installer::lock_managed_package_lifecycle(database).map_err(invalid)?;
    let result = recover_locked(repository, database, &Files::new(repository)?);
    let _ = fs2::FileExt::unlock(&lifecycle);
    result
}
fn recover_locked(
    repository: &Repository,
    database: &std::path::Path,
    files: &Files,
) -> Result<()> {
    files.cleanup_temporaries()?;
    if !files.exists("journal.json")? {
        if files.exists("outcome.json")? {
            let previous: Outcome = files.read("outcome.json", 4096)?;
            if previous.status == "pending" {
                outcome(files, &previous.operation_id, "interrupted")?;
            }
        }
        return Ok(());
    }
    let journal: Journal = files.read("journal.json", 3 * 1024 * 1024)?;
    let current = config_text(repository)?;
    if current != journal.old_config && current != journal.new_config {
        return Err(ManagementError::Conflict);
    }
    let committed = PluginStore::logical_restore_committed_at(database, &journal.operation_id)
        .map_err(invalid)?;
    replace_config(
        repository,
        if committed {
            &journal.new_config
        } else {
            &journal.old_config
        },
    )?;
    outcome(
        files,
        &journal.operation_id,
        if committed { "applied" } else { "rolled_back" },
    )?;
    files.remove("journal.json")
}
fn fault(point: &str) {
    let _ = point;
    #[cfg(test)]
    if std::env::var("BKMQB_TEST_RESTORE_CRASH").ok().as_deref() == Some(point) {
        std::process::exit(92);
    }
}

#[cfg(test)]
mod tests;
