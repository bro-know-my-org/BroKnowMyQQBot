use super::*;
use crate::plugin_management::PluginManager;
use base64::Engine as _;
use plugin_host::{LogicalArchive, PluginStore, StaticPluginHost};
use std::os::unix::fs::PermissionsExt as _;

fn package_bytes() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(
            include_str!("../../test-support/wasm-plugins/ping/component.bkm-plugin.b64").trim(),
        )
        .unwrap()
}

#[tokio::test]
async fn two_databases_with_different_auth_boundaries_never_share_artifacts() {
    let f = Fixture::new();
    let artifact = f.export().await;
    let other = Repository::new(&f.root.join("other.db"), f.repo.config.clone());
    assert_ne!(other.root, f.repo.root);
    assert!(other.list().unwrap().is_empty());
    assert!(other.open(&artifact.id).is_err());
    assert_eq!(f.repo.list().unwrap().len(), 1);
}

#[tokio::test]
async fn future_artifact_mtime_does_not_poison_list_open_or_create() {
    let f = Fixture::new();
    let artifact = f.export().await;
    let file = File::open(f.repo.path(&artifact.id).unwrap()).unwrap();
    file.set_times(
        fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(86_400)),
    )
    .unwrap();
    assert_eq!(f.repo.list().unwrap().len(), 1);
    assert!(f.repo.open(&artifact.id).is_ok());
    f.export().await;
    assert_eq!(f.repo.list().unwrap().len(), 2);
}

