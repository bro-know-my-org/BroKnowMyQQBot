//! A missing declarations file is materialized only on the first confirmed save.
use super::{
    ManagementError, PluginManager,
    bundled::{file_error, read_source},
};
use crate::config::{self, BotConfig, ConfigFileLock};
use std::{fs, io::Write as _, path::PathBuf};

pub(super) struct Source {
    pub(super) text: String,
    pub(super) revision: String,
    pub(super) path: PathBuf,
    _lock: Option<ConfigFileLock>,
    initialize: Option<(PathBuf, String)>,
}
impl PluginManager {
    pub(super) fn read_bundled_source(&self, writing: bool) -> Result<Source, ManagementError> {
        if let Some(path) = self.declaration_path()? {
            let lock = writing
                .then(|| ConfigFileLock::acquire(&path).map_err(|e| file_error(&e)))
                .transpose()?;
            let text = read_source(&path)?;
            return Ok(Source {
                revision: config::config_revision(text.as_bytes()),
                text,
                path,
                _lock: lock,
                initialize: None,
            });
        }
        let Some(path) = &self.bundled_config_path else {
            if writing {
                return Err(ManagementError::BundledUnavailable);
            }
            return Ok(Source {
                text: String::new(),
                revision: config::config_revision(b""),
                path: PathBuf::new(),
                _lock: None,
                initialize: None,
            });
        };
        let lock = writing
            .then(|| ConfigFileLock::acquire(path).map_err(|e| file_error(&e)))
            .transpose()?;
        let source = read_source(path)?;
        let saved: BotConfig =
            toml::from_str(&source).map_err(|_| ManagementError::BundledUnavailable)?;
        // A different writer may have set a declaration path since this process started.
        // Require a restart instead of replacing its intent or loading an arbitrary file.
        if saved.plugins.installations.is_some() {
            return Err(ManagementError::Conflict);
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|_| ManagementError::BundledUnavailable)?;
        Ok(Source {
            text: String::new(),
            revision: config::config_revision(source.as_bytes()),
            path: parent.join(format!("plugins-{}.toml", uuid::Uuid::new_v4())),
            _lock: lock,
            initialize: Some((path.clone(), source)),
        })
    }
}
impl Source {
    pub(super) fn persist(
        self,
        manager: &PluginManager,
        updated: &str,
    ) -> Result<(), ManagementError> {
        let Some((config_path, source)) = &self.initialize else {
            return config::atomic_write(&self.path, updated.as_bytes(), 0o600)
                .map_err(|e| file_error(&e));
        };
        let patch =
            toml::Value::try_from(serde_json::json!({"plugins":{"installations": self.path}}))
                .map_err(|_| ManagementError::BundledUnavailable)?;
        let encoded = config::patch_document(source, &patch)
            .map_err(|()| ManagementError::BundledUnavailable)?;
        if encoded.len() > 1024 * 1024 {
            return Err(ManagementError::BundledUnavailable);
        }
        let saved: BotConfig =
            toml::from_str(&encoded).map_err(|_| ManagementError::BundledUnavailable)?;
        if saved.plugins.installations.as_ref() != Some(&self.path) {
            return Err(ManagementError::BundledUnavailable);
        }
        let mut active = manager
            .declarations_override
            .write()
            .map_err(|_| ManagementError::Store)?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|_| ManagementError::BundledUnavailable)?;
        let result = (|| {
            file.write_all(updated.as_bytes())
                .map_err(|_| ManagementError::BundledUnavailable)?;
            file.sync_all()
                .map_err(|_| ManagementError::BundledUnavailable)?;
            #[cfg(unix)]
            fs::File::open(
                self.path
                    .parent()
                    .ok_or(ManagementError::BundledUnavailable)?,
            )
            .and_then(|directory| directory.sync_all())
            .map_err(|_| ManagementError::BundledUnavailable)?;
            config::atomic_write(config_path, encoded.as_bytes(), 0o600).map_err(|e| file_error(&e))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&self.path);
        }
        result?;
        *active = Some(self.path.clone());
        Ok(())
    }
}

#[cfg(test)]
#[path = "bundled_source_tests.rs"]
mod tests;
