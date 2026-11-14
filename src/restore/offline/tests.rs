use super::*;
use base64::Engine as _;
use std::io::Write as _;
struct Fixture {
    root: PathBuf,
    repository: Repository,
    database: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bkm-offline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("plugins.db");
        let repository = Repository::new(&database, root.join("bot.toml"));
        std::fs::write(&repository.config,"# keep comment\n[commands]\nprefix='!'\n[management]\nauth='none'\nlisten='127.0.0.1:0'\nauto_port=true\n[plugins]\ndatabase='current.db'\n").unwrap();
        Self {
            root,
            repository,
            database,
        }
    }
    fn prepare(&self) -> Prepared {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(
                include_str!("../../../test-support/wasm-plugins/ping/component.bkm-plugin.b64")
                    .trim(),
            )
            .unwrap();
        let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        for (name, data) in package.files() {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            if name == "plugin.toml" {
                writer
                    .write_all(
                        std::str::from_utf8(data)
                            .unwrap()
                            .replace("storage_quota_bytes = 0", "storage_quota_bytes = 4096")
                            .as_bytes(),
                    )
                    .unwrap();
            } else {
                writer.write_all(data).unwrap();
            }
        }
        let bytes = writer.finish().unwrap().into_inner();
        let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
        let manifest = package.manifest();
        let path = self.root.join("original.bkm-plugin");
        std::fs::write(&path, &bytes).unwrap();
        let item:PluginInstallation=serde_json::from_value(serde_json::json!({"plugin_id":manifest.id,"metadata":manifest.metadata,"instance_id":"dev.bkm.wasm-ping/restore","version":manifest.version,"package_sha256":package.package_sha256(),"package_path":path,"source":"local","trust_level":"local-wasm","signature_status":"unsigned","requested_permissions":manifest.requested_capabilities(),"granted_permissions":manifest.requested_capabilities(),"config":{},"enabled":true,"installed_at_ms":1,"updated_at_ms":1})).unwrap();
        let store = PluginStore::open(&self.database).unwrap();
        store.upsert_installation(&item).unwrap();
        let connection = rusqlite::Connection::open(&self.database).unwrap();
        connection
            .execute(
                "INSERT INTO plugin_state VALUES (?,?,?,?)",
                rusqlite::params![item.instance_id, "business", b"target".as_slice(), 7],
            )
            .unwrap();
        drop(connection);
        let mut snapshot = store.logical_snapshot().unwrap();
        let expected = snapshot.installations.clone();
        snapshot.state[0].value = b"backup".to_vec();
        snapshot.installations[0].package_path =
            format!("packages/{}.bkm-plugin", package.package_sha256());
        let archive = LogicalArchive::create(
            BTreeMap::from([
                (
                    "bot".into(),
                    backup::selected(&config::BotConfig::default()),
                ),
                ("bindings".into(), serde_json::json!([])),
            ]),
            snapshot,
            BTreeMap::from([(package.package_sha256().into(), bytes)]),
            1,
        )
        .unwrap();
        Prepared::new(
            archive,
            config_text(&self.repository).unwrap(),
            expected,
            vec![],
            &[],
        )
        .unwrap()
    }
    fn accepted(&self) -> Accepted {
        stage(
            self.repository.clone(),
            self.database.clone(),
            None,
            self.prepare(),
        )
        .unwrap()
    }
    fn check(&self, new: bool) {
        let text = config_text(&self.repository).unwrap();
        let saved: config::BotConfig = toml::from_str(&text).unwrap();
        assert_eq!(saved.commands.prefix, if new { "/" } else { "!" });
        assert_eq!(saved.management.auth, config::ManagementAuth::None);
        assert!(saved.management.auto_port);
        assert_eq!(saved.plugins.database, std::path::Path::new("current.db"));
        assert!(text.contains("# keep comment"));
        let store = PluginStore::open(&self.database).unwrap();
        let snapshot = store.logical_snapshot().unwrap();
        assert_eq!(
            snapshot.state[0].value,
            if new { b"backup" } else { b"target" }
        );
        assert_eq!(snapshot.installations[0].enabled, !new);
        if new {
            assert!(snapshot.state[0].revision > 7);
            assert_eq!(snapshot.installations[0].signature_status, "unsigned");
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn offline_apply_is_atomic_retains_private_rollback_and_preserves_host_paths() {
    let f = Fixture::new();
    let accepted = f.accepted();
    apply(&accepted).unwrap();
    f.check(true);
    let files = Files::new(&f.repository).unwrap();
    let result: Outcome = files.read("outcome.json", 4096).unwrap();
    assert_eq!(result.status, "applied");
    assert!(!files.exists("journal.json").unwrap());
    let rollback = LogicalArchive::from_bytes(
        &backup::read(&files.root.join("rollback.bkm-backup"), backup::LIMIT).unwrap(),
    )
    .unwrap();
    assert_eq!(rollback.snapshot.state[0].value, b"target");
    recover(&f.repository, &f.database).unwrap();
    f.check(true);
}
#[test]
fn config_and_installation_cas_refuse_before_irreversible_writes() {
    for kind in ["config", "installation", "accepted", "archive"] {
        let f = Fixture::new();
        let accepted = f.accepted();
        match kind {
            "config" => {
                std::fs::write(&f.repository.config, "[commands]\nprefix='changed'\n").unwrap();
            }
            "installation" => {
                let store = PluginStore::open(&f.database).unwrap();
                let mut item = store.installations().unwrap().remove(0);
                item.updated_at_ms += 1;
                store.upsert_installation(&item).unwrap();
            }
            "accepted" => {
                Files::new(&f.repository)
                    .unwrap()
                    .bytes("accepted.json", b"{}")
                    .unwrap();
            }
            _ => {
                Files::new(&f.repository)
                    .unwrap()
                    .bytes("archive.bkm-backup", b"bad")
                    .unwrap();
            }
        }
        assert!(apply(&accepted).is_err());
        let files = Files::new(&f.repository).unwrap();
        assert!(!files.exists("journal.json").unwrap());
        assert!(!files.exists("rollback.bkm-backup").unwrap());
        assert_eq!(
            PluginStore::open(&f.database)
                .unwrap()
                .logical_snapshot()
                .unwrap()
                .state[0]
                .value,
            b"target"
        );
    }
}
#[test]
fn crash_at_each_commit_boundary_recovers_old_or_complete_new_state() {
    const CHILD: &str = "BKMQB_RESTORE_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let database = root.join("plugins.db");
        let repository = Repository::new(&database, root.join("bot.toml"));
        let files = Files::new(&repository).unwrap();
        let accepted = Accepted {
            repository,
            database,
            declarations: None,
            operation_id: files
                .read::<Outcome>("outcome.json", 4096)
                .unwrap()
                .operation_id,
            work_hash: files.hash("accepted.json", backup::LIMIT).unwrap(),
        };
        apply(&accepted).unwrap();
        panic!("injected crash must exit");
    }
    for point in ["journal", "config", "commit"] {
        let f = Fixture::new();
        let accepted = f.accepted();
        drop(accepted);
        let mut child=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","restore::offline::tests::crash_at_each_commit_boundary_recovers_old_or_complete_new_state","--nocapture"]).env(CHILD,&f.root).env("BKMQB_TEST_RESTORE_CRASH",point).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("crash fixture timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(92), "{point}");
        recover(&f.repository, &f.database).unwrap();
        f.check(point == "commit");
        recover(&f.repository, &f.database).unwrap();
        f.check(point == "commit");
    }
}
#[test]
fn interrupted_staging_does_not_apply_and_locks_fail_without_deadlock() {
    let f = Fixture::new();
    let accepted = f.accepted();
    let lock = config::ConfigFileLock::acquire(&f.repository.config).unwrap();
    assert!(apply(&accepted).is_err());
    drop(lock);
    f.check(false);
    recover(&f.repository, &f.database).unwrap();
    assert_eq!(
        Files::new(&f.repository)
            .unwrap()
            .read::<Outcome>("outcome.json", 4096)
            .unwrap()
            .status,
        "interrupted"
    );
    f.check(false);
}

