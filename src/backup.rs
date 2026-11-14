//! Private, finite logical artifacts; never accept a browser-selected host path.
use crate::{config::BotConfig, plugin_management::ManagementError};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};
pub(crate) type Result<T> = std::result::Result<T, ManagementError>;
pub(crate) fn invalid<E>(_: E) -> ManagementError {
    ManagementError::Backup
}
pub(crate) const LIMIT: usize = plugin_host::MAX_LOGICAL_ARCHIVE_BYTES;

#[derive(Clone)]
pub(crate) struct Repository {
    pub(crate) root: PathBuf,
    pub(crate) config: PathBuf,
}
#[derive(Serialize)]
pub(crate) struct Artifact {
    pub(crate) id: String,
    pub(crate) bytes: u64,
    pub(crate) created_at_ms: u128,
}

impl Repository {
    pub(crate) fn new(database: &Path, config: PathBuf) -> Self {
        Self {
            root: database
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!(
                    ".bkmqb-logical-backups-{}",
                    hex::encode(Sha256::digest(
                        database
                            .file_name()
                            .unwrap_or(database.as_os_str())
                            .as_encoded_bytes()
                    ))
                )),
            config,
        }
    }
    fn directory(&self) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
        match fs::DirBuilder::new().mode(0o700).create(&self.root) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(invalid(e)),
        }
        let metadata = fs::symlink_metadata(&self.root).map_err(invalid)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.permissions().mode() & 0o777 != 0o700
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(ManagementError::Backup);
        }
        Ok(())
    }
    pub(crate) fn path(&self, id: &str) -> Result<PathBuf> {
        if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
            return Err(ManagementError::InvalidOperation);
        }
        Ok(self.root.join(format!("{id}.bkm-backup")))
    }
    pub(crate) fn list(&self) -> Result<Vec<Artifact>> {
        self.directory()?;
        let mut result = vec![];
        for (index, entry) in fs::read_dir(&self.root)
            .map_err(invalid)?
            .take(33)
            .enumerate()
        {
            if index == 32 {
                return Err(ManagementError::Backup);
            }
            let entry = entry.map_err(invalid)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(ManagementError::Backup)?;
            if let Some(id) = name.strip_suffix(".bkm-backup") {
                let path = self.path(id)?;
                let metadata = fs::symlink_metadata(&path).map_err(invalid)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(ManagementError::Backup);
                }
                let modified = metadata.modified().map_err(invalid)?;
                if modified.elapsed().unwrap_or_default().as_secs() >= 86_400 {
                    fs::remove_file(path).map_err(invalid)?;
                    continue;
                }
                result.push(Artifact {
                    id: id.into(),
                    bytes: metadata.len(),
                    created_at_ms: modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(invalid)?
                        .as_millis(),
                });
            }
            if result.len() > 4 {
                return Err(ManagementError::Backup);
            }
        }
        result.sort_by_key(|a| std::cmp::Reverse(a.created_at_ms));
        Ok(result)
    }
    pub(crate) fn save(&self, bytes: &[u8]) -> Result<Artifact> {
        use std::os::unix::fs::OpenOptionsExt as _;
        if bytes.len() > LIMIT || self.list()?.len() >= 4 {
            return Err(ManagementError::Backup);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let path = self.path(&id)?;
        let mut created = false;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            created = true;
            file.write_all(bytes)?;
            file.sync_all()?;
            File::open(&self.root)?.sync_all()
        })();
        if let Err(error) = result {
            if created {
                let _ = fs::remove_file(path);
            }
            return Err(invalid(error));
        }
        Ok(Artifact {
            id,
            bytes: bytes.len() as u64,
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(invalid)?
                .as_millis(),
        })
    }
    pub(crate) fn open(&self, id: &str) -> Result<File> {
        if !self.list()?.iter().any(|a| a.id == id) {
            return Err(ManagementError::ExpiredSelection);
        }
        regular(&self.path(id)?, LIMIT)
    }
}

pub(crate) fn regular(path: &Path, limit: usize) -> Result<File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(invalid)?;
    let metadata = file.metadata().map_err(invalid)?;
    if !metadata.is_file() || metadata.len() > limit as u64 || metadata.nlink() != 1 {
        return Err(ManagementError::Backup);
    }
    Ok(file)
}
pub(crate) fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = regular(path, limit)?;
    let expected = usize::try_from(file.metadata().map_err(invalid)?.len()).map_err(invalid)?;
    if expected > limit {
        return Err(ManagementError::Backup);
    }
    let mut bytes = Vec::with_capacity(expected + 1);
    std::io::Read::by_ref(&mut file)
        .take((expected + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(invalid)?;
    if bytes.len() != expected {
        return Err(ManagementError::Backup);
    }
    Ok(bytes)
}

/// Only on-disk typed settings, never the environment-overridden runtime config.
pub(crate) fn selected(config: &BotConfig) -> serde_json::Value {
    let qq = &config.qq;
    let webhook = &qq.webhook;
    let onebot = &config.onebot11;
    let runtime = &config.runtime;
    let console = &config.logging.console;
    let files = &config.logging.files;
    serde_json::json!({
        "commands":{"prefix":config.commands.prefix},
        "runtime":{"queue_capacity":runtime.queue_capacity,"event_concurrency":runtime.event_concurrency,"handler_timeout_seconds":runtime.handler_timeout_seconds,"shutdown_timeout_seconds":runtime.shutdown_timeout_seconds,"dedup_capacity":runtime.dedup_capacity},
        "qq":{"enabled":qq.enabled,"environment":qq.environment,"transport":qq.transport,"public_guild_messages":qq.public_guild_messages,"private_guild_messages":qq.private_guild_messages,"direct_messages":qq.direct_messages,"extended_events":qq.extended_events,"forum_events":qq.forum_events,"audio_events":qq.audio_events,"open_forum_events":qq.open_forum_events,"audio_live_member_events":qq.audio_live_member_events,"check_only":qq.check_only,
            "webhook":{"path":webhook.path,"timestamp_tolerance_seconds":webhook.timestamp_tolerance_seconds,"max_body_bytes":webhook.max_body_bytes,"max_request_concurrency":webhook.max_request_concurrency,"request_timeout_seconds":webhook.request_timeout_seconds}},
        "onebot11":{"enabled":onebot.enabled,"action_timeout_seconds":onebot.action_timeout_seconds,"max_message_bytes":onebot.max_message_bytes,"max_pending_actions":onebot.max_pending_actions},
        "logging":{"console":{"enabled":console.enabled,"language":console.language,"ansi":console.ansi,"filter":console.filter},
            "files":{"enabled":files.enabled,"filter":files.filter,"zstd_level":files.zstd_level,"buffer_lines":files.buffer_lines,"runtime_max_file_mb":files.runtime_max_file_mb,"runtime_max_total_mb":files.runtime_max_total_mb,"messages_max_file_mb":files.messages_max_file_mb,"messages_max_total_mb":files.messages_max_total_mb}}
    })
}

#[cfg(test)]
mod tests;
