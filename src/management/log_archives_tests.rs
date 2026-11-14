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

fn record() -> String {
    json!({"timestamp":"2026-11-14T00:00:00Z","level":"INFO","target":"plugin_host::runtime",
        "message":"registered plugin","spans":[{"instance_id":"sample","plugin_id":"dev.bkm.sample","token":"span-secret"}],
        "error":"private-error","payload":{"body":"message-secret"},"content":"message-secret"}).to_string()+"\n"
}
#[tokio::test]
async fn paged_runtime_records_are_safe_and_cursors_cannot_cross_files() {
    let (temp, logs, _) = fixture();
    let month = temp.path().join("runtime/2026-11");
    std::fs::write(month.join(NAME), record().repeat(3)).unwrap();
    std::fs::write(month.join(NAME.replace("s000", "s001")), record()).unwrap();
    let app = app(logs);
    let (_, inventory) = call(&app, "/api/logs/archives", true, false).await;
    let selector = inventory["files"][1]["selector"].as_str().unwrap();
    let path = format!("/api/logs/archives/page?selector={selector}&limit=2");
    assert_eq!(
        call(&app, &path, false, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(call(&app, &path, true, true).await.0, StatusCode::FORBIDDEN);
    let (status, first) = call(&app, &path, true, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    assert_eq!(first["entries"][0]["instance_id"], "sample");
    for forbidden in [
        "private-error",
        "span-secret",
        "message-secret",
        "payload",
        "spans",
    ] {
        assert!(!first.to_string().contains(forbidden));
    }
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, second) = call(&app, &format!("{path}&cursor={cursor}"), true, false).await;
    assert_eq!(second["entries"][0]["id"], 3);
    assert!(second["next_cursor"].is_null());
    let other = inventory["files"][0]["selector"].as_str().unwrap();
    assert_eq!(
        call(
            &app,
            &format!("/api/logs/archives/page?selector={other}&cursor={cursor}"),
            true,
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    std::fs::write(month.join(NAME), record()).unwrap();
    assert_eq!(call(&app, &path, true, false).await.0, StatusCode::CONFLICT);
}
#[tokio::test]
async fn page_rejects_paths_bad_bounds_unknown_options_and_malformed_compression() {
    let (temp, logs, _) = fixture();
    let file = temp
        .path()
        .join("runtime/2026-11")
        .join(format!("{NAME}.zst"));
    std::fs::write(&file, "not zstd").unwrap();
    let app = app(logs);
    let (_, inventory) = call(&app, "/api/logs/archives", true, false).await;
    let selector = inventory["files"][0]["selector"].as_str().unwrap();
    for query in [
        "selector=../../messages",
        "selector=%2Fetc%2Fpasswd",
        "path=/etc/passwd",
        "limit=0",
        "limit=201",
        "limit=-1",
        "cursor=invalid",
        "include_content=true",
    ] {
        let uri = format!("/api/logs/archives/page?selector={selector}&{query}");
        assert_eq!(
            call(&app, &uri, true, false).await.0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    let uri = format!("/api/logs/archives/page?selector={selector}");
    assert_eq!(
        call(&app, &uri, true, false).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &file).unwrap();
    assert_eq!(call(&app, &uri, true, false).await.0, StatusCode::CONFLICT);
}
#[tokio::test]
async fn compressed_pages_skip_unknown_oversized_and_malformed_lines_truthfully() {
    let (temp, logs, _) = fixture();
    let file = temp
        .path()
        .join("runtime/2026-11")
        .join(format!("{NAME}.zst"));
    let data = format!(
        "invalid\n{}\n{}\n{}",
        "x".repeat(17000),
        json!({"message":"unsafe old secret"}),
        record()
    );
    let compressed = zstd::stream::encode_all(data.as_bytes(), 3).unwrap();
    std::fs::write(&file, compressed).unwrap();
    let app = app(logs);
    let (_, inventory) = call(&app, "/api/logs/archives", true, false).await;
    let selector = inventory["files"][0]["selector"].as_str().unwrap();
    let (status, page) = call(
        &app,
        &format!("/api/logs/archives/page?selector={selector}"),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["skipped"], 3);
    assert_eq!(page["scanned"], 4);
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert!(page["next_cursor"].is_null());
}
#[tokio::test]
async fn scan_limits_bound_bombs_and_allow_empty_pages_to_advance() {
    use std::io::{Read as _, Write as _};
    let (temp, logs, _) = fixture();
    let month = temp.path().join("runtime/2026-11");
    std::fs::write(
        month.join(NAME),
        format!("{}{}", "bad\n".repeat(5000), record()),
    )
    .unwrap();
    let app = app(logs.clone());
    let (_, inventory) = call(&app, "/api/logs/archives", true, false).await;
    let selector = inventory["files"][0]["selector"].as_str().unwrap();
    let uri = format!("/api/logs/archives/page?selector={selector}");
    let (_, page) = call(&app, &uri, true, false).await;
    assert_eq!(page["scanned"], 5000);
    assert_eq!(page["skipped"], 5000);
    assert_eq!(page["entries"], json!([]));
    let (_, page) = call(
        &app,
        &format!("{uri}&cursor={}", page["next_cursor"].as_str().unwrap()),
        true,
        false,
    )
    .await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    std::fs::remove_file(month.join(NAME)).unwrap();
    let file = std::fs::File::create(month.join(format!("{NAME}.zst"))).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    std::io::copy(
        &mut std::io::repeat(b'x').take(64 * 1024 * 1024 + 1024),
        &mut encoder,
    )
    .unwrap();
    encoder.write_all(b"\n").unwrap();
    encoder.finish().unwrap();
    let (_, inventory) = call(&app, "/api/logs/archives", true, false).await;
    let selector = inventory["files"][0]["selector"].as_str().unwrap();
    let (status, page) = call(
        &app,
        &format!("/api/logs/archives/page?selector={selector}"),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["limited"], true);
    assert!(page["next_cursor"].is_null());
    assert_eq!(page["entries"], json!([]));
}

#[tokio::test]
async fn total_line_budget_ends_with_explicit_unavailable_tail() {
    for length in [100_000, 100_001] {
        let (temp, logs, _) = fixture();
        std::fs::write(
            temp.path().join("runtime/2026-11").join(NAME),
            "bad\n".repeat(length),
        )
        .unwrap();
        let app = app(logs);
        let (_, inventory) = call(&app, "/api/logs/archives", true, false).await;
        let selector = inventory["files"][0]["selector"].as_str().unwrap();
        let uri = format!("/api/logs/archives/page?selector={selector}");
        let mut cursor = None;
        for index in 0..20 {
            let (_, page) = call(
                &app,
                &cursor
                    .as_ref()
                    .map_or(uri.clone(), |c| format!("{uri}&cursor={c}")),
                true,
                false,
            )
            .await;
            assert_eq!(page["scanned"], 5000);
            assert_eq!(page["skipped"], 5000);
            if index < 19 {
                cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
            } else {
                assert_eq!(page["limited"], length > 100_000);
                assert!(page["next_cursor"].is_null());
            }
        }
    }
}
