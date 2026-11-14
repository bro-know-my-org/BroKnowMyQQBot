use super::*;
use std::fs;

fn manager() -> crate::plugin_management::PluginManager {
    let store = plugin_host::PluginStore::in_memory().unwrap();
    crate::plugin_management::PluginManager::new(
        plugin_host::StaticPluginHost::new(store.clone()),
        store,
        std::collections::BTreeMap::new(),
        std::collections::BTreeSet::new(),
    )
}

#[test]
fn environment_overrides_stay_out_of_saved_config() {
    if std::env::var_os("BKMQB_SYSTEM_SETTINGS_TEST_CHILD").is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "management::system_settings::tests::environment_overrides_stay_out_of_saved_config"])
            .env("BKMQB_SYSTEM_SETTINGS_TEST_CHILD", "1")
            .env("BKMQB_MANAGEMENT_LISTEN", "127.0.0.1:9090")
            .env("BKMQB_QQ_ENVIRONMENT", "sandbox").output().unwrap();
        assert!(
            result.status.success(),
            "environment regression child failed"
        );
        return;
    }
    let f = Fixture::new();
    let text = fs::read_to_string(&f.path).unwrap()
        + "\n[management]\nenabled = true\nlisten = '${MANAGED_LISTEN}' # keep env reference\n";
    fs::write(&f.path, text).unwrap();
    let mut request = f.request("global");
    request["settings"]["runtime"]["event_concurrency"] = json!(7);
    f.save(request).unwrap();
    let saved = fs::read_to_string(&f.path).unwrap();
    assert!(saved.contains("listen = '${MANAGED_LISTEN}' # keep env reference"));
    let config: BotConfig = toml::from_str(&saved).unwrap();
    assert_eq!(config.qq.environment, "production");
    assert_eq!(
        BotConfig::check_source(&saved).unwrap().qq.environment,
        "sandbox"
    );
}

#[test]
fn concurrent_command_and_system_saves_have_one_winner_and_fresh_saves_preserve_both() {
    let f = Fixture::new();
    let manager = manager();
    let mut request = f.request("global");
    request["settings"]["runtime"]["event_concurrency"] = json!(7);
    let revision = f.view()["revision"].as_str().unwrap().to_owned();
    let barrier = std::sync::Barrier::new(2);
    let (system, command) = std::thread::scope(|scope| {
        let system = scope.spawn(|| {
            barrier.wait();
            f.save(request)
        });
        let command = scope.spawn(|| {
            barrier.wait();
            super::super::settings::update(&f.path, &manager, "!!", &revision)
        });
        (system.join().unwrap(), command.join().unwrap())
    });
    assert!(matches!(
        (&system, &command),
        (Ok(_), Err(ManagementError::Conflict)) | (Err(ManagementError::Conflict), Ok(_))
    ));
    if system.is_err() {
        let mut request = f.request("global");
        request["settings"]["runtime"]["event_concurrency"] = json!(7);
        f.save(request).unwrap();
    } else {
        super::super::settings::update(
            &f.path,
            &manager,
            "!!",
            f.view()["revision"].as_str().unwrap(),
        )
        .unwrap();
    }
    let text = fs::read_to_string(&f.path).unwrap();
    let saved: BotConfig = toml::from_str(&text).unwrap();
    assert_eq!(saved.commands.prefix, "!!");
    assert_eq!(saved.runtime.event_concurrency, 7);
    assert!(text.contains("# keep environment"));
    assert!(text.contains("# keep prefix"));
}

