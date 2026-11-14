use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use tower::ServiceExt as _;
struct TestDirectory(std::path::PathBuf);
impl TestDirectory {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const NAME: &str = "20261114T000000.000+0800-p1-test-s000.jsonl";
fn fixture() -> (TestDirectory, LogBuffer, Archives) {
    let temp = TestDirectory(
        std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("bkm-archives-test-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(temp.path().join("runtime/2026-11")).unwrap();
    let logs = LogBuffer::default();
    logs.configure_archives(Some(temp.path().to_path_buf()));
    let archives = Archives {
        logs: logs.clone(),
        epoch: "test-epoch".into(),
        slots: Arc::new(Semaphore::new(2)),
    };
    (temp, logs, archives)
}
async fn call(app: &Router, path: &str, token: bool, foreign: bool) -> (StatusCode, Value) {
    let mut request = Request::get(path).header("host", "127.0.0.1:9090");
    if token {
        request = request.header(
            "authorization",
            "Bearer archive-test-token-01234567890123456789",
        );
    }
    if foreign {
        request = request.header("origin", "https://foreign.test");
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn app(logs: LogBuffer) -> Router {
    router(
        logs,
        super::super::plugins::token_digest("archive-test-token-01234567890123456789")
            .unwrap()
            .into(),
    )
}
#[tokio::test]
async fn inventory_uses_existing_auth_and_source_checks_and_disabled_state() {
    let app = app(LogBuffer::default());
    assert_eq!(
        call(&app, "/api/logs/archives", false, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "/api/logs/archives", true, true).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = call(&app, "/api/logs/archives", true, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["files"], json!([]));
}
#[test]
fn inventory_rejects_symlinks_nonregular_files_and_unmanaged_names() {
    use std::os::unix::fs::symlink;
    let (temp, _, archives) = fixture();
    let month = temp.path().join("runtime/2026-11");
    std::fs::write(month.join(NAME), "{}\n").unwrap();
    std::fs::write(month.join("../../messages.jsonl"), "private").unwrap();
    std::fs::write(month.join("unmanaged.jsonl"), "private").unwrap();
    symlink(month.join(NAME), month.join(NAME.replace("s000", "s001"))).unwrap();
    std::fs::create_dir(month.join(NAME.replace("s000", "s002"))).unwrap();
    symlink(&month, temp.path().join("runtime/2026-10")).unwrap();
    let inventory = archives.inventory().unwrap();
    assert_eq!(inventory.files.len(), 1);
    assert_eq!(inventory.files[0].name, NAME);
    assert_eq!(inventory.files[0].selector.len(), 64);
    let original = &inventory.files[0].selector;
    std::fs::write(month.join(NAME), "replacement\n").unwrap();
    assert_ne!(*original, archives.inventory().unwrap().files[0].selector);
    let outside = temp.path().join("linked");
    symlink(temp.path().join("runtime"), &outside).unwrap();
    assert!(safe_directory(&outside).is_err());
}
#[test]
fn inventory_is_bounded_and_reports_files_too_large_without_reading_them() {
    let (temp, _, archives) = fixture();
    let month = temp.path().join("runtime/2026-11");
    for i in 0..=MAX_FILES {
        std::fs::write(month.join(NAME.replace("s000", &format!("s{i:03}"))), "{}").unwrap();
    }
    let inventory = archives.inventory().unwrap();
    assert_eq!(inventory.files.len(), MAX_FILES);
    assert!(inventory.truncated);
    for path in std::fs::read_dir(&month).unwrap() {
        std::fs::remove_file(path.unwrap().path()).unwrap();
    }
    let file = std::fs::File::create(month.join(format!("{NAME}.zst"))).unwrap();
    file.set_len(MAX_COMPRESSED_BYTES + 1).unwrap();
    let inventory = archives.inventory().unwrap();
    assert!(!inventory.files[0].available);
    assert!(inventory.files[0].compressed);
}
#[test]
fn global_enumeration_budget_counts_unmanaged_entries_too() {
    let (temp, _, archives) = fixture();
    for i in 0..MAX_ENUMERATED {
        std::fs::write(
            temp.path().join("runtime").join(format!("unmanaged-{i}")),
            "",
        )
        .unwrap();
    }
    assert!(archives.inventory().unwrap().truncated);
}

#[test]
fn configured_parent_components_are_opened_without_following_symlinks() {
    let (temp, _, archives) = fixture();
    assert!(safe_directory(&temp.path().join("runtime/../runtime")).is_ok());
    assert!(
        archives
            .inventory_at(&temp.path().join("runtime/../runtime"))
            .is_ok()
    );
    std::os::unix::fs::symlink(temp.path().join("runtime"), temp.path().join("link")).unwrap();
    assert!(safe_directory(&temp.path().join("link/../runtime")).is_err());
}

#[test]
fn native_non_utf8_configured_directories_remain_usable() {
    use std::os::unix::ffi::OsStringExt as _;
    let (temp, _, archives) = fixture();
    let path = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"logs-\xff".to_vec()));
    std::fs::create_dir_all(path.join("runtime")).unwrap();
    assert!(archives.inventory_at(&path.join("runtime")).is_ok());
}
