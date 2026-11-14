//! Version 1 is a closed, non-extracting ZIP format. Nested plugin packages retain
//! their existing independent compression and validation limits.
use super::LogicalSnapshot;
use crate::{MAX_PLUGIN_PACKAGE_BYTES, ValidatedPluginPackage};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read as _, Write as _},
};
use thiserror::Error;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub const MAX_LOGICAL_ARCHIVE_BYTES: usize = 128 * 1024 * 1024;
const MAX_DATA_BYTES: usize = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 128 * 1024;
const MAX_ENTRIES: usize = 258;
const SCOPE: &str = "bot-settings+plugin-installations+verified-packages+private-kv";
const EXCLUSIONS: &[&str] = &[
    "secrets.env",
    "process-environment",
    "host-paths-and-auth",
    "logs",
    "command-ledger",
    "outbox",
    "deliveries",
    "scheduled-tasks",
    "health",
];

#[derive(Debug, Error)]
pub enum LogicalArchiveError {
    #[error("logical archive is invalid, unsupported, corrupt or exceeds resource limits")]
    Invalid,
}
type Result<T> = std::result::Result<T, LogicalArchiveError>;
fn invalid<E>(_: E) -> LogicalArchiveError {
    LogicalArchiveError::Invalid
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    size: usize,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalArchiveManifest {
    pub format_version: u32,
    pub created_at_ms: i64,
    pub scope: String,
    pub exclusions: Vec<String>,
    pub installation_count: usize,
    pub state_rows: usize,
    pub package_count: usize,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Data {
    configuration: BTreeMap<String, serde_json::Value>,
    snapshot: LogicalSnapshot,
}

#[derive(Debug)]
pub struct LogicalArchive {
    pub manifest: LogicalArchiveManifest,
    /// The application must supply and validate an explicit non-credential allowlist.
    pub configuration: BTreeMap<String, serde_json::Value>,
    pub snapshot: LogicalSnapshot,
    pub packages: BTreeMap<String, Vec<u8>>,
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for BoundedBytes {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other("archive limit"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    // Allocate the exact permitted capacity once, avoiding geometric over-allocation.
    let mut output = BoundedBytes {
        bytes: Vec::with_capacity(limit),
        limit,
    };
    serde_json::to_writer(&mut output, value).map_err(invalid)?;
    Ok(output.bytes)
}

impl LogicalArchive {
    pub fn create(
        configuration: BTreeMap<String, serde_json::Value>,
        snapshot: LogicalSnapshot,
        packages: BTreeMap<String, Vec<u8>>,
        created_at_ms: i64,
    ) -> Result<Self> {
        let archive = Self {
            manifest: LogicalArchiveManifest {
                format_version: 1,
                created_at_ms,
                scope: SCOPE.into(),
                exclusions: EXCLUSIONS.iter().map(|s| (*s).into()).collect(),
                installation_count: snapshot.installations.len(),
                state_rows: snapshot.state.len(),
                package_count: packages.len(),
                entries: vec![],
            },
            configuration,
            snapshot,
            packages,
        };
        archive.validate()?;
        Ok(archive)
    }

    fn validate(&self) -> Result<()> {
        self.snapshot.validate().map_err(invalid)?;
        if self.manifest.created_at_ms < 0
            || self.packages.len() > 256
            || self.manifest.format_version != 1
            || self.manifest.scope != SCOPE
            || self.manifest.exclusions != EXCLUSIONS
            || self.manifest.installation_count != self.snapshot.installations.len()
            || self.manifest.state_rows != self.snapshot.state.len()
            || self.manifest.package_count != self.packages.len()
        {
            return Err(LogicalArchiveError::Invalid);
        }
        encode(&self.configuration, 1024 * 1024)?;
        let required: BTreeSet<_> = self
            .snapshot
            .installations
            .iter()
            .map(|i| i.package_sha256.as_str())
            .collect();
        if required.len() != self.packages.len() {
            return Err(LogicalArchiveError::Invalid);
        }
        let mut total = 0;
        for (digest, bytes) in &self.packages {
            total += bytes.len();
            if !required.contains(digest.as_str())
                || bytes.len() > MAX_PLUGIN_PACKAGE_BYTES
                || total > MAX_LOGICAL_ARCHIVE_BYTES
                || hash(bytes) != *digest
            {
                return Err(LogicalArchiveError::Invalid);
            }
            let package = ValidatedPluginPackage::from_bytes(bytes).map_err(invalid)?;
            for item in self
                .snapshot
                .installations
                .iter()
                .filter(|i| i.package_sha256 == *digest)
            {
                if package.manifest().id.as_str() != item.plugin_id
                    || package.manifest().version != item.version
                {
                    return Err(LogicalArchiveError::Invalid);
                }
            }
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let data = encode(
            &Data {
                configuration: self.configuration.clone(),
                snapshot: self.snapshot.clone(),
            },
            MAX_DATA_BYTES,
        )?;
        let mut entries = vec![Entry {
            name: "logical.json".into(),
            size: data.len(),
            sha256: hash(&data),
        }];
        for (digest, bytes) in &self.packages {
            entries.push(Entry {
                name: format!("packages/{digest}.bkm-plugin"),
                size: bytes.len(),
                sha256: digest.clone(),
            });
        }
        let mut manifest = self.manifest.clone();
        manifest.entries = entries;
        let manifest_bytes = encode(&manifest, MAX_MANIFEST_BYTES)?;
        let total: usize = manifest.entries.iter().map(|e| e.size).sum();
        if total + manifest_bytes.len() + MAX_MANIFEST_BYTES > MAX_LOGICAL_ARCHIVE_BYTES {
            return Err(LogicalArchiveError::Invalid);
        }
        let mut writer = ZipWriter::new(Cursor::new(Vec::with_capacity(
            total + manifest_bytes.len() + MAX_MANIFEST_BYTES,
        )));
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o600);
        for (name, bytes) in [
            ("manifest.json", manifest_bytes.as_slice()),
            ("logical.json", data.as_slice()),
        ] {
            writer.start_file(name, options).map_err(invalid)?;
            writer.write_all(bytes).map_err(invalid)?;
        }
        for (digest, bytes) in &self.packages {
            writer
                .start_file(format!("packages/{digest}.bkm-plugin"), options)
                .map_err(invalid)?;
            writer.write_all(bytes).map_err(invalid)?;
        }
        let result = writer.finish().map_err(invalid)?.into_inner();
        if result.len() > MAX_LOGICAL_ARCHIVE_BYTES {
            return Err(LogicalArchiveError::Invalid);
        }
        Ok(result)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        preflight(bytes)?;
        let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(invalid)?;
        if !(2..=MAX_ENTRIES).contains(&zip.len()) {
            return Err(LogicalArchiveError::Invalid);
        }
        let mut contents = BTreeMap::new();
        let mut total = 0;
        for index in 0..zip.len() {
            let mut file = zip.by_index(index).map_err(invalid)?;
            let name = std::str::from_utf8(file.name_raw())
                .map_err(invalid)?
                .to_owned();
            let limit = entry_limit(&name)?;
            let size = usize::try_from(file.size()).map_err(invalid)?;
            if size > limit {
                return Err(LogicalArchiveError::Invalid);
            }
            total += size;
            if file.compression() != CompressionMethod::Stored
                || file.compressed_size() != file.size()
                || !file.is_file()
                || file.is_symlink()
                || file.encrypted()
                || size > limit
                || total > MAX_LOGICAL_ARCHIVE_BYTES
                || contents.contains_key(&name)
                || file
                    .unix_mode()
                    .is_some_and(|m| m & 0o170_000 != 0 && m & 0o170_000 != 0o100_000)
            {
                return Err(LogicalArchiveError::Invalid);
            }
            let mut value = Vec::with_capacity(size);
            file.by_ref()
                .take(u64::try_from(size + 1).map_err(invalid)?)
                .read_to_end(&mut value)
                .map_err(invalid)?;
            if value.len() != size {
                return Err(LogicalArchiveError::Invalid);
            }
            contents.insert(name, value);
        }
        let manifest: LogicalArchiveManifest = serde_json::from_slice(
            &contents
                .remove("manifest.json")
                .ok_or(LogicalArchiveError::Invalid)?,
        )
        .map_err(invalid)?;
        if manifest.format_version != 1
            || manifest.scope != SCOPE
            || manifest.exclusions != EXCLUSIONS
            || manifest.entries.len() != contents.len()
        {
            return Err(LogicalArchiveError::Invalid);
        }
        let mut names = BTreeSet::new();
        for entry in &manifest.entries {
            let value = contents
                .get(&entry.name)
                .ok_or(LogicalArchiveError::Invalid)?;
            if !names.insert(&entry.name)
                || value.len() != entry.size
                || hash(value) != entry.sha256
            {
                return Err(LogicalArchiveError::Invalid);
            }
        }
        let data: Data = serde_json::from_slice(
            &contents
                .remove("logical.json")
                .ok_or(LogicalArchiveError::Invalid)?,
        )
        .map_err(invalid)?;
        let packages = contents
            .into_iter()
            .map(|(name, bytes)| (name[9..73].to_owned(), bytes))
            .collect();
        let result = Self {
            manifest,
            configuration: data.configuration,
            snapshot: data.snapshot,
            packages,
        };
        if result.manifest.installation_count != result.snapshot.installations.len()
            || result.manifest.state_rows != result.snapshot.state.len()
            || result.manifest.package_count != result.packages.len()
        {
            return Err(LogicalArchiveError::Invalid);
        }
        result.validate()?;
        Ok(result)
    }
}

fn entry_limit(name: &str) -> Result<usize> {
    match name {
        "manifest.json" => Ok(MAX_MANIFEST_BYTES),
        "logical.json" => Ok(MAX_DATA_BYTES),
        _ if name.len() == 84
            && name.starts_with("packages/")
            && name.ends_with(".bkm-plugin")
            && name.as_bytes()[9..73]
                .iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c)) =>
        {
            Ok(MAX_PLUGIN_PACKAGE_BYTES)
        }
        _ => Err(LogicalArchiveError::Invalid),
    }
}

// Reject ZIP64, split archives, huge central directories and entry counts before
// ZipArchive constructs its per-entry metadata. Our writer produces this closed form.
fn preflight(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 22 || bytes.len() > MAX_LOGICAL_ARCHIVE_BYTES {
        return Err(LogicalArchiveError::Invalid);
    }
    let end = &bytes[bytes.len() - 22..];
    let u16_at = |n| u16::from_le_bytes([end[n], end[n + 1]]);
    let u32_at = |n| u32::from_le_bytes([end[n], end[n + 1], end[n + 2], end[n + 3]]);
    if &end[..4] != b"PK\x05\x06"
        || u16_at(4) != 0
        || u16_at(6) != 0
        || u16_at(20) != 0
        || u16_at(8) != u16_at(10)
        || !(2..=MAX_ENTRIES).contains(&usize::from(u16_at(10)))
        || u32_at(12) > 128 * 1024
        || u64::from(u32_at(16)) + u64::from(u32_at(12)) + 22 != bytes.len() as u64
    {
        return Err(LogicalArchiveError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