#[test]
fn concurrent_setup_and_system_saves_cannot_overwrite_each_other() {
    let f = Fixture::new();
    let mut setup = BotConfig::load_from_path(&f.path, true).unwrap();
    setup.commands.prefix = "!!".into();
    let mut request = f.request("global");
    request["settings"]["runtime"]["event_concurrency"] = json!(7);
    let barrier = std::sync::Barrier::new(2);
    let (system, setup) = std::thread::scope(|scope| {
        let system = scope.spawn(|| {
            barrier.wait();
            f.save(request)
        });
        let setup = scope.spawn(|| {
            barrier.wait();
            setup.save()
        });
        (system.join().unwrap(), setup.join().unwrap())
    });
    let saved: BotConfig = toml::from_str(&fs::read_to_string(&f.path).unwrap()).unwrap();
    match (system, setup) {
        (Ok(_), Err(config::ConfigError::Conflict(_))) => {
            assert_eq!(saved.runtime.event_concurrency, 7);
            assert_eq!(saved.commands.prefix, "/");
        }
        (Err(ManagementError::Conflict), Ok(_)) => {
            assert_eq!(saved.runtime.event_concurrency, 32);
            assert_eq!(saved.commands.prefix, "!!");
        }
        _ => panic!("exactly one configuration writer must win"),
    }
}

