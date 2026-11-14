//! Real isolated binary: no QQ, user files, external requests or supervisor.
#![cfg(target_os = "linux")]
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

const TOKEN: &str = "isolated-management-token-0123456789abcdef";
const ROTATED: &str = "rotated-management-token-0123456789abcdef";

struct Fixture {
    child: Child,
    root: PathBuf,
    base: String,
    onebot: u16,
    management: u16,
    client: Client,
}
impl Fixture {
    async fn start() -> Self {
        Self::start_with_auth("none").await
    }
    async fn start_with_auth(auth: &str) -> Self {
        Self::start_with_options(auth, false).await
    }
    async fn start_with_options(auth: &str, auto_port: bool) -> Self {
        for _ in 0..3 {
            let mut fixture = Self::spawn(auth, auto_port);
            for _ in 0..150 {
                if fixture.child.try_wait().unwrap().is_some() {
                    break;
                }
                if fixture.discover_management() && fixture.session().await.is_ok() {
                    return fixture;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        panic!("isolated fixture could not bind after three attempts");
    }
    fn spawn(auth: &str, auto_port: bool) -> Self {
        let root = std::env::temp_dir().join(format!("bkmqb-lifecycle-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let management_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let onebot_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let management = management_reservation.local_addr().unwrap().port();
        let onebot = onebot_reservation.local_addr().unwrap().port();
        let config = root.join("bot.toml");
        fs::write(
            &config,
            configuration(management, onebot, "/").replace(
                "auth=\"token\"",
                &format!("auth=\"token\"\nauto_port={auto_port}"),
            ),
        )
        .unwrap();
        let secrets = root.join("secrets.env");
        fs::write(
            &secrets,
            format!("BKMQB_ONEBOT_ACCESS_TOKEN=isolated-fixture-onebot-token\nBKMQB_MANAGEMENT_TOKEN={TOKEN}\n"),
        )
        .unwrap();
        fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();
        drop((management_reservation, onebot_reservation));
        let child = Command::new(env!("CARGO_BIN_EXE_bkmqb"))
            .args(["run", "--management-auth", auth])
            .current_dir(&root)
            .env_clear()
            .env("BKMQB_CONFIG", &config)
            .env("BKMQB_SECRETS_FILE", &secrets)
            .env("BKMQB_BROWSER_HOME", root.join("missing-browser"))
            .env("TOKIO_WORKER_THREADS", "2")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            child,
            root,
            base: format!("http://127.0.0.1:{management}"),
            onebot,
            management,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        }
    }
    fn discover_management(&mut self) -> bool {
        use std::io::Read as _;
        let Ok(months) = fs::read_dir(self.root.join("logs/runtime")) else {
            return false;
        };
        for month in months.take(4).filter_map(Result::ok) {
            let Ok(files) = fs::read_dir(month.path()) else {
                continue;
            };
            for file in files.take(16).filter_map(Result::ok) {
                if file.path().extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let mut text = String::new();
                let Ok(handle) = fs::File::open(file.path()) else {
                    continue;
                };
                if handle.take(64 * 1024).read_to_string(&mut text).is_err() {
                    continue;
                }
                for line in text.lines() {
                    let Ok(row) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if row["message"] != "management server listening" {
                        continue;
                    }
                    let Some(address) = row["listen"]
                        .as_str()
                        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
                    else {
                        continue;
                    };
                    if !address.ip().is_loopback() {
                        continue;
                    }
                    self.management = address.port();
                    self.base = format!("http://{address}");
                    return true;
                }
            }
        }
        false
    }
    async fn session(&self) -> Result<Value, reqwest::Error> {
        self.client
            .get(format!("{}/api/session", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }
    async fn ready(&mut self, old: Option<&str>) -> Value {
        for _ in 0..150 {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "fixture exited before readiness"
            );
            if let Ok(session) = self.session().await {
                if session["boot_id"]
                    .as_str()
                    .is_some_and(|id| Some(id) != old)
                {
                    return session;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("fixture did not become ready");
    }
    async fn cookie_session(&self, cookie: &str) -> Value {
        self.client
            .get(format!("{}/api/session", self.base))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn login(&self, token: &str) -> String {
        let response = self
            .client
            .post(format!("{}/api/session/login", self.base))
            .json(&json!({"token":token}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }
    async fn control(&self, action: &str, boot: &str) -> reqwest::Response {
        self.client
            .post(format!("{}/api/{action}", self.base))
            .json(&json!({"expected_boot_id":boot}))
            .send()
            .await
            .unwrap()
    }
    async fn exited(&mut self) {
        for _ in 0..150 {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "fixture exit: {status}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("fixture did not stop");
    }
    fn log_handles(&self) -> Vec<PathBuf> {
        fs::read_dir(format!("/proc/{}/fd", self.child.id()))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| {
                path.starts_with(self.root.join("logs"))
                    && path.extension().is_some_and(|ext| ext == "jsonl")
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn configuration(management: u16, onebot: u16, prefix: &str) -> String {
    format!(
        r#"[qq]
enabled=false
[onebot11]
enabled=true
listen="127.0.0.1:{onebot}"
[management]
enabled=true
listen="127.0.0.1:{management}"
auth="token"
[commands]
prefix="{prefix}"
[plugins]
database="data/plugins.db"
[logging.console]
enabled=false
[logging.files]
enabled=true
directory="logs"
"#
    )
}

#[tokio::test]
async fn direct_exec_preserves_pid_ports_overrides_and_reloads_config_then_sigterm_exits() {
    let mut fixture = Fixture::start().await;
    let boot = fixture.ready(None).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pid = fixture.child.id();
    let old_logs = fixture.log_handles();
    assert!(!old_logs.is_empty());
    assert_eq!(
        fixture.control("restart", "stale-boot").await.status(),
        StatusCode::CONFLICT
    );
    fs::write(
        fixture.root.join("bot.toml"),
        configuration(fixture.management, fixture.onebot, "!"),
    )
    .unwrap();
    let response = fixture.control("restart", &boot).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.json::<Value>().await.unwrap()["boot_id"], boot);
    let next = fixture.ready(Some(&boot)).await;
    assert_eq!(next["mode"], "none"); // CLI override beats the token config after exec too.
    assert_eq!(fixture.child.id(), pid);
    for port in [fixture.management, fixture.onebot] {
        assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
    }
    assert!(
        fixture
            .log_handles()
            .iter()
            .all(|path| !old_logs.contains(path))
    );
    let commands: Value = fixture
        .client
        .get(format!("{}/api/settings/commands", fixture.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(commands["prefix"], "!");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    fixture.exited().await;
    assert!(fixture.session().await.is_err());
}

#[tokio::test]
async fn stop_accepts_then_exits_normally_without_restarting_and_releases_listeners() {
    let mut fixture = Fixture::start().await;
    let boot = fixture.ready(None).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for action in ["stop", "restart"] {
        let response = fixture
            .client
            .post(format!("{}/api/{action}", fixture.base))
            .header("Origin", "http://evil.example")
            .json(&json!({"expected_boot_id":boot}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = fixture.control("stop", &boot).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(response.json::<Value>().await.unwrap()["action"], "stop");
    fixture.exited().await;
    assert!(fixture.session().await.is_err());
    for port in [fixture.management, fixture.onebot] {
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
    }
}

#[tokio::test]
async fn only_initiator_cookie_resumes_on_exec_and_rotating_file_token_invalidates_it() {
    let mut fixture = Fixture::start_with_auth("token").await;
    let boot = fixture.ready(None).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for action in ["restart", "stop"] {
        assert_eq!(
            fixture.control(action, &boot).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let cookie = fixture.login(TOKEN).await;
    let scope = fixture.cookie_session(&cookie).await["history_scope"].clone();
    let other = fixture.login(TOKEN).await;
    let mut stream = fixture
        .client
        .get(format!("{}/api/logs/stream", fixture.base))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert!(stream.chunk().await.unwrap().is_some());
    let response = fixture
        .client
        .post(format!("{}/api/restart", fixture.base))
        .header("Cookie", &cookie)
        .json(&json!({"expected_boot_id":boot}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _: Value = response.json().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await
    .unwrap();
    let next = fixture.ready(Some(&boot)).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(fixture.cookie_session(&cookie).await["authenticated"], true);
    assert_eq!(
        fixture.cookie_session(&cookie).await["history_scope"],
        scope
    );
    assert_eq!(fixture.cookie_session(&other).await["authenticated"], false);
    let bearer = fixture
        .client
        .get(format!("{}/api/runtime", fixture.base))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), StatusCode::OK);
    fs::write(fixture.root.join("secrets.env"), format!("BKMQB_ONEBOT_ACCESS_TOKEN=isolated-fixture-onebot-token\nBKMQB_MANAGEMENT_TOKEN={ROTATED}\n")).unwrap();
    let response = fixture
        .client
        .post(format!("{}/api/restart", fixture.base))
        .header("Cookie", &cookie)
        .json(&json!({"expected_boot_id":next}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _: Value = response.json().await.unwrap();
    let last = fixture.ready(Some(&next)).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        fixture.cookie_session(&cookie).await["authenticated"],
        false
    );
    for (token, status) in [(TOKEN, StatusCode::UNAUTHORIZED), (ROTATED, StatusCode::OK)] {
        assert_eq!(
            fixture
                .client
                .get(format!("{}/api/runtime", fixture.base))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
    }
    let cookie = fixture.login(ROTATED).await;
    let response = fixture
        .client
        .post(format!("{}/api/stop", fixture.base))
        .header("Cookie", &cookie)
        .json(&json!({"expected_boot_id":last}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _: Value = response.json().await.unwrap();
    fixture.exited().await;
}

#[tokio::test]
async fn automatically_allocated_management_port_is_retained_across_real_exec() {
    let mut fixture = Fixture::start_with_options("token", true).await;
    let boot = fixture.ready(None).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let port = fixture.management;
    let pid = fixture.child.id();
    let cookie = fixture.login(TOKEN).await;
    let response = fixture
        .client
        .post(format!("{}/api/restart", fixture.base))
        .header("Cookie", &cookie)
        .json(&json!({"expected_boot_id":boot}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _: Value = response.json().await.unwrap();
    let next = fixture.ready(Some(&boot)).await["boot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(fixture.management, port);
    assert_eq!(fixture.child.id(), pid);
    assert_eq!(fixture.cookie_session(&cookie).await["authenticated"], true);
    let response = fixture
        .client
        .post(format!("{}/api/stop", fixture.base))
        .header("Cookie", &cookie)
        .json(&json!({"expected_boot_id":next}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let _: Value = response.json().await.unwrap();
    fixture.exited().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One isolated export-to-new-boot acceptance scenario.
async fn logical_backup_restore_preserves_target_history_and_host() {
    use base64::Engine as _;
    let mut f = Fixture::start_with_options("token", true).await;
    let cookie = f.login(TOKEN).await;
    let boot = f.ready(None).await["boot_id"].as_str().unwrap().to_owned();
    let pid = f.child.id();
    let address = f.base.clone();
    let database = f.root.join("data/plugins.db");
    let store = plugin_host::PluginStore::open(&database).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(include_str!("wasm-plugins/ping/component.bkm-plugin.b64").trim())
        .unwrap();
    let package = plugin_host::ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let manifest = package.manifest();
    let path = f.root.join("original.bkm-plugin");
    fs::write(&path, &bytes).unwrap();
    let item=serde_json::from_value(json!({"plugin_id":manifest.id,"metadata":manifest.metadata,"instance_id":"dev.bkm.wasm-ping/offline","version":manifest.version,"package_path":path,"package_sha256":package.package_sha256(),"source":"local","trust_level":"local-wasm","signature_status":"unsigned","requested_permissions":manifest.requested_capabilities(),"granted_permissions":manifest.requested_capabilities(),"config":{},"enabled":false,"installed_at_ms":1,"updated_at_ms":1})).unwrap();
    store.upsert_installation(&item).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO plugin_state VALUES ('dev.bkm.counter/default','backup-fixture',?,1)",
            [b"saved-business-data".as_slice()],
        )
        .unwrap();
    connection.execute_batch("INSERT INTO command_ledger VALUES ('dev.bkm.counter/default','source','source-only','source-key','fixture',X'00','succeeded',NULL); INSERT INTO scheduled_tasks VALUES ('dev.bkm.counter/default','source-only','mock',9223372036854775807,X'00','completed'); INSERT INTO plugin_deliveries(instance_id,event_id,delivery_id,invocation_id,attempt,envelope,origin,status,next_attempt_ms,updated_at_ms) VALUES ('dev.bkm.counter/default','source-only','source-delivery','source',0,X'00',X'00','delivered',0,0);").unwrap();
    let artifact: Value = f
        .client
        .post(format!("{}/api/backups", f.base))
        .header("Cookie", &cookie)
        .json(&json!({"confirm_sensitive_plugin_data":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = f
        .client
        .get(format!(
            "{}/api/backups/{}",
            f.base,
            artifact["id"].as_str().unwrap()
        ))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let archive = response.bytes().await.unwrap();
    store
        .remove_installation("dev.bkm.wasm-ping/offline")
        .unwrap();
    fs::remove_file(&path).unwrap();
    connection
        .execute(
            "UPDATE plugin_state SET value=?,revision=2 WHERE key='backup-fixture'",
            [b"changed-business-data".as_slice()],
        )
        .unwrap();
    connection.execute_batch("DELETE FROM command_ledger WHERE command_id='source-only'; DELETE FROM scheduled_tasks; DELETE FROM plugin_deliveries; INSERT INTO command_ledger VALUES ('dev.bkm.counter/default','target','target-only','target-key','fixture',X'00','succeeded',NULL); INSERT INTO scheduled_tasks VALUES ('dev.bkm.counter/default','target-pending','mock',9223372036854775807,X'00','pending');").unwrap();
    drop((connection, store));
    let current = configuration(0, f.onebot, "!!")
        .replace("auth=\"token\"", "auth=\"token\"\nauto_port=true")
        .replace("directory=\"logs\"", "directory=\"current-logs\"");
    fs::write(
        f.root.join("bot.toml"),
        format!("# retain current host\n{current}"),
    )
    .unwrap();
    let plan: Value = f
        .client
        .post(format!("{}/api/restores/inspect", f.base))
        .timeout(Duration::from_secs(30))
        .header("Cookie", &cookie)
        .header("content-type", "application/octet-stream")
        .body(archive)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = plan["operation_id"].as_str().unwrap();
    assert_eq!(plan["settings_before"]["commands"]["prefix"], "!!");
    assert_eq!(plan["settings_after"]["commands"]["prefix"], "/");
    let imported = plan["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["instance_id"] == "dev.bkm.wasm-ping/offline")
        .unwrap();
    assert!(imported["retained_grants"].as_array().unwrap().is_empty());
    let response=f.client.post(format!("{}/api/restores/{id}/commit",f.base)).header("Cookie",&cookie).json(&json!({"expected_boot_id":boot,"confirm_sensitive_plugin_data":true,"confirm_unsigned_disabled":true})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let next = f.ready(Some(&boot)).await;
    assert_eq!(f.child.id(), pid);
    assert_eq!(f.base, address);
    assert_eq!(next["mode"], "token");
    assert_eq!(f.cookie_session(&cookie).await["authenticated"], true);
    let outcome: Value = f
        .client
        .get(format!("{}/api/restores/{id}/outcome", f.base))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(outcome["status"], "applied");
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("bot.toml")).unwrap()).unwrap();
    assert_eq!(config["commands"]["prefix"].as_str(), Some("/"));
    assert_eq!(config["management"]["auth"].as_str(), Some("token"));
    assert_eq!(config["management"]["listen"].as_str(), Some("127.0.0.1:0"));
    assert_eq!(config["management"]["auto_port"].as_bool(), Some(true));
    assert_eq!(
        config["plugins"]["database"].as_str(),
        Some("data/plugins.db")
    );
    assert_eq!(
        config["logging"]["files"]["directory"].as_str(),
        Some("current-logs")
    );
    let store = plugin_host::PluginStore::open(&database).unwrap();
    assert_eq!(
        store
            .state_get("dev.bkm.counter/default", "backup-fixture")
            .unwrap()
            .unwrap()
            .value,
        b"saved-business-data"
    );
    let imported = store
        .installation("dev.bkm.wasm-ping/offline")
        .unwrap()
        .unwrap();
    assert!(!imported.enabled);
    assert!(imported.granted_permissions.is_empty());
    assert_eq!(imported.signature_status, "unsigned");
    assert!(std::path::Path::new(&imported.package_path).is_file());
    assert_ne!(std::path::Path::new(&imported.package_path), path);
    assert_eq!(fs::read(&imported.package_path).unwrap(), bytes);
    let connection = rusqlite::Connection::open(&database).unwrap();
    for table in ["command_ledger", "scheduled_tasks", "plugin_deliveries"] {
        let count: i64 = connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE {}='source-only'",
                    if table == "command_ledger" {
                        "command_id"
                    } else if table == "scheduled_tasks" {
                        "task_id"
                    } else {
                        "event_id"
                    }
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "source execution history must stay excluded: {table}"
        );
    }
    let status: String = connection
        .query_row(
            "SELECT status FROM command_ledger WHERE command_id='target-only'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "succeeded");
    let status: String = connection
        .query_row(
            "SELECT status FROM scheduled_tasks WHERE task_id='target-pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "cancelled");
    let sends: i64 = connection
        .query_row("SELECT COUNT(*) FROM command_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sends, 0);
    drop((connection, store));
    let response = f
        .client
        .post(format!("{}/api/stop", f.base))
        .header("Cookie", &cookie)
        .json(&json!({"expected_boot_id":next["boot_id"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    f.exited().await;
}
