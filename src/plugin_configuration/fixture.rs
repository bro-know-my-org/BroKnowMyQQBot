use base64::Engine as _;
use plugin_host::{PluginInstallation, PluginStore, ValidatedPluginPackage};
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Write as _},
    path::PathBuf,
};
use zip::{ZipWriter, write::SimpleFileOptions};
pub(crate) struct Fixture {
    pub(crate) directory: PathBuf,
    pub(crate) store: PluginStore,
    pub(crate) installation: PluginInstallation,
}

impl Fixture {
    pub(crate) fn new(enabled: bool) -> Self {
        let directory =
            std::env::temp_dir().join(format!("bkm-management-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let original = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../../test-support/wasm-plugins/ping/component.bkm-plugin.b64")
                    .trim(),
            )
            .unwrap();
        let original = ValidatedPluginPackage::from_bytes(&original).unwrap();
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in original.files() {
            if name == "config.schema.json" {
                continue;
            }
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.start_file("config.schema.json", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(br#"{"type":"object","properties":{"greeting":{"type":"string"}},"additionalProperties":false}"#).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
        let path = directory.join("plugin.bkm-plugin");
        fs::write(&path, &bytes).unwrap();
        let installation = PluginInstallation {
            plugin_id: package.manifest().id.to_string(),
            metadata: package.manifest().metadata.clone(),
            instance_id: "dev.bkm.wasm-ping/managed".to_owned(),
            version: package.manifest().version.clone(),
            package_path: path.to_str().unwrap().to_owned(),
            package_sha256: package.package_sha256().to_owned(),
            source: "local".to_owned(),
            trust_level: "local-wasm".to_owned(),
            signature_status: "unsigned".to_owned(),
            requested_permissions: package
                .manifest()
                .requested_capabilities()
                .into_iter()
                .collect(),
            granted_permissions: package
                .manifest()
                .requested_capabilities()
                .into_iter()
                .collect(),
            config: BTreeMap::new(),
            enabled,
            installed_at_ms: 1,
            updated_at_ms: 1,
        };
        let store = PluginStore::open(directory.join("plugins.db")).unwrap();
        store.upsert_installation(&installation).unwrap();
        Self {
            directory,
            store,
            installation,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