#[tokio::test]
async fn route_authentication_and_safe_schema_errors() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request as HttpRequest, StatusCode},
    };
    use tower::ServiceExt as _;
    let token = "fixture-system-token-at-least-32-characters";
    let router = super::super::plugins::router(
        manager(),
        super::super::plugins::token_digest(token).unwrap(),
    );
    let unauthorized = router
        .clone()
        .oneshot(
            HttpRequest::get("/api/settings/system")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let invalid = router
        .oneshot(
            HttpRequest::post("/api/settings/system")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"scope":"secret-userinfo-https://user:password@host/?token=secret"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(invalid.into_body(), 4096).await.unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(!text.contains("password") && !text.contains("userinfo"));
}

struct Fixture {
    directory: std::path::PathBuf,
    path: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("bkm-system-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bot.toml");
        fs::write(&path, "# keep header\n[commands]\nprefix = '/' # keep prefix\n[qq]\nenvironment = 'production' # keep environment\n[plugins]\ndatabase = 'fixture.db'\n# BKMQB_QQ_OFFICIAL_APP_SECRET remains local\n").unwrap();
        Self { directory, path }
    }
    fn view(&self) -> Value {
        read_view(&self.path).unwrap()
    }
    fn request(&self, scope: &str) -> Value {
        let view = self.view();
        json!({"scope":scope,"expected_revision":view["revision"],"settings":view[scope]})
    }
    fn save(&self, request: Value) -> Result<Value, ManagementError> {
        update(&self.path, serde_json::from_value(request).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn round_trip_preserves_comments_unowned_fields_defaults_and_secret_references() {
    let f = Fixture::new();
    let original = f.view();
    assert_eq!(original["connections"]["qq"]["transport"], "websocket");
    assert_eq!(
        original["global"]["logging"]["console"]["message_content"],
        false
    );
    assert_eq!(original["credential_presence"], "unknown");
    assert!(original.get("management").is_none());
    assert!(
        original["global"]["logging"]["files"]
            .get("directory")
            .is_none()
    );
    let mut request = f.request("connections");
    request["settings"]["qq"]["environment"] = json!("sandbox");
    let result = f.save(request).unwrap();
    assert_eq!(result["apply"], "restart_required");
    assert_ne!(result["revision"], original["revision"]);
    let text = fs::read_to_string(&f.path).unwrap();
    for retained in [
        "# keep header",
        "# keep prefix",
        "# keep environment",
        "database = 'fixture.db'",
        "# BKMQB_QQ_OFFICIAL_APP_SECRET remains local",
    ] {
        assert!(text.contains(retained), "{retained}");
    }
    let mut request = f.request("global");
    request["settings"]["runtime"]["event_concurrency"] = json!(7);
    f.save(request).unwrap();
    let saved: BotConfig = toml::from_str(&fs::read_to_string(&f.path).unwrap()).unwrap();
    assert_eq!(saved.qq.environment, "sandbox");
    assert_eq!(saved.commands.prefix, "/");
    assert_eq!(saved.runtime.event_concurrency, 7);
    assert!(!saved.logging.console.message_content && !saved.logging.files.message_content);
}

#[test]
fn shares_revision_and_lock_with_setup_and_command_writer() {
    let f = Fixture::new();
    let stale = f.request("global");
    let mut setup = BotConfig::load_from_path(&f.path, true).unwrap();
    let lock = config::ConfigFileLock::acquire(&f.path).unwrap();
    assert!(matches!(
        f.save(stale.clone()),
        Err(ManagementError::Conflict)
    ));
    drop(lock);
    let mut change = f.request("connections");
    change["settings"]["qq"]["environment"] = json!("sandbox");
    f.save(change).unwrap();
    assert!(matches!(
        setup.save(),
        Err(config::ConfigError::Conflict(_))
    ));
    assert!(matches!(f.save(stale), Err(ManagementError::Conflict)));
    let mut setup = BotConfig::load_from_path(&f.path, true).unwrap();
    setup.commands.prefix = "!!".into();
    setup.save().unwrap();
    let current = f.request("global");
    f.save(current).unwrap();
    let saved: BotConfig = toml::from_str(&fs::read_to_string(&f.path).unwrap()).unwrap();
    assert_eq!(saved.commands.prefix, "!!");
    assert_eq!(saved.qq.environment, "sandbox");
}

#[test]
fn invalid_combinations_limits_filters_and_urls_never_write_or_echo_input() {
    let f = Fixture::new();
    let original = fs::read(&f.path).unwrap();
    for (scope, pointer, value) in [
        ("connections", "/qq/enabled", json!(false)),
        (
            "connections",
            "/qq/transport",
            json!("https://user:secret@host/?token=secret"),
        ),
        (
            "connections",
            "/qq/webhook/path",
            json!("/callback?token=secret"),
        ),
        ("connections", "/onebot11/listen", json!("secret.invalid:1")),
        ("global", "/runtime/queue_capacity", json!(999_999)),
        ("global", "/runtime/handler_timeout_seconds", json!(0)),
        ("global", "/logging/files/filter", json!("[secret")),
        (
            "global",
            "/logging/files/runtime_max_file_mb",
            json!(999_999_999),
        ),
    ] {
        let mut request = f.request(scope);
        *request["settings"].pointer_mut(pointer).unwrap() = value;
        let error = f.save(request).unwrap_err();
        assert!(matches!(error, ManagementError::InvalidSystemSettings));
        assert!(!error.to_string().contains("secret"));
        assert_eq!(fs::read(&f.path).unwrap(), original);
    }
    let mut request = f.request("connections");
    request["settings"]["onebot11"]["enabled"] = json!(true);
    request["settings"]["onebot11"]["listen"] = json!("0.0.0.0:6700");
    assert!(matches!(
        f.save(request),
        Err(ManagementError::InvalidSystemSettings)
    ));
}

#[test]
fn unknown_keys_credentials_management_and_paths_are_rejected() {
    let f = Fixture::new();
    for (scope, pointer, key) in [
        ("connections", "/qq", "app_secret"),
        ("connections", "", "management"),
        ("global", "/logging/files", "directory"),
        ("global", "/runtime", "unknown"),
    ] {
        let mut request = f.request(scope);
        request["settings"].pointer_mut(pointer).unwrap()[key] = json!("secret");
        assert!(serde_json::from_value::<Request>(request).is_err());
    }
}

#[test]
fn missing_symlink_and_oversized_configs_are_not_recreated_or_disclosed() {
    let f = Fixture::new();
    let request = f.request("global");
    fs::remove_file(&f.path).unwrap();
    assert!(matches!(
        f.save(request.clone()),
        Err(ManagementError::SettingsUnavailable)
    ));
    assert!(!f.path.exists());
    #[cfg(unix)]
    {
        let other = f.directory.join("other.toml");
        fs::write(&other, "# secret").unwrap();
        std::os::unix::fs::symlink(&other, &f.path).unwrap();
        assert!(matches!(
            f.save(request.clone()),
            Err(ManagementError::SettingsUnavailable)
        ));
        assert_eq!(fs::read_to_string(other).unwrap(), "# secret");
        fs::remove_file(&f.path).unwrap();
    }
    fs::write(&f.path, "#".repeat(1024 * 1024 + 1)).unwrap();
    assert!(matches!(
        f.save(request),
        Err(ManagementError::SettingsUnavailable)
    ));
}