#[tokio::test]
async fn declared_package_drift_refuses_a_mixed_runtime_and_saved_backup() {
    let mut f = Fixture::new();
    let path = f.root.join("plugin.bkm-plugin");
    let bytes = package_bytes();
    fs::write(&path, &bytes).unwrap();
    let declaration = f.root.join("plugins.toml");
    fs::write(
        &declaration,
        "bundled=[]\n[[wasm]]\npackage='plugin.bkm-plugin'\ninstance_id='dev.bkm.wasm-ping/file'\n",
    )
    .unwrap();
    let mut host = StaticPluginHost::new(f.store.clone());
    let loaded = crate::plugins::load_plugins(&mut host, &f.store, Some(&declaration))
        .await
        .unwrap();
    assert_eq!(loaded.packages.len(), 1);
    f.manager = PluginManager::new(
        host,
        f.store.clone(),
        loaded.packages,
        loaded.file_instances,
    )
    .with_marketplace(
        f.root.join("plugins.db"),
        Some(declaration.clone()),
        "https://example.com/index.json".into(),
    )
    .unwrap();
    let original = fs::read(&declaration).unwrap();
    fs::write(&declaration, "bundled=[]\n").unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .unwrap();
    assert!(
        f.manager
            .create_backup(f.repo.clone(), permit)
            .await
            .is_err()
    );
    fs::write(&declaration, original).unwrap();
    let package = plugin_host::ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
    for (name, value) in package.files() {
        zip.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(value).unwrap();
    }
    zip.start_file(
        "assets/backup-drift",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(b"changed after loading").unwrap();
    let changed = zip.finish().unwrap().into_inner();
    assert_ne!(
        plugin_host::ValidatedPluginPackage::from_bytes(&changed)
            .unwrap()
            .package_sha256(),
        package.package_sha256()
    );
    fs::write(path, changed).unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .unwrap();
    assert!(
        f.manager
            .create_backup(f.repo.clone(), permit)
            .await
            .is_err()
    );
    assert!(f.repo.list().unwrap().is_empty());
    f.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn individually_valid_packages_cannot_grow_the_buffer_past_the_archive_budget() {
    let f = Fixture::new();
    let original = plugin_host::ValidatedPluginPackage::from_bytes(&package_bytes()).unwrap();
    for index in 0..5 {
        let path = f.root.join(format!("large-{index}.bkm-plugin"));
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, value) in original.files() {
            writer.start_file(name, options).unwrap();
            writer.write_all(value).unwrap();
        }
        writer
            .start_file(format!("assets/fixture-{index}"), options)
            .unwrap();
        std::io::copy(&mut std::io::repeat(0).take(31 * 1024 * 1024), &mut writer).unwrap();
        writer.finish().unwrap();
        let package = plugin_host::ValidatedPluginPackage::from_path(&path).unwrap();
        let item:plugin_host::PluginInstallation=serde_json::from_value(serde_json::json!({
            "plugin_id":package.manifest().id,"metadata":package.manifest().metadata,"instance_id":format!("dev.test/{index}"),"version":package.manifest().version,
            "package_path":path,"package_sha256":package.package_sha256(),"source":"local","trust_level":"local-wasm","signature_status":"unsigned",
            "requested_permissions":[],"granted_permissions":[],"config":{},"enabled":false,"installed_at_ms":1,"updated_at_ms":1
        })).unwrap();
        f.store.upsert_installation(&item).unwrap();
    }
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .unwrap();
    assert!(
        f.manager
            .create_backup(f.repo.clone(), permit)
            .await
            .is_err()
    );
    assert!(f.repo.list().unwrap().is_empty());
}
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

struct Fixture {
    root: PathBuf,
    repo: Repository,
    manager: PluginManager,
    store: PluginStore,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bkm-backup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let path = root.join("bot.toml");
        fs::write(&path, "# keep comments\n[commands]\nprefix='!!'\n[management]\nlisten='127.0.0.1:12345'\nauto_port=true\nauth='none'\n[plugins]\ndatabase='host-only.db'\n[logging.files]\ndirectory='host-only-logs'\nmessage_content=true\n").unwrap();
        let database = root.join("plugins.db");
        let store = PluginStore::open(&database).unwrap();
        let manager = PluginManager::new(
            StaticPluginHost::new(store.clone()),
            store.clone(),
            BTreeMap::new(),
            BTreeSet::new(),
        )
        .with_marketplace(
            database.clone(),
            None,
            "https://example.com/index.json".into(),
        )
        .unwrap();
        let repo = Repository::new(&database, path);
        Self {
            root,
            repo,
            manager,
            store,
        }
    }
    async fn export(&self) -> Artifact {
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        self.manager
            .create_backup(self.repo.clone(), permit)
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn actual_export_is_logical_private_and_excludes_host_configuration_and_execution() {
    let f = Fixture::new();
    f.store
        .commit(
            "dev.test/default",
            "fixture",
            &[plugin_api::StateOp::Put {
                key: "private".into(),
                value: b"business-value".to_vec(),
                expected_revision: None,
            }],
            &[],
            plugin_host::CommitOptions::new(4096),
        )
        .unwrap();
    let artifact = f.export().await;
    let path = f.repo.path(&artifact.id).unwrap();
    let archive = LogicalArchive::from_bytes(&read(&path, LIMIT).unwrap()).unwrap();
    assert_eq!(archive.snapshot.state[0].value, b"business-value");
    let bot = &archive.configuration["bot"];
    assert_eq!(bot["commands"]["prefix"], "!!");
    assert!(bot.get("management").is_none());
    assert!(bot.get("plugins").is_none());
    assert!(bot["logging"]["files"].get("directory").is_none());
    assert!(bot["logging"]["files"].get("message_content").is_none());
    assert!(bot["qq"]["webhook"].get("listen").is_none());
    assert!(bot["onebot11"].get("listen").is_none());
    assert_eq!(
        fs::metadata(&f.repo.root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(f.repo.list().unwrap().len(), 1);
    assert!(f.repo.open("../bot.toml").is_err());
    assert!(f.repo.open(&uuid::Uuid::new_v4().to_string()).is_err());
}

use std::os::unix::fs::symlink;

#[test]
fn fifo_declarations_and_disabled_package_are_rejected_without_waiting_for_a_writer() {
    const CHILD: &str = "BKMQB_TEST_FIFO_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        assert!(crate::plugins::backup_declarations(Some(&root.join("fifo"))).is_err());
        assert!(crate::plugins::backup_declarations(Some(&root.join("plugins.toml"))).is_err());
        return;
    }
    let f = Fixture::new();
    let fifo = f.root.join("fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let declarations = f.root.join("plugins.toml");
    fs::write(
        &declarations,
        "[[wasm]]\npackage='fifo'\ninstance_id='dev.test/default'\nenabled=false\n",
    )
    .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "backup::tests::fifo_declarations_and_disabled_package_are_rejected_without_waiting_for_a_writer"])
        .env_clear().env(CHILD, &f.root).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("FIFO reader blocked");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
#[tokio::test]
async fn retention_is_finite_and_symlinks_never_read_or_replaced() {
    let f = Fixture::new();
    for _ in 0..4 {
        f.export().await;
    }
    assert!(f.repo.save(b"fifth").is_err());
    assert_eq!(f.repo.list().unwrap().len(), 4);
    let id = f.repo.list().unwrap()[0].id.clone();
    let path = f.repo.path(&id).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&f.repo.config, &path).unwrap();
    assert!(f.repo.open(&id).is_err());
    assert!(f.repo.list().is_err());
    assert!(
        fs::read_to_string(&f.repo.config)
            .unwrap()
            .contains("# keep comments")
    );
}

#[tokio::test]
async fn configuration_lock_conflict_fails_without_partial_archive_and_can_retry() {
    let f = Fixture::new();
    let lock = crate::config::ConfigFileLock::acquire(&f.repo.config).unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .acquire_owned()
        .await
        .unwrap();
    assert!(
        f.manager
            .create_backup(f.repo.clone(), permit)
            .await
            .is_err()
    );
    assert!(f.repo.list().unwrap().is_empty());
    drop(lock);
    f.export().await;
    assert_eq!(f.repo.list().unwrap().len(), 1);
}

#[test]
fn public_or_symlinked_private_root_is_rejected() {
    let f = Fixture::new();
    fs::create_dir(&f.repo.root).unwrap();
    fs::set_permissions(&f.repo.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(f.repo.list().is_err());
    fs::remove_dir(&f.repo.root).unwrap();
    symlink(&f.root, &f.repo.root).unwrap();
    assert!(f.repo.list().is_err());
}
