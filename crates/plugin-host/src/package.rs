//! Safe, read-only validation for `.bkm-plugin` ZIP archives.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Read},
    path::Path,
};

use plugin_api::PluginManifest;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use zip::ZipArchive;

const MAX_PACKAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_UNPACKED_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILE_COUNT: usize = 256;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_CONFIG_SCHEMA_BYTES: usize = 256 * 1024;

pub struct ValidatedPluginPackage {
    manifest: PluginManifest,
    package_sha256: String,
    files: BTreeMap<String, Vec<u8>>,
    config_schema: Option<Value>,
}

impl std::fmt::Debug for ValidatedPluginPackage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ValidatedPluginPackage")
            .field("manifest", &self.manifest)
            .field("package_sha256", &self.package_sha256)
            .field("file_count", &self.files.len())
            .field("has_config_schema", &self.config_schema.is_some())
            .finish()
    }
}

impl ValidatedPluginPackage {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, PluginPackageError> {
        let path = path.as_ref();
        if path.extension().and_then(|extension| extension.to_str()) != Some("bkm-plugin") {
            return Err(PluginPackageError::InvalidExtension);
        }
        let mut file = fs::File::open(path).map_err(PluginPackageError::Io)?;
        let mut bytes = Vec::with_capacity(MAX_PACKAGE_BYTES.min(64 * 1024));
        file.by_ref()
            .take(u64::try_from(MAX_PACKAGE_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut bytes)
            .map_err(PluginPackageError::Io)?;
        if bytes.len() > MAX_PACKAGE_BYTES {
            return Err(PluginPackageError::PackageTooLarge(bytes.len()));
        }
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PluginPackageError> {
        if bytes.len() > MAX_PACKAGE_BYTES {
            return Err(PluginPackageError::PackageTooLarge(bytes.len()));
        }
        let package_sha256 = hex_digest(Sha256::digest(bytes).as_slice());
        let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(PluginPackageError::Zip)?;
        if archive.len() > MAX_FILE_COUNT {
            return Err(PluginPackageError::TooManyFiles(archive.len()));
        }

        let mut paths = BTreeSet::new();
        let mut files = BTreeMap::new();
        let mut unpacked_bytes = 0_usize;
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).map_err(PluginPackageError::Zip)?;
            let raw_name = std::str::from_utf8(file.name_raw())
                .map_err(|_| PluginPackageError::NonUtf8Path)?;
            let path = normalized_archive_path(raw_name, file.is_dir())?;
            if !paths.insert(path.clone()) {
                return Err(PluginPackageError::DuplicatePath(path));
            }
            if file.encrypted() {
                return Err(PluginPackageError::EncryptedFile(path));
            }
            if file.is_symlink() {
                return Err(PluginPackageError::SymbolicLink(path));
            }
            if file.is_dir() {
                if file.size() != 0 {
                    return Err(PluginPackageError::InvalidDirectory(path));
                }
                continue;
            }
            let file_size = usize::try_from(file.size()).unwrap_or(usize::MAX);
            if file_size > MAX_FILE_BYTES {
                return Err(PluginPackageError::FileTooLarge {
                    path,
                    size: file_size,
                });
            }
            unpacked_bytes = unpacked_bytes
                .checked_add(file_size)
                .ok_or(PluginPackageError::UnpackedTooLarge(usize::MAX))?;
            if unpacked_bytes > MAX_UNPACKED_BYTES {
                return Err(PluginPackageError::UnpackedTooLarge(unpacked_bytes));
            }
            let mut data = Vec::with_capacity(file_size);
            file.by_ref()
                .take(u64::try_from(MAX_FILE_BYTES).unwrap_or(u64::MAX) + 1)
                .read_to_end(&mut data)
                .map_err(PluginPackageError::Io)?;
            if data.len() != file_size {
                return Err(PluginPackageError::SizeMismatch {
                    path,
                    declared: file_size,
                    actual: data.len(),
                });
            }
            files.insert(path, data);
        }

