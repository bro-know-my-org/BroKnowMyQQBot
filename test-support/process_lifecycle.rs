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
        for _ in 0..3 {
            let mut fixture = Self::spawn(auth);
            for _ in 0..150 {
                if fixture.child.try_wait().unwrap().is_some() {
                    break;
                }
                if fixture.session().await.is_ok() {
                    return fixture;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        panic!("isolated fixture could not bind after three attempts");
    }
    fn spawn(auth: &str) -> Self {
        let root = std::env::temp_dir().join(format!("bkmqb-lifecycle-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let management_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let onebot_reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let management = management_reservation.local_addr().unwrap().port();
        let onebot = onebot_reservation.local_addr().unwrap().port();
        let config = root.join("bot.toml");
        fs::write(&config, configuration(management, onebot, "/")).unwrap();
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
