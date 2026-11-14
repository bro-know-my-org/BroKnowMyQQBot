use super::*;
use base64::Engine as _;

fn archive_fixture() -> LogicalArchive {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
        )
        .unwrap();
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let manifest = package.manifest();
    let item:PluginInstallation=serde_json::from_value(serde_json::json!({
        "plugin_id":manifest.id,"metadata":manifest.metadata,"instance_id":"dev.bkm.wasm-ping/restored","version":manifest.version,"package_sha256":package.package_sha256(),"package_path":format!("packages/{}.bkm-plugin",package.package_sha256()),"source":"archive","trust_level":"verified-wasm","signature_status":"signed","requested_permissions":manifest.requested_capabilities(),"granted_permissions":manifest.requested_capabilities(),"config":{},"enabled":true,"installed_at_ms":1,"updated_at_ms":1
    })).unwrap();
    LogicalArchive::create(
        BTreeMap::from([
            (
                "bot".into(),
                backup::selected(&config::BotConfig::default()),
            ),
            ("bindings".into(), serde_json::json!([])),
        ]),
        LogicalSnapshot {
            installations: vec![item],
            state: vec![],
        },
        BTreeMap::from([(package.package_sha256().into(), bytes)]),
        1,
    )
    .unwrap()
}
fn prepare(archive: LogicalArchive) -> Result<Prepared> {
    Prepared::new(archive,"# retained\n[management]\nauth='none'\nlisten='127.0.0.1:0'\nauto_port=true\n[plugins]\ndatabase='current.db'\n[logging.files]\ndirectory='current-logs'\nmessage_content=true\n".into(),vec![],vec![],&[])
}
#[tokio::test]
async fn verified_contents_do_not_copy_archived_trust_grants_or_enabled_state() {
    let plan = prepare(archive_fixture()).unwrap();
    plan.validate_guests().await.unwrap();
    let item = &plan.imported.installations[0];
    assert!(!item.enabled);
    assert!(item.granted_permissions.is_empty());
    assert_eq!(item.trust_level, "local-wasm");
    assert_eq!(item.signature_status, "unsigned");
    let saved: config::BotConfig = toml::from_str(&plan.new_config).unwrap();
    assert_eq!(saved.management.auth, config::ManagementAuth::None);
    assert!(saved.management.auto_port);
    assert_eq!(saved.management.listen, "127.0.0.1:0");
    assert_eq!(saved.plugins.database, std::path::Path::new("current.db"));
    assert!(saved.logging.files.message_content);
    assert!(plan.new_config.contains("# retained"));
}
#[test]
fn host_fields_and_body_capture_cannot_enter_a_restore_patch() {
    for field in ["management", "plugins", "unexpected"] {
        let mut archive = archive_fixture();
        archive.configuration.get_mut("bot").unwrap()[field] =
            serde_json::json!({"listen":"0.0.0.0:1"});
        assert!(prepare(archive).is_err());
    }
    let mut archive = archive_fixture();
    archive.configuration.get_mut("bot").unwrap()["logging"]["files"]["message_content"] =
        true.into();
    assert!(prepare(archive).is_err());
    let result = Prepared::new(
        archive_fixture(),
        "[logging.files]\nenabled=false\nmessage_content=true\n".into(),
        vec![],
        vec![],
        &[],
    );
    assert!(result.is_err());
}
#[test]
fn declarations_preserve_current_metadata_and_require_exact_package_identity() {
    let archive = archive_fixture();
    let mut declared = archive.snapshot.installations[0].clone();
    declared.config = BTreeMap::from([("current-owned".into(), true.into())]);
    declared.source = "declaration".into();
    let plan = Prepared::new(archive, String::new(), vec![], vec![declared.clone()], &[]).unwrap();
    assert!(plan.imported.installations.is_empty());
    assert!(plan.inspection.plugins[0].declaration_owned);
    declared.package_sha256 = "0".repeat(64);
    assert!(
        Prepared::new(
            archive_fixture(),
            String::new(),
            vec![],
            vec![declared],
            &[]
        )
        .is_err()
    );
}
#[test]
fn managed_identity_and_current_grants_are_authoritative() {
    let archive = archive_fixture();
    let mut current = archive.snapshot.installations[0].clone();
    current.granted_permissions = vec!["message.reply".into(), "host.invalid".into()];
    let plan = Prepared::new(archive, String::new(), vec![current.clone()], vec![], &[]).unwrap();
    assert_eq!(
        plan.imported.installations[0].granted_permissions,
        vec!["message.reply"]
    );
    current.plugin_id = "dev.other.identity".into();
    assert!(Prepared::new(archive_fixture(), String::new(), vec![current], vec![], &[]).is_err());
}
#[test]
fn package_free_bindings_cannot_replace_current_managed_instances() {
    let mut archive = archive_fixture();
    let item = archive.snapshot.installations.pop().unwrap();
    let manifest = ValidatedPluginPackage::from_bytes(archive.packages.values().next().unwrap())
        .unwrap()
        .manifest()
        .clone();
    archive.packages.clear();
    archive.configuration.insert(
        "bindings".into(),
        serde_json::json!([Binding {
            instance_id: item.instance_id.clone(),
            manifest: manifest.clone(),
            config: BTreeMap::new(),
            grants: BTreeSet::new()
        }]),
    );
    let runtime = vec![PluginInstanceSnapshot {
        instance_id: item.instance_id.clone(),
        manifest,
        state: plugin_host::PluginInstanceState::Ready,
        config: BTreeMap::new(),
        schema: None,
        granted_capabilities: BTreeSet::new(),
    }];
    assert!(Prepared::new(archive, String::new(), vec![item], vec![], &runtime).is_err());
}

#[test]
fn state_only_archive_cannot_bypass_identity_and_manifest_quota_checks() {
    let mut archive = archive_fixture();
    archive.snapshot.installations.clear();
    archive.packages.clear();
    archive.snapshot.state.push(plugin_host::LogicalStateEntry {
        instance_id: "dev.unknown/default".into(),
        key: "data".into(),
        value: vec![1],
        revision: 1,
    });
    assert!(prepare(archive).is_err());
}

#[tokio::test]
async fn declaration_validation_checks_effective_current_config_instead_of_discarded_archive_config()
 {
    use std::io::Write as _;
    let mut archive = archive_fixture();
    let package =
        ValidatedPluginPackage::from_bytes(archive.packages.values().next().unwrap()).unwrap();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
    for (name, bytes) in package.files() {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer
        .start_file(
            "config.schema.json",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    writer
        .write_all(
            br#"{"type":"object","required":["valid"],"properties":{"valid":{"type":"boolean"}}}"#,
        )
        .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let item = &mut archive.snapshot.installations[0];
    item.package_sha256 = package.package_sha256().into();
    item.package_path = format!("packages/{}.bkm-plugin", item.package_sha256);
    let mut current = item.clone();
    current.config.insert("valid".into(), true.into());
    archive.packages = BTreeMap::from([(item.package_sha256.clone(), bytes)]);
    let mut plan = Prepared::new(archive, String::new(), vec![], vec![current], &[]).unwrap();
    plan.validate_guests().await.unwrap();
    plan.declarations[0].config.clear();
    assert!(plan.validate_guests().await.is_err());
}
