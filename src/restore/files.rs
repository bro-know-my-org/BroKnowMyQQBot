//! Fixed private names and bounded, durable writes for offline restoration.
use crate::backup::{self, Repository, Result, invalid};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::PathBuf,
};

pub(crate) struct Files {
    pub root: PathBuf,
}
impl Files {
    pub(crate) fn new(repository: &Repository) -> Result<Self> {
        repository.directory()?;
        let child = Repository {
            root: repository.root.join("restore"),
            config: repository.config.clone(),
        };
        child.directory()?;
        Ok(Self { root: child.root })
    }
    fn path(&self, name: &str) -> Result<PathBuf> {
        if ![
            "accepted.json",
            "archive.bkm-backup",
            "rollback.bkm-backup",
            "journal.json",
            "outcome.json",
        ]
        .contains(&name)
        {
            return Err(crate::plugin_management::ManagementError::InvalidOperation);
        }
        Ok(self.root.join(name))
    }
    pub(crate) fn exists(&self, name: &str) -> Result<bool> {
        match fs::symlink_metadata(self.path(name)?) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(invalid(e)),
        }
    }
    pub(crate) fn read<T: DeserializeOwned>(&self, name: &str, limit: usize) -> Result<T> {
        serde_json::from_slice(&backup::read(&self.path(name)?, limit)?).map_err(invalid)
    }
    pub(crate) fn write(&self, name: &str, value: &impl Serialize, limit: usize) -> Result<String> {
        self.atomic(
            name,
            |file| serde_json::to_writer(file, value).map_err(invalid),
            limit,
        )
    }
    pub(crate) fn bytes(&self, name: &str, bytes: &[u8]) -> Result<String> {
        self.atomic(
            name,
            |file| file.write_all(bytes).map_err(invalid),
            backup::LIMIT,
        )
    }
    fn atomic(
        &self,
        name: &str,
        write: impl FnOnce(&mut Bounded) -> Result<()>,
        limit: usize,
    ) -> Result<String> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let path = self.path(name)?;
        if self.exists(name)? {
            drop(backup::regular(&path, limit)?);
        }
        let temporary = self.root.join(format!(".{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temporary)
                .map_err(invalid)?;
            let mut writer = Bounded {
                file,
                remaining: limit,
                hash: Sha256::new(),
            };
            write(&mut writer)?;
            writer.file.sync_all().map_err(invalid)?;
            let hash = hex::encode(writer.hash.finalize());
            drop(writer.file);
            fs::rename(&temporary, path).map_err(invalid)?;
            File::open(&self.root)
                .map_err(invalid)?
                .sync_all()
                .map_err(invalid)?;
            Ok(hash)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    pub(crate) fn hash(&self, name: &str, limit: usize) -> Result<String> {
        let mut file = backup::regular(&self.path(name)?, limit)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 8192];
        let mut remaining = limit;
        loop {
            let count = file.read(&mut buffer).map_err(invalid)?;
            if count == 0 {
                break;
            }
            if count > remaining {
                return Err(crate::plugin_management::ManagementError::Backup);
            }
            remaining -= count;
            hash.update(&buffer[..count]);
        }
        Ok(hex::encode(hash.finalize()))
    }
    pub(crate) fn cleanup_temporaries(&self) -> Result<()> {
        for (index, entry) in fs::read_dir(&self.root)
            .map_err(invalid)?
            .take(17)
            .enumerate()
        {
            if index == 16 {
                return Err(crate::plugin_management::ManagementError::Backup);
            }
            let entry = entry.map_err(invalid)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(crate::plugin_management::ManagementError::Backup);
            };
            if let Some(id) = name.strip_prefix('.') {
                if id.len() == 36 && uuid::Uuid::parse_str(id).is_ok() {
                    drop(backup::regular(&entry.path(), backup::LIMIT)?);
                    fs::remove_file(entry.path()).map_err(invalid)?;
                }
            }
        }
        File::open(&self.root)
            .map_err(invalid)?
            .sync_all()
            .map_err(invalid)
    }
    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        fs::remove_file(self.path(name)?).map_err(invalid)?;
        File::open(&self.root)
            .map_err(invalid)?
            .sync_all()
            .map_err(invalid)
    }
}
struct Bounded {
    file: File,
    remaining: usize,
    hash: Sha256,
}
impl std::io::Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other("restore record limit"));
        }
        let count = self.file.write(bytes)?;
        self.remaining -= count;
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("bkm-journal-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn files(&self) -> Files {
            Files::new(&Repository::new(
                &self.0.join("plugins.db"),
                self.0.join("bot.toml"),
            ))
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn durable_records_are_private_bounded_and_atomically_replaced() {
        use std::os::unix::fs::PermissionsExt as _;
        let f = Fixture::new();
        let files = f.files();
        let hash = files.write("outcome.json", &"old", 32).unwrap();
        assert_eq!(files.hash("outcome.json", 32).unwrap(), hash);
        assert_eq!(
            fs::metadata(files.path("outcome.json").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(files.write("outcome.json", &"too large", 8).is_err());
        assert_eq!(files.read::<String>("outcome.json", 32).unwrap(), "old");
        assert_eq!(fs::read_dir(&files.root).unwrap().count(), 1);
        files.write("outcome.json", &"new", 32).unwrap();
        assert_eq!(files.read::<String>("outcome.json", 32).unwrap(), "new");
        assert!(files.bytes("../../escape", b"bad").is_err());
        files.remove("outcome.json").unwrap();
        assert!(!files.exists("outcome.json").unwrap());
    }
    #[test]
    fn bare_relative_repository_parent_is_current_directory() {
        let root = PathBuf::from(format!(".bkm-relative-fixture-{}", uuid::Uuid::new_v4()));
        let repository = Repository {
            root: root.clone(),
            config: root.join("unused"),
        };
        repository.directory().unwrap();
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn no_links_special_files_or_unbounded_orphan_enumeration() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let files = f.files();
        let outside = f.0.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        symlink(&outside, files.root.join("journal.json")).unwrap();
        assert!(files.write("journal.json", &"bad", 32).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        fs::remove_file(files.root.join("journal.json")).unwrap();
        let orphan = files.root.join(format!(".{}", uuid::Uuid::new_v4()));
        fs::write(&orphan, b"orphan").unwrap();
        files.cleanup_temporaries().unwrap();
        assert!(!orphan.exists());
        for i in 0..17 {
            fs::write(files.root.join(format!("unknown{i}")), b"").unwrap();
        }
        assert!(files.cleanup_temporaries().is_err());
    }
}
