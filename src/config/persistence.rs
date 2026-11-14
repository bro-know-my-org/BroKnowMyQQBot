//! Shared writer lock and revisions for setup and Web configuration updates.

use std::{fs, path::Path};

use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};

use super::ConfigError;

/// Apply only explicitly selected fields while retaining comments and other sections.
pub(crate) fn patch_document(source: &str, patch: &toml::Value) -> Result<String, ()> {
    fn merge(target: &mut dyn toml_edit::TableLike, patch: &dyn toml_edit::TableLike) {
        for (key, value) in patch.iter() {
            if let Some(existing) = target.get_mut(key) {
                if let (Some(left), Some(right)) =
                    (existing.as_table_like_mut(), value.as_table_like())
                {
                    merge(left, right);
                    continue;
                }
                let mut replacement = value.clone();
                if let (Some(old), Some(new)) = (existing.as_value(), replacement.as_value_mut()) {
                    *new.decor_mut() = old.decor().clone();
                }
                *existing = replacement;
            } else {
                target.insert(key, value.clone());
            }
        }
    }
    let mut document = source.parse::<toml_edit::DocumentMut>().map_err(|_| ())?;
    let patch = toml::to_string(patch)
        .map_err(|_| ())?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| ())?;
    merge(document.as_table_mut(), patch.as_table());
    Ok(document.to_string())
}

pub(crate) fn config_revision(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Keep the sidecar inode stable across atomic replacements of the config file.
/// Writers must hold this guard from revision validation through replacement.
pub(crate) struct ConfigFileLock {
    file: fs::File,
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        // A concurrently spawned child may briefly inherit the open file
        // description before exec. Explicitly unlock instead of relying on
        // the last descriptor closing to release the advisory lock.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl ConfigFileLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self, ConfigError> {
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".management.lock");
        let acquire = || {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let file = options.open(&lock_path)?;
            file.try_lock_exclusive()?;
            Ok(Self { file })
        };
        acquire().map_err(|source: std::io::Error| {
            if source.kind() == std::io::ErrorKind::WouldBlock {
                ConfigError::Conflict(path.to_path_buf())
            } else {
                ConfigError::Lock {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BotConfig;
    use std::{sync::Barrier, thread};

    #[test]
    fn configurations_with_different_extensions_have_independent_locks() {
        let directory = std::env::temp_dir().join(format!("bkm-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let toml = ConfigFileLock::acquire(&directory.join("bot.toml")).unwrap();
        let conf = ConfigFileLock::acquire(&directory.join("bot.conf")).unwrap();
        assert!(matches!(
            ConfigFileLock::acquire(&directory.join("bot.toml")),
            Err(ConfigError::Conflict(_))
        ));
        drop((toml, conf));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_saves_of_the_same_snapshot_have_only_one_winner() {
        let directory = std::env::temp_dir().join(format!("bkm-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bot.toml");
        fs::write(&path, "[commands]\nprefix = '/'\n").unwrap();
        let first = BotConfig::load_from_path(&path, true).unwrap();
        let second = first.clone();
        let barrier = Barrier::new(2);
        let save = |mut config: BotConfig, prefix: &str| {
            config.commands.prefix = prefix.to_owned();
            barrier.wait();
            config.save()
        };
        let (first, second) = thread::scope(|scope| {
            let first = scope.spawn(|| save(first, "!"));
            let second = scope.spawn(|| save(second, "机器人"));
            (first.join().unwrap(), second.join().unwrap())
        });
        let winner = match (first, second) {
            (Ok(_), Err(ConfigError::Conflict(_))) => "!",
            (Err(ConfigError::Conflict(_)), Ok(_)) => "机器人",
            results => panic!("expected one successful save and one conflict: {results:?}"),
        };
        let saved: BotConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.commands.prefix, winner);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saving_a_loaded_snapshot_does_not_recreate_a_deleted_config() {
        let directory = std::env::temp_dir().join(format!("bkm-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bot.toml");
        fs::write(&path, "[commands]\nprefix = '/'\n").unwrap();
        let mut config = BotConfig::load_from_path(&path, true).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(matches!(config.save(), Err(ConfigError::Conflict(_))));
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