#[test]
fn missing_or_unknown_database_retains_journal_and_does_not_create_state() {
    for missing in [true, false] {
        let f = Fixture::new();
        let prepared = f.prepare();
        let files = Files::new(&f.repository).unwrap();
        files
            .write(
                "journal.json",
                &Journal {
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    old_config: prepared.old_config,
                    new_config: prepared.new_config,
                },
                3 * 1024 * 1024,
            )
            .unwrap();
        std::fs::remove_file(&f.database).unwrap();
        if !missing {
            std::fs::write(&f.database, b"").unwrap();
        }
        assert!(recover(&f.repository, &f.database).is_err());
        assert!(files.exists("journal.json").unwrap());
        assert_eq!(f.database.exists(), !missing);
    }
}
#[test]
fn stale_accepted_handle_cannot_modify_a_later_pending_operation() {
    let f = Fixture::new();
    let first = f.accepted();
    apply(&first).unwrap();
    let files = Files::new(&f.repository).unwrap();
    let archive = LogicalArchive::from_bytes(
        &backup::read(&files.root.join("archive.bkm-backup"), backup::LIMIT).unwrap(),
    )
    .unwrap();
    let prepared = Prepared::new(
        archive,
        config_text(&f.repository).unwrap(),
        PluginStore::open(&f.database)
            .unwrap()
            .installations()
            .unwrap(),
        vec![],
        &[],
    )
    .unwrap();
    let next = stage(f.repository.clone(), f.database.clone(), None, prepared).unwrap();
    assert!(apply(&first).is_err());
    let outcome: Outcome = files.read("outcome.json", 4096).unwrap();
    assert_eq!(outcome.operation_id, next.operation_id);
    assert_eq!(outcome.status, "pending");
    apply(&next).unwrap();
}