        let manifest_bytes = required_file(&files, "plugin.toml")?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PluginPackageError::ManifestTooLarge(manifest_bytes.len()));
        }
        let manifest_source =
            std::str::from_utf8(manifest_bytes).map_err(|_| PluginPackageError::ManifestNotUtf8)?;
        let manifest = PluginManifest::from_toml(manifest_source)
            .map_err(|error| PluginPackageError::Manifest(error.to_string()))?;
        let component = required_file(&files, "component.wasm")?;
        if !wasmparser::Parser::is_component(component)
            || wasmparser::Validator::new()
                .validate_all(component)
                .is_err()
        {
            return Err(PluginPackageError::InvalidComponent);
        }
        let config_schema = files
            .get("config.schema.json")
            .map(|schema| validate_config_schema(schema))
            .transpose()?;

        Ok(Self {
            manifest,
            package_sha256,
            files,
            config_schema,
        })
    }

    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn package_sha256(&self) -> &str {
        &self.package_sha256
    }

    pub fn component(&self) -> &[u8] {
        self.files
            .get("component.wasm")
            .expect("validated package must contain component.wasm")
    }

    pub const fn config_schema(&self) -> Option<&Value> {
        self.config_schema.as_ref()
    }

    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn files(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files
            .iter()
            .map(|(path, data)| (path.as_str(), data.as_slice()))
    }
}

#[derive(Debug, Error)]
pub enum PluginPackageError {
    #[error("plugin package path must end in `.bkm-plugin`")]
    InvalidExtension,
    #[error("plugin package I/O failed")]
    Io(#[source] std::io::Error),
    #[error("plugin package ZIP is invalid")]
    Zip(#[source] zip::result::ZipError),
    #[error("plugin package is {0} bytes and exceeds the Host limit")]
    PackageTooLarge(usize),
    #[error("plugin package contains {0} entries and exceeds the Host limit")]
    TooManyFiles(usize),
    #[error("plugin package path is not UTF-8")]
    NonUtf8Path,
    #[error("plugin package path `{0}` is not canonical and relative")]
    InvalidPath(String),
    #[error("plugin package contains duplicate path `{0}`")]
    DuplicatePath(String),
    #[error("plugin package file `{0}` is encrypted")]
    EncryptedFile(String),
    #[error("plugin package contains symbolic link `{0}`")]
    SymbolicLink(String),
    #[error("plugin package directory `{0}` has non-zero content")]
    InvalidDirectory(String),
    #[error("plugin package file `{path}` is {size} bytes and exceeds the Host limit")]
    FileTooLarge { path: String, size: usize },
    #[error("plugin package expands to {0} bytes and exceeds the Host limit")]
    UnpackedTooLarge(usize),
    #[error("plugin package file `{path}` size mismatch: declared {declared} bytes, read {actual}")]
    SizeMismatch {
        path: String,
        declared: usize,
        actual: usize,
    },
    #[error("plugin package is missing required file `{0}`")]
    MissingFile(&'static str),
    #[error("plugin manifest is {0} bytes and exceeds the Host limit")]
    ManifestTooLarge(usize),
    #[error("plugin manifest is not UTF-8")]
    ManifestNotUtf8,
    #[error("plugin manifest is invalid: {0}")]
    Manifest(String),
    #[error("component.wasm is not a WebAssembly binary")]
    InvalidComponent,
    #[error("config.schema.json is {0} bytes and exceeds the Host limit")]
    ConfigSchemaTooLarge(usize),
    #[error("config.schema.json is invalid: {0}")]
    ConfigSchema(String),
}

fn normalized_archive_path(name: &str, directory: bool) -> Result<String, PluginPackageError> {
    let normalized = if directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains(['\\', '\0'])
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.contains(':'))
    {
        return Err(PluginPackageError::InvalidPath(name.to_owned()));
    }
    Ok(normalized.to_owned())
}

fn required_file<'a>(
    files: &'a BTreeMap<String, Vec<u8>>,
    path: &'static str,
) -> Result<&'a [u8], PluginPackageError> {
    files
        .get(path)
        .map(Vec::as_slice)
        .ok_or(PluginPackageError::MissingFile(path))
}

fn validate_config_schema(bytes: &[u8]) -> Result<Value, PluginPackageError> {
    if bytes.len() > MAX_CONFIG_SCHEMA_BYTES {
        return Err(PluginPackageError::ConfigSchemaTooLarge(bytes.len()));
    }
    let schema: Value = serde_json::from_slice(bytes)
        .map_err(|error| PluginPackageError::ConfigSchema(error.to_string()))?;
    if let Some(reference) = external_schema_reference(&schema) {
        return Err(PluginPackageError::ConfigSchema(format!(
            "external schema reference `{reference}` is not allowed"
        )));
    }
    jsonschema::validator_for(&schema)
        .map_err(|error| PluginPackageError::ConfigSchema(error.to_string()))?;
    Ok(schema)
}

