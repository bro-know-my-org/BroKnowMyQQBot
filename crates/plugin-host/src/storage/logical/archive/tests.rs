use super::*;
use crate::{LogicalStateEntry, PluginInstallation};
use base64::Engine as _;
use serde_json::json;

fn fixture() -> LogicalArchive {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!(
                "../../../../../../test-support/wasm-plugins/ping/component.bkm-plugin.b64"
            )
            .trim(),
        )
        .unwrap();
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let item: PluginInstallation = serde_json::from_value(json!({
        "plugin_id":package.manifest().id, "metadata":package.manifest().metadata,
        "instance_id":"dev.test.plugin/default", "version":package.manifest().version,
        "package_path":"reference-only", "package_sha256":package.package_sha256(),
        "source":"local", "trust_level":"local-wasm", "signature_status":"unsigned",
        "requested_permissions":["message.reply"], "granted_permissions":[],
        "config":{"greeting":"business-sensitive-value"}, "enabled":false,
        "installed_at_ms":1, "updated_at_ms":1,
    }))
    .unwrap();
    let state = LogicalStateEntry {
        instance_id: item.instance_id.clone(),
        key: "binary".into(),
        value: vec![0, 255, 7],
        revision: 9,
    };
    LogicalArchive::create(
        BTreeMap::from([("prefix".into(), json!("!!"))]),
        LogicalSnapshot {
            installations: vec![item],
            state: vec![state],
        },
        BTreeMap::from([(package.package_sha256().to_owned(), bytes)]),
        100,
    )
    .unwrap()
}

fn rewrite(
    source: &[u8],
    change: impl Fn(&str, Vec<u8>) -> (String, Vec<u8>, SimpleFileOptions),
) -> Vec<u8> {
    let mut archive = ZipArchive::new(Cursor::new(source)).unwrap();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();
        let (name, bytes, options) = change(file.name(), bytes);
        writer.start_file(name, options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}
fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
}

#[test]
fn versioned_archive_roundtrips_opaque_business_values_and_fixed_packages() {
    let original = fixture();
    let bytes = original.to_bytes().unwrap();
    let restored = LogicalArchive::from_bytes(&bytes).unwrap();
    assert_eq!(restored.snapshot, original.snapshot);
    assert_eq!(restored.configuration, original.configuration);
    assert_eq!(restored.packages, original.packages);
    assert_eq!(restored.manifest.format_version, 1);
    assert_eq!(restored.manifest.installation_count, 1);
    assert_eq!(restored.manifest.state_rows, 1);
    assert_eq!(restored.manifest.exclusions, EXCLUSIONS);
}

#[test]
fn missing_unreferenced_hash_changed_and_identity_changed_packages_fail() {
    let mut archive = fixture();
    archive.snapshot.installations[0].plugin_id = "dev.other.plugin".into();
    assert!(archive.to_bytes().is_err());
    archive = fixture();
    archive.packages.values_mut().next().unwrap()[0] ^= 1;
    assert!(archive.to_bytes().is_err());
    archive = fixture();
    archive.packages.clear();
    assert!(archive.to_bytes().is_err());
    archive = fixture();
    archive.snapshot.installations.clear();
    assert!(archive.to_bytes().is_err());
}

#[test]
fn unknown_formats_counts_and_payload_corruption_are_rejected() {
    let original = fixture().to_bytes().unwrap();
    for field in [
        "format_version",
        "state_rows",
        "installation_count",
        "package_count",
    ] {
        let changed = rewrite(&original, |name, mut bytes| {
            if name == "manifest.json" {
                let mut manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                manifest[field] = json!(99);
                bytes = serde_json::to_vec(&manifest).unwrap();
            }
            (name.into(), bytes, stored())
        });
        assert!(LogicalArchive::from_bytes(&changed).is_err(), "{field}");
    }
    let changed = rewrite(&original, |name, mut bytes| {
        if name == "logical.json" {
            bytes[0] ^= 1;
        }
        (name.into(), bytes, stored())
    });
    assert!(LogicalArchive::from_bytes(&changed).is_err());
}

#[test]
fn traversal_nonregular_compressed_and_duplicate_paths_fail() {
    let original = fixture().to_bytes().unwrap();
    for bad in [
        "../logical.json",
        "/logical.json",
        "C:/logical.json",
        "packages/../x",
        "logical\\json",
        "logical.json/",
    ] {
        let changed = rewrite(&original, |name, bytes| {
            (
                if name == "logical.json" {
                    bad.into()
                } else {
                    name.into()
                },
                bytes,
                stored(),
            )
        });
        assert!(LogicalArchive::from_bytes(&changed).is_err(), "{bad}");
    }
    let compressed = rewrite(&original, |name, bytes| {
        (
            name.into(),
            bytes,
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
    });
    assert!(LogicalArchive::from_bytes(&compressed).is_err());
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_symlink("logical.json", "any-target", stored())
        .unwrap();
    zip.start_file("manifest.json", stored()).unwrap();
    zip.write_all(b"{}").unwrap();
    assert!(LogicalArchive::from_bytes(&zip.finish().unwrap().into_inner()).is_err());
    let duplicate = rewrite(&original, |name, bytes| {
        (
            if name == "logical.json" {
                "manifest.jsox".into()
            } else {
                name.into()
            },
            bytes,
            stored(),
        )
    });
    // Equal-length raw replacement bypasses ZipWriter's own duplicate-name rejection.
    let duplicate = duplicate
        .windows(13)
        .enumerate()
        .filter_map(|(i, w)| (w == b"manifest.jsox").then_some(i))
        .collect::<Vec<_>>()
        .into_iter()
        .fold(duplicate, |mut b, i| {
            b[i..i + 13].copy_from_slice(b"manifest.json");
            b
        });
    assert!(LogicalArchive::from_bytes(&duplicate).is_err());
}

#[test]
fn preflight_rejects_oversized_central_metadata_before_zip_parsing() {
    let original = fixture().to_bytes().unwrap();
    for offset in [8, 10, 12, 16, 20] {
        let mut changed = original.clone();
        let end = changed.len() - 22;
        changed[end + offset] = 255;
        changed[end + offset + 1] = 255;
        assert!(LogicalArchive::from_bytes(&changed).is_err(), "{offset}");
    }
    let mut archive = fixture();
    archive
        .configuration
        .insert("oversized".into(), json!("x".repeat(33 * 1024)));
    assert!(archive.to_bytes().is_err());
}