#[test]
fn relative_config_apply_and_recovery_work_in_an_isolated_working_directory() {
    const CHILD: &str = "BKMQB_RELATIVE_RESTORE";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let database = root.join("plugins.db");
        let repository = Repository::new(&database, PathBuf::from("bot.toml"));
        let files = Files::new(&repository).unwrap();
        let work: Work = files.read("accepted.json", backup::LIMIT).unwrap();
        let accepted = Accepted {
            repository: repository.clone(),
            database: database.clone(),
            declarations: None,
            operation_id: work.operation_id.clone(),
            work_hash: files.hash("accepted.json", backup::LIMIT).unwrap(),
        };
        apply(&accepted).unwrap();
        files
            .write(
                "journal.json",
                &Journal {
                    operation_id: work.operation_id,
                    old_config: work.old_config,
                    new_config: work.new_config,
                },
                3 * 1024 * 1024,
            )
            .unwrap();
        recover(&repository, &database).unwrap();
        return;
    }
    let f = Fixture::new();
    f.accepted();
    let mut child=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","restore::offline::tests::relative_config_apply_and_recovery_work_in_an_isolated_working_directory"]).env(CHILD,&f.root).current_dir(&f.root).stdout(std::process::Stdio::null()).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("relative fixture timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    f.check(true);
}

#[cfg(unix)]
#[test]
fn non_utf8_canonical_package_paths_fail_before_logical_commit() {
    use std::os::unix::{ffi::OsStringExt as _, fs::symlink};
    let f = Fixture::new();
    let accepted = f.accepted();
    let target = f.root.join(std::ffi::OsString::from_vec(vec![b'p', 255]));
    std::fs::create_dir(&target).unwrap();
    symlink(&target, f.root.join("plugins")).unwrap();
    assert!(apply(&accepted).is_err());
    f.check(false);
    assert!(
        !Files::new(&f.repository)
            .unwrap()
            .exists("journal.json")
            .unwrap()
    );
}