fn external_schema_reference(schema: &Value) -> Option<String> {
    let mut pending = vec![schema];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                for keyword in ["$ref", "$dynamicRef", "$recursiveRef"] {
                    if let Some(reference) = object.get(keyword).and_then(Value::as_str)
                        && !reference.starts_with('#')
                    {
                        return Some(reference.to_owned());
                    }
                }
                pending.extend(object.values());
            }
            Value::Array(values) => pending.extend(values),
            _ => {}
        }
    }
    None
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use sha2::{Digest, Sha256};
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::{PluginPackageError, ValidatedPluginPackage, hex_digest};

    const MANIFEST: &str = r#"
        manifest_version = 1
        id = "dev.bkm.package-fixture"
        version = "0.1.0"
        protocol = ">=1.0,<2.0"

        [metadata]
        default_locale = "en"

        [metadata.locales.en]
        name = "Package Fixture"
    "#;
    const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";

    fn package(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for (path, content) in entries {
            writer.start_file(*path, options).unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn valid_entries() -> Vec<(&'static str, &'static [u8])> {
        vec![
            ("plugin.toml", MANIFEST.as_bytes()),
            ("component.wasm", COMPONENT),
            (
                "config.schema.json",
                br##"{"$defs":{"name":{"type":"string"}},"type":"object","properties":{"name":{"$ref":"#/$defs/name"}}}"##,
            ),
            ("assets/readme.txt", b"fixture asset"),
        ]
    }

    #[test]
    fn validates_package_and_records_content_hash() {
        let bytes = package(&valid_entries());
        let validated = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
        assert_eq!(validated.manifest().id.as_str(), "dev.bkm.package-fixture");
        assert_eq!(validated.component(), COMPONENT);
        assert!(validated.config_schema().is_some());
        assert_eq!(
            validated.file("assets/readme.txt"),
            Some(&b"fixture asset"[..])
        );
        assert_eq!(
            validated.package_sha256(),
            hex_digest(Sha256::digest(&bytes).as_slice())
        );
    }

    #[test]
    fn rejects_traversal_absolute_windows_and_duplicate_paths() {
        for invalid in [
            "../component.wasm",
            "/component.wasm",
            "C:/component.wasm",
            "a\\b",
        ] {
            let bytes = package(&[(invalid, COMPONENT)]);
            assert!(matches!(
                ValidatedPluginPackage::from_bytes(&bytes),
                Err(PluginPackageError::InvalidPath(_))
            ));
        }

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();
        writer.start_file("assets", options).unwrap();
        writer.write_all(b"file").unwrap();
        writer.add_directory("assets/", options).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert!(matches!(
            ValidatedPluginPackage::from_bytes(&bytes),
            Err(PluginPackageError::DuplicatePath(path)) if path == "assets"
        ));
    }

    #[test]
    fn rejects_symbolic_links_and_external_schema_references() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();
        writer
            .add_symlink("component.wasm", "elsewhere.wasm", options)
            .unwrap();
        writer.start_file("plugin.toml", options).unwrap();
        writer.write_all(MANIFEST.as_bytes()).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert!(matches!(
            ValidatedPluginPackage::from_bytes(&bytes),
            Err(PluginPackageError::SymbolicLink(path)) if path == "component.wasm"
        ));

        let bytes = package(&[
            ("plugin.toml", MANIFEST.as_bytes()),
            ("component.wasm", COMPONENT),
            (
                "config.schema.json",
                br#"{"$ref":"https://example.com/config.schema.json"}"#,
            ),
        ]);
        assert!(matches!(
            ValidatedPluginPackage::from_bytes(&bytes),
            Err(PluginPackageError::ConfigSchema(message))
                if message.contains("external schema reference")
        ));
    }

    #[test]
    fn rejects_file_count_and_declared_unpacked_size_limits() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default();
        for index in 0..257 {
            writer
                .start_file(format!("assets/{index}"), options)
                .unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        assert!(matches!(
            ValidatedPluginPackage::from_bytes(&bytes),
            Err(PluginPackageError::TooManyFiles(257))
        ));

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let compressed =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file("component.wasm", compressed).unwrap();
        writer.write_all(&vec![0_u8; 32 * 1024 * 1024 + 1]).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert!(matches!(
            ValidatedPluginPackage::from_bytes(&bytes),
            Err(PluginPackageError::FileTooLarge { path, .. }) if path == "component.wasm"
        ));
    }
}
