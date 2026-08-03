//! Optional Playwright and Chromium runtime management.

use std::{
    env, fs,
    io::{self, Cursor, Read as _, Write as _},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flate2::read::GzDecoder;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::{process::Command, time::timeout};

const NODE_VERSION: &str = "22.23.2";
const PLAYWRIGHT_VERSION: &str = "1.62.1";
const PLAYWRIGHT_SHA256: &str = "954be1e183d0ddb9748fe0d2d08b0b66a9210c74dd75c397aeb70303b9f08a00";
const PLAYWRIGHT_TREE_SHA256: &str =
    "56d9c79a81caabc7754771f96672d649425f27690565c25e832d95a15a830948";
const WORKER_SHA256: &str = "42ab76b5b0a1e90dde3bf76f269b23fd117ea3439257d49c2c1d60e5e87a030e";
const CHROMIUM_VERSION: &str = "151.0.7922.34";
const CHROMIUM_REVISION: &str = "1234";
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_EXTRACTED_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 50_000;
const MAX_DIRECTORY_DEPTH: usize = 64;
const STALE_STAGING_AGE_MS: u128 = 2 * 60 * 60 * 1_000;
const BROWSER_DOWNLOAD_PARTS: u64 = 16;
const BROWSER_DOWNLOAD_ATTEMPTS: usize = 3;

type InstallResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn installer_result<T>(result: InstallResult<T>) -> Result<T, io::Error> {
    result.map_err(io::Error::other)
}

const WORKER_SOURCE: &str = r"'use strict';
const { chromium } = require('./playwright');

async function main() {
  const browser = await chromium.launch({ headless: true });
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    await page.setContent('<!doctype html><title>bkmqb browser check</title><p>ok</p>');
    const title = await page.title();
    process.stdout.write(JSON.stringify({ ok: title === 'bkmqb browser check', browser: browser.version() }));
  } finally {
    await browser.close();
  }
}

main().catch(error => {
  process.stderr.write(String(error && error.stack || error));
  process.exitCode = 1;
});
";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BrowserRuntimeManifest {
    schema_version: u32,
    node_version: String,
    playwright_version: String,
    chromium_version: String,
    chromium_generation: String,
    platform: String,
}

#[derive(Debug, Clone, Copy)]
struct NodeAsset {
    platform: &'static str,
    archive: &'static str,
    sha256: &'static str,
    tree_sha256: &'static str,
    format: ArchiveFormat,
}

#[derive(Debug, Clone, Copy)]
enum ArchiveFormat {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, Copy)]
struct BrowserAsset {
    archive_path: &'static str,
    generation: &'static str,
    etag: &'static str,
    size: u64,
    executable_path: &'static str,
    tree_sha256: &'static str,
}

struct StagingDirectory {
    path: Option<PathBuf>,
}

impl StagingDirectory {
    fn create(runtimes: &Path) -> io::Result<Self> {
        let path = runtimes.join(format!(".installing-{}-{}", now_ms(), uuid::Uuid::new_v4()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self { path: Some(path) })
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("staging path is armed")
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if fs::symlink_metadata(&path).is_err() {
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        let cleanup = parent.join(format!(".cleanup-{}-{}", now_ms(), uuid::Uuid::new_v4()));
        if fs::rename(&path, &cleanup).is_err() {
            return;
        }
        let _ = std::thread::Builder::new()
            .name("bkmqb-browser-cleanup".to_owned())
            .spawn(move || {
                let _ = fs::remove_dir_all(cleanup);
            });
    }
}

impl BrowserAsset {
    fn metadata_url(self) -> String {
        format!(
            "https://storage.googleapis.com/chrome-for-testing-public/{CHROMIUM_VERSION}/{}",
            self.archive_path
        )
    }
}

pub(crate) async fn run(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().map(String::as_str) {
        Some("status") if arguments.len() == 1 => status().await,
        Some("install" | "update") if arguments.len() == 1 => install().await,
        Some("check") if arguments.len() == 1 => check().await,
        Some("remove") if arguments.len() == 1 => remove().await,
        Some("help" | "--help" | "-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => {
            Err(format!("unknown browser command `{other}`; run `bkmqb browser help`").into())
        }
    }
}

fn print_help() {
    println!(
        "Optional browser runtime\n\n\
Usage:\n\
  bkmqb browser status\n\
  bkmqb browser install\n\
  bkmqb browser check\n\
  bkmqb browser update\n\
  bkmqb browser remove\n\n\
The browser runtime is downloaded only after an explicit install command."
    );
}

async fn status() -> Result<(), Box<dyn std::error::Error>> {
    let home = browser_home()?;
    ensure_runtime_home(&home)?;
    let _lock = acquire_lock(&home)?;
    let Some(runtime) = active_runtime(&home)? else {
        println!("Browser Runtime: not installed");
        println!("Install with: bkmqb browser install");
        return Ok(());
    };
    let validation_runtime = runtime.clone();
    tokio::task::spawn_blocking(move || validate_runtime(&validation_runtime))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
    let manifest = installer_result(read_manifest(&runtime))?;
    println!("Browser Runtime: installed");
    println!("Playwright: {}", manifest.playwright_version);
    println!("Node: {}", manifest.node_version);
    println!("Platform: {}", manifest.platform);
    println!("Location: {}", runtime.display());
    Ok(())
}

async fn install() -> Result<(), Box<dyn std::error::Error>> {
    let node_asset = installer_result(node_asset())?;
    let browser_asset = installer_result(browser_asset())?;
    let home = browser_home()?;
    ensure_runtime_home(&home)?;
    let _lock = acquire_lock(&home)?;
    let runtimes = home.join("runtimes");
    let cleanup_runtimes = runtimes.clone();
    tokio::task::spawn_blocking(move || cleanup_stale_staging(&cleanup_runtimes))
        .await
        .map_err(io::Error::other)??;
    let runtime_name = format!(
        "pw-{PLAYWRIGHT_VERSION}-node-{NODE_VERSION}-{}",
        node_asset.platform
    );
    let final_runtime = runtimes.join(&runtime_name);
    if fs::symlink_metadata(&final_runtime).is_ok_and(|metadata| metadata_is_link_like(&metadata)) {
        return Err("refusing to use a symlinked Browser Runtime entry".into());
    }
    if final_runtime.is_dir() {
        let validation_runtime = final_runtime.clone();
        tokio::task::spawn_blocking(move || validate_runtime(&validation_runtime))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        activate(&home, &runtime_name)?;
        println!(
            "Browser Runtime is already installed: {}",
            final_runtime.display()
        );
        return check_runtime(&final_runtime).await;
    }

    let mut staging_guard = StagingDirectory::create(&runtimes)?;
    let staging = staging_guard.path().to_owned();
    let result = install_into(&staging, node_asset, browser_asset).await;
    if let Err(error) = result {
        if let Err(cleanup_error) = remove_directory_blocking(staging.clone()).await {
            return Err(format!(
                "{error}; additionally failed to remove incomplete Browser Runtime: {cleanup_error}"
            )
            .into());
        }
        return Err(error);
    }
    if let Err(error) = check_runtime(&staging).await {
        if let Err(cleanup_error) = remove_directory_blocking(staging.clone()).await {
            return Err(format!(
                "{error}; additionally failed to remove unhealthy Browser Runtime: {cleanup_error}"
            )
            .into());
        }
        return Err(error);
    }
    if let Err(error) = fs::rename(&staging, &final_runtime) {
        if let Err(cleanup_error) = remove_directory_blocking(staging.clone()).await {
            return Err(format!(
                "failed to publish Browser Runtime ({error}); additionally failed to remove staging directory ({cleanup_error})"
            )
            .into());
        }
        return Err(error.into());
    }
    staging_guard.disarm();
    activate(&home, &runtime_name)?;
    println!("Browser Runtime installed at {}", final_runtime.display());
    Ok(())
}

async fn install_into(
    staging: &Path,
    node_asset: NodeAsset,
    browser_asset: BrowserAsset,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Downloading private Node Runtime {NODE_VERSION}...");
    let node_url = format!(
        "https://nodejs.org/dist/v{NODE_VERSION}/{}",
        node_asset.archive
    );
    let node_archive = download_sha256(&node_url, node_asset.sha256).await?;

    println!("Downloading playwright-core {PLAYWRIGHT_VERSION}...");
    let playwright_url = format!(
        "https://registry.npmjs.org/playwright-core/-/playwright-core-{PLAYWRIGHT_VERSION}.tgz"
    );
    let playwright_archive = download_sha256(&playwright_url, PLAYWRIGHT_SHA256).await?;

    let staging = staging.to_owned();
    let blocking_staging = staging.clone();
    tokio::task::block_in_place(move || {
        prepare_runtime(
            &blocking_staging,
            node_asset,
            &node_archive,
            &playwright_archive,
        )
    })
    .map_err(io::Error::other)?;

    println!("Downloading Chromium Headless Shell {CHROMIUM_VERSION}...");
    let chromium_archive = download_browser_archive(browser_asset).await?;

    tokio::task::block_in_place(move || {
        extract_zip(
            &chromium_archive,
            &staging
                .join("browsers")
                .join(format!("chromium_headless_shell-{CHROMIUM_REVISION}")),
            0,
        )?;
        finalize_runtime(&staging, node_asset, browser_asset)
    })
    .map_err(io::Error::other)?;
    Ok(())
}

async fn check() -> Result<(), Box<dyn std::error::Error>> {
    let home = browser_home()?;
    ensure_runtime_home(&home)?;
    let _lock = acquire_lock(&home)?;
    let runtime = active_runtime(&home)?
        .ok_or("Browser Runtime is not installed; run `bkmqb browser install` first")?;
    check_runtime(&runtime).await
}

async fn check_runtime(runtime: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = runtime.to_owned();
    let validation_runtime = runtime.clone();
    tokio::task::spawn_blocking(move || validate_runtime(&validation_runtime))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
    let mut command = Command::new(node_executable(&runtime));
    command
        .arg(runtime.join("worker.js"))
        .env("PLAYWRIGHT_BROWSERS_PATH", runtime.join("browsers"))
        .current_dir(&runtime)
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| "Browser Runtime check timed out after 30 seconds")??;
    if !output.status.success() {
        return Err(format!(
            "Browser Runtime check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if result.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("Browser Runtime returned an invalid health-check result".into());
    }
    println!(
        "Browser Runtime check passed ({})",
        result
            .get("browser")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Chromium")
    );
    Ok(())
}

async fn remove_directory_blocking(path: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || fs::remove_dir_all(path))
        .await
        .map_err(io::Error::other)?
}

async fn remove() -> Result<(), Box<dyn std::error::Error>> {
    tokio::task::spawn_blocking(|| {
        remove_blocking().map_err(|error| io::Error::other(error.to_string()))
    })
    .await
    .map_err(io::Error::other)??;
    Ok(())
}

fn remove_blocking() -> Result<(), Box<dyn std::error::Error>> {
    let home = browser_home()?;
    ensure_runtime_home(&home)?;
    let _lock = acquire_lock(&home)?;
    let runtimes = home.join("runtimes");
    let mut removed = Vec::new();
    if runtimes.is_dir() {
        for entry in fs::read_dir(&runtimes)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("pw-")
                && !name.starts_with(".installing-")
                && !name.starts_with(".cleanup-")
            {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata_is_link_like(&metadata) {
                return Err(
                    format!("refusing to remove symlinked Browser Runtime entry `{name}`").into(),
                );
            }
            if metadata.is_dir() {
                fs::remove_dir_all(entry.path())?;
                removed.push(name.to_owned());
            }
        }
    }
    let active = home.join("active.json");
    match fs::symlink_metadata(&active) {
        Ok(metadata) if metadata_is_link_like(&metadata) || !metadata.is_file() => {
            return Err("Browser Runtime active record must be a regular file".into());
        }
        Ok(_) => fs::remove_file(active)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if removed.is_empty() {
        println!("Browser Runtime is not installed");
        return Ok(());
    }
    println!("Removed {} Browser Runtime version(s)", removed.len());
    println!("Temporary browser artifacts are not retained by this runtime manager.");
    Ok(())
}

fn cleanup_stale_staging(runtimes: &Path) -> io::Result<()> {
    for entry in fs::read_dir(runtimes)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(started_ms) = name
            .strip_prefix(".installing-")
            .or_else(|| name.strip_prefix(".cleanup-"))
            .and_then(|suffix| suffix.split_once('-'))
            .and_then(|(timestamp, _)| timestamp.parse::<u128>().ok())
        else {
            continue;
        };
        if now_ms().saturating_sub(started_ms) < STALE_STAGING_AGE_MS {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata_is_link_like(&metadata) || !metadata.is_dir() {
            return Err(io::Error::other(format!(
                "refusing to clean unsafe Browser Runtime staging entry `{name}`"
            )));
        }
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

async fn download_sha256(
    url: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bytes = download_response(url, None).await?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected_sha256 {
        return Err(format!(
            "Browser Runtime download checksum mismatch: expected {expected_sha256}, got {actual}"
        )
        .into());
    }
    Ok(bytes)
}

async fn download_browser_archive(
    asset: BrowserAsset,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let url = format!("{}?generation={}", asset.metadata_url(), asset.generation);
    let client = download_client()?;
    let response = client.head(&url).send().await?.error_for_status()?;
    validate_browser_response(&response, asset, asset.size).map_err(io::Error::other)?;

    let part_size = asset.size.div_ceil(BROWSER_DOWNLOAD_PARTS);
    let mut tasks = tokio::task::JoinSet::new();
    let mut part_count = 0_usize;
    for part in 0..BROWSER_DOWNLOAD_PARTS {
        let start = part * part_size;
        if start >= asset.size {
            break;
        }
        let end = (start + part_size - 1).min(asset.size - 1);
        let client = client.clone();
        let url = url.clone();
        tasks.spawn(async move {
            let bytes = download_browser_range(client, url, asset, start, end).await?;
            let part = usize::try_from(part)
                .map_err(|_| "Chromium byte range index is too large".to_owned())?;
            Ok::<_, String>((part, start, bytes))
        });
        part_count += 1;
    }

    let archive_len = usize::try_from(asset.size)
        .map_err(|_| "Chromium archive is too large for this platform")?;
    let mut archive = vec![0_u8; archive_len];
    let mut received = vec![false; part_count];
    while let Some(result) = tasks.join_next().await {
        let (part, start, bytes) = result
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        let start = usize::try_from(start).map_err(|_| "Chromium byte range is too large")?;
        let end = start
            .checked_add(bytes.len())
            .ok_or("Chromium byte range overflow")?;
        archive
            .get_mut(start..end)
            .ok_or("Chromium byte range exceeds the expected archive size")?
            .copy_from_slice(&bytes);
        let received_part = received
            .get_mut(part)
            .ok_or("Chromium byte range index is invalid")?;
        if *received_part {
            return Err("Chromium byte range was received more than once".into());
        }
        *received_part = true;
    }
    if received.iter().any(|received| !received) {
        return Err("Chromium byte range download was incomplete".into());
    }
    Ok(archive)
}

async fn download_browser_range(
    client: reqwest::Client,
    url: String,
    asset: BrowserAsset,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, String> {
    let mut last_error = String::new();
    for attempt in 1..=BROWSER_DOWNLOAD_ATTEMPTS {
        match download_browser_range_once(&client, &url, asset, start, end).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                last_error = error;
                if attempt < BROWSER_DOWNLOAD_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
            }
        }
    }
    Err(format!(
        "Chromium byte range {start}-{end} failed after {BROWSER_DOWNLOAD_ATTEMPTS} attempts: {last_error}"
    ))
}

async fn download_browser_range_once(
    client: &reqwest::Client,
    url: &str,
    asset: BrowserAsset,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, String> {
    let expected_size = end - start + 1;
    let expected_range = format!("bytes {start}-{end}/{}", asset.size);
    let response = client
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(format!(
            "Chromium server ignored byte range {start}-{end}: {}",
            response.status()
        ));
    }
    validate_browser_response(&response, asset, expected_size)
        .map_err(|error| error.to_string())?;
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok());
    if content_range != Some(expected_range.as_str()) {
        return Err(format!(
            "Chromium Content-Range mismatch: expected {expected_range}, got {}",
            content_range.unwrap_or("missing")
        ));
    }
    read_bounded(response, Some(expected_size))
        .await
        .map_err(|error| error.to_string())
}

fn validate_browser_response(
    response: &reqwest::Response,
    asset: BrowserAsset,
    expected_content_length: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let generation = response
        .headers()
        .get("x-goog-generation")
        .and_then(|value| value.to_str().ok());
    if generation != Some(asset.generation) {
        return Err(format!(
            "Chromium object generation mismatch: expected {}, got {}",
            asset.generation,
            generation.unwrap_or("missing")
        )
        .into());
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok());
    let expected_etag = format!("\"{}\"", asset.etag);
    if etag != Some(expected_etag.as_str()) {
        return Err(format!(
            "Chromium object ETag mismatch: expected {expected_etag}, got {}",
            etag.unwrap_or("missing")
        )
        .into());
    }
    let content_length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if content_length != Some(expected_content_length) {
        return Err(format!(
            "Chromium response size mismatch: expected {}, got {}",
            expected_content_length,
            content_length.map_or_else(|| "missing".to_owned(), |length| length.to_string())
        )
        .into());
    }
    Ok(())
}

async fn download_response(
    url: &str,
    expected_size: Option<u64>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let response = download_client()?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    read_bounded(response, expected_size).await
}

fn download_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .https_only(true)
        .http1_only()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()
}

async fn read_bounded(
    mut response: reqwest::Response,
    expected_size: Option<u64>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if let Some(length) = response.content_length() {
        if length > MAX_DOWNLOAD_BYTES {
            return Err("Browser Runtime download exceeds the size limit".into());
        }
        if expected_size.is_some_and(|expected| expected != length) {
            return Err(format!(
                "Browser Runtime download size mismatch: expected {}, got {length}",
                expected_size.unwrap_or_default()
            )
            .into());
        }
    }
    let capacity = expected_size
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or_default();
    let mut bytes = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await? {
        let next_size = u64::try_from(bytes.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if next_size > MAX_DOWNLOAD_BYTES {
            return Err("Browser Runtime download exceeds the size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if expected_size.is_some_and(|expected| expected != actual_size) {
        return Err(format!(
            "Browser Runtime download size mismatch: expected {}, got {actual_size}",
            expected_size.unwrap_or_default()
        )
        .into());
    }
    Ok(bytes)
}

fn prepare_runtime(
    staging: &Path,
    node_asset: NodeAsset,
    node_archive: &[u8],
    playwright_archive: &[u8],
) -> InstallResult<()> {
    match node_asset.format {
        ArchiveFormat::TarGz => extract_tar_gz(node_archive, &staging.join("node"), 1)?,
        ArchiveFormat::Zip => extract_zip(node_archive, &staging.join("node"), 1)?,
    }
    extract_tar_gz(playwright_archive, &staging.join("playwright"), 1)?;
    fs::write(staging.join("worker.js"), WORKER_SOURCE)?;
    Ok(())
}

fn finalize_runtime(
    staging: &Path,
    node_asset: NodeAsset,
    browser_asset: BrowserAsset,
) -> InstallResult<()> {
    harden_runtime_directories(staging)?;
    let manifest = BrowserRuntimeManifest {
        schema_version: 1,
        node_version: NODE_VERSION.to_owned(),
        playwright_version: PLAYWRIGHT_VERSION.to_owned(),
        chromium_version: CHROMIUM_VERSION.to_owned(),
        chromium_generation: browser_asset.generation.to_owned(),
        platform: node_asset.platform.to_owned(),
    };
    fs::write(
        staging.join("runtime.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    validate_runtime(staging)?;
    Ok(())
}

fn harden_runtime_directories(root: &Path) -> InstallResult<()> {
    harden_runtime_directories_at_depth(root, 0)
}

fn harden_runtime_directories_at_depth(root: &Path, depth: usize) -> InstallResult<()> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err("Browser Runtime directory nesting exceeds the depth limit".into());
    }
    let metadata = fs::symlink_metadata(root)?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err("Browser Runtime root must be a regular directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata_is_link_like(&metadata) {
            return Err("Browser Runtime contains a symlink".into());
        }
        if metadata.is_dir() {
            harden_runtime_directories_at_depth(&entry.path(), depth + 1)?;
        } else if !metadata.is_file() {
            return Err("Browser Runtime contains an unsupported file type".into());
        }
    }
    Ok(())
}

fn extract_tar_gz(
    archive: &[u8],
    destination: &Path,
    strip_components: usize,
) -> InstallResult<()> {
    fs::create_dir_all(destination)?;
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut archive = tar::Archive::new(decoder);
    let mut entry_count = 0_usize;
    let mut extracted_bytes = 0_u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        entry_count = entry_count.saturating_add(1);
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err("Browser Runtime archive contains too many entries".into());
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            // The official Node archive includes npm/corepack convenience
            // links. The browser runtime invokes only the regular `node`
            // executable, so links are unnecessary and are deliberately not
            // materialized from downloaded archives.
            continue;
        }
        let path = entry.path()?;
        let stripped = strip_path(&path, strip_components)?;
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(stripped);
        if entry_type.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if !entry_type.is_file() {
            return Err("Browser Runtime archive contains an unsupported entry type".into());
        }
        let size = entry.size();
        validate_extracted_size(size, &mut extracted_bytes)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        let mode = entry.header().mode()?;
        let mut output = fs::File::create(&target)?;
        let copied = io::copy(&mut entry, &mut output)?;
        if copied != size {
            return Err("Browser Runtime archive entry size changed during extraction".into());
        }
        drop(output);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

fn extract_zip(archive: &[u8], destination: &Path, strip_components: usize) -> InstallResult<()> {
    fs::create_dir_all(destination)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(archive))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err("Browser Runtime archive contains too many entries".into());
    }
    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(path) = entry.enclosed_name() else {
            return Err("Browser Runtime archive contains an unsafe path".into());
        };
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            continue;
        }
        let stripped = strip_path(&path, strip_components)?;
        if stripped.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(stripped);
        if entry.is_dir() {
            fs::create_dir_all(target)?;
            continue;
        }
        let size = entry.size();
        validate_extracted_size(size, &mut extracted_bytes)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        let unix_mode = entry.unix_mode();
        let mut output = fs::File::create(&target)?;
        let copied = io::copy(&mut entry, &mut output)?;
        if copied != size {
            return Err("Browser Runtime archive entry size changed during extraction".into());
        }
        drop(output);
        #[cfg(unix)]
        if let Some(mode) = unix_mode {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn validate_extracted_size(size: u64, extracted_bytes: &mut u64) -> InstallResult<()> {
    if size > MAX_EXTRACTED_FILE_BYTES {
        return Err("Browser Runtime archive entry exceeds the size limit".into());
    }
    *extracted_bytes = extracted_bytes
        .checked_add(size)
        .ok_or("Browser Runtime archive size overflow")?;
    if *extracted_bytes > MAX_EXTRACTED_BYTES {
        return Err("Browser Runtime archive exceeds the extracted size limit".into());
    }
    Ok(())
}

fn strip_path(path: &Path, count: usize) -> InstallResult<PathBuf> {
    let components = path.components().collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("Browser Runtime archive contains an unsafe path".into());
    }
    if components.len() <= count {
        return Ok(PathBuf::new());
    }
    if components.len() - count > MAX_DIRECTORY_DEPTH {
        return Err("Browser Runtime archive path exceeds the depth limit".into());
    }
    let mut result = PathBuf::new();
    for component in &components[count..] {
        match component {
            Component::Normal(value) => result.push(value),
            _ => return Err("Browser Runtime archive contains an unsafe path".into()),
        }
    }
    Ok(result)
}

fn browser_home() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os("BKMQB_BROWSER_HOME") {
        if path.is_empty() {
            return Err("BKMQB_BROWSER_HOME must not be empty".into());
        }
        let path = PathBuf::from(path);
        #[cfg(windows)]
        validate_windows_browser_home_override(&path)?;
        return Ok(path);
    }
    if cfg!(windows) {
        let local_data = env::var_os("LOCALAPPDATA").or_else(|| env::var_os("APPDATA"));
        let local_data = local_data
            .ok_or("cannot determine Browser Runtime directory; set BKMQB_BROWSER_HOME")?;
        return Ok(PathBuf::from(local_data).join("BroKnowMyQQBot/browser"));
    }
    if cfg!(target_os = "macos") {
        let home = env::var_os("HOME")
            .ok_or("cannot determine Browser Runtime directory; set BKMQB_BROWSER_HOME")?;
        return Ok(PathBuf::from(home).join("Library/Application Support/BroKnowMyQQBot/browser"));
    }
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("bkmqb/browser"));
    }
    let home = env::var_os("HOME")
        .ok_or("cannot determine Browser Runtime directory; set BKMQB_BROWSER_HOME")?;
    Ok(PathBuf::from(home).join(".local/share/bkmqb/browser"))
}

fn ensure_runtime_home(home: &Path) -> io::Result<()> {
    ensure_directory_no_symlinks(home, true)?;
    ensure_directory_no_symlinks(&home.join("runtimes"), true)?;
    Ok(())
}

#[cfg(windows)]
fn validate_windows_browser_home_override(path: &Path) -> InstallResult<()> {
    let trusted_root = env::var_os("LOCALAPPDATA")
        .or_else(|| env::var_os("APPDATA"))
        .ok_or("cannot determine trusted Windows application data directory")?;
    let trusted_root = PathBuf::from(trusted_root);
    if !path.is_absolute() || !trusted_root.is_absolute() || !path.starts_with(&trusted_root) {
        return Err(
            "BKMQB_BROWSER_HOME on Windows must be inside the current user's application data directory"
                .into(),
        );
    }
    Ok(())
}

fn ensure_directory_no_symlinks(path: &Path, private_leaf: bool) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other(
            "Browser Runtime directory must be an absolute path",
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(io::Error::other(
            "Browser Runtime directory must not contain `.` or `..` components",
        ));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match component {
            Component::Prefix(_) | Component::RootDir => continue,
            Component::Normal(_) => {}
            Component::CurDir | Component::ParentDir => unreachable!("validated above"),
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                validate_directory_component(&current, &metadata, private_leaf && current == path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt as _;
                    builder.mode(0o700);
                }
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                let metadata = fs::symlink_metadata(&current)?;
                validate_directory_component(&current, &metadata, private_leaf && current == path)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn metadata_is_link_like(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

fn validate_directory_component(
    path: &Path,
    metadata: &fs::Metadata,
    private: bool,
) -> io::Result<()> {
    if metadata_is_link_like(metadata) || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "Browser Runtime directory component `{}` must be a regular directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if private && metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(io::Error::other(format!(
                "Browser Runtime directory component `{}` must be owned by the current user",
                path.display()
            )));
        }
        let mode = metadata.permissions().mode();
        if private && mode & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "Browser Runtime directory component `{}` must be private",
                path.display()
            )));
        }
        if !private && mode & 0o022 != 0 && mode & 0o1000 == 0 {
            return Err(io::Error::other(format!(
                "Browser Runtime directory component `{}` must not be writable by other users",
                path.display()
            )));
        }
    }
    Ok(())
}

fn active_runtime(home: &Path) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    let active = home.join("active.json");
    let metadata = match fs::symlink_metadata(&active) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata_is_link_like(&metadata) || !metadata.is_file() {
        return Err("Browser Runtime active record must be a regular file".into());
    }
    let name: String = serde_json::from_slice(&fs::read(active)?)?;
    if name.is_empty()
        || Path::new(&name).components().count() != 1
        || !matches!(
            Path::new(&name).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err("Browser Runtime active record contains an unsafe path".into());
    }
    let runtime = home.join("runtimes").join(name);
    let metadata = fs::symlink_metadata(&runtime)
        .map_err(|_| "Browser Runtime active record points to a missing directory".to_owned())?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err("Browser Runtime active record points to a missing directory".into());
    }
    Ok(Some(runtime))
}

fn activate(home: &Path, runtime_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = home.join(format!(".active-{}.json", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(runtime_name)?)?;
    file.sync_all()?;
    drop(file);
    let active = home.join("active.json");
    if let Err(error) = replace_active_file(&temporary, &active) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_active_file(temporary: &Path, active: &Path) -> io::Result<()> {
    fs::rename(temporary, active)
}

#[cfg(windows)]
fn replace_active_file(temporary: &Path, active: &Path) -> io::Result<()> {
    let active_metadata = match fs::symlink_metadata(active) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = active_metadata {
        if metadata_is_link_like(&metadata) || !metadata.is_file() {
            return Err(io::Error::other(
                "Browser Runtime active record must be a regular file",
            ));
        }
        let backup = active.with_file_name(format!(".active-backup-{}.json", uuid::Uuid::new_v4()));
        fs::rename(active, &backup)?;
        if let Err(error) = fs::rename(temporary, active) {
            return match fs::rename(&backup, active) {
                Ok(()) => Err(error),
                Err(restore_error) => Err(io::Error::other(format!(
                    "failed to activate Browser Runtime ({error}) and restore the previous activation record ({restore_error})"
                ))),
            };
        }
        let _ = fs::remove_file(backup);
        return Ok(());
    }
    fs::rename(temporary, active)
}

fn acquire_lock(home: &Path) -> Result<fs::File, Box<dyn std::error::Error>> {
    let lock_path = home.join(".browser.lock");
    if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| metadata_is_link_like(&metadata)) {
        return Err("Browser Runtime lock must not be a symlink".into());
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    lock.try_lock_exclusive()
        .map_err(|error| format!("another Browser Runtime operation is active: {error}"))?;
    Ok(lock)
}

fn read_manifest(runtime: &Path) -> InstallResult<BrowserRuntimeManifest> {
    Ok(serde_json::from_slice(&fs::read(
        runtime.join("runtime.json"),
    )?)?)
}

fn validate_runtime(runtime: &Path) -> InstallResult<()> {
    let metadata = fs::symlink_metadata(runtime)?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err("Browser Runtime root must be a regular directory".into());
    }
    let node_asset = node_asset()?;
    let browser_asset = browser_asset()?;
    validate_runtime_file(runtime, Path::new("runtime.json"))?;
    let manifest = read_manifest(runtime)?;
    if manifest.schema_version != 1
        || manifest.node_version != NODE_VERSION
        || manifest.playwright_version != PLAYWRIGHT_VERSION
        || manifest.chromium_version != CHROMIUM_VERSION
        || manifest.chromium_generation != browser_asset.generation
        || manifest.platform != node_asset.platform
    {
        return Err("Browser Runtime manifest is incompatible with this bkmqb build".into());
    }
    validate_runtime_file(runtime, node_executable_relative())?;
    validate_runtime_file(runtime, Path::new("worker.js"))?;
    validate_runtime_file(runtime, Path::new("playwright/index.js"))?;
    validate_runtime_file(runtime, Path::new("playwright/browsers.json"))?;
    validate_file_checksum(
        "Browser Runtime worker",
        &runtime.join("worker.js"),
        WORKER_SHA256,
    )?;
    validate_tree_checksum(
        "Node Runtime",
        &runtime.join("node"),
        node_asset.tree_sha256,
        &[],
    )?;
    validate_tree_checksum(
        "playwright-core",
        &runtime.join("playwright"),
        PLAYWRIGHT_TREE_SHA256,
        &[],
    )?;
    validate_playwright_browser_metadata(runtime)?;
    validate_runtime_file(
        runtime,
        &Path::new("browsers")
            .join(format!("chromium_headless_shell-{CHROMIUM_REVISION}"))
            .join(browser_asset.executable_path),
    )?;
    validate_browser_tree(runtime, browser_asset)?;
    Ok(())
}

fn validate_playwright_browser_metadata(runtime: &Path) -> InstallResult<()> {
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(runtime.join("playwright/browsers.json"))?)?;
    let browser = metadata
        .get("browsers")
        .and_then(serde_json::Value::as_array)
        .and_then(|browsers| {
            browsers.iter().find(|browser| {
                browser.get("name").and_then(serde_json::Value::as_str)
                    == Some("chromium-headless-shell")
            })
        })
        .ok_or("playwright-core metadata does not declare Chromium Headless Shell")?;
    let revision = browser.get("revision").and_then(serde_json::Value::as_str);
    let browser_version = browser
        .get("browserVersion")
        .and_then(serde_json::Value::as_str);
    if revision != Some(CHROMIUM_REVISION) || browser_version != Some(CHROMIUM_VERSION) {
        return Err(format!(
            "playwright-core browser metadata mismatch: expected Chromium {CHROMIUM_VERSION} revision {CHROMIUM_REVISION}"
        )
        .into());
    }
    Ok(())
}

fn validate_runtime_file(runtime: &Path, relative: &Path) -> InstallResult<()> {
    let mut current = runtime.to_owned();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            return Err("Browser Runtime contains an unsafe internal path".into());
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata_is_link_like(&metadata) {
            return Err("Browser Runtime contains a symlinked internal path".into());
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            return Err("Browser Runtime layout is invalid".into());
        }
    }
    Ok(())
}

fn validate_browser_tree(runtime: &Path, asset: BrowserAsset) -> InstallResult<()> {
    let root = runtime
        .join("browsers")
        .join(format!("chromium_headless_shell-{CHROMIUM_REVISION}"));
    validate_tree_checksum(
        "Chromium installation",
        &root,
        asset.tree_sha256,
        &["INSTALLATION_COMPLETE", "DEPENDENCIES_VALIDATED"],
    )
}

#[cfg(test)]
fn browser_tree_sha256(root: &Path) -> InstallResult<String> {
    directory_tree_sha256(root, &["INSTALLATION_COMPLETE", "DEPENDENCIES_VALIDATED"])
}

fn validate_tree_checksum(
    label: &str,
    root: &Path,
    expected: &str,
    ignored_file_names: &[&str],
) -> InstallResult<()> {
    let actual = directory_tree_sha256(root, ignored_file_names)?;
    if actual != expected {
        return Err(format!("{label} checksum mismatch: expected {expected}, got {actual}").into());
    }
    Ok(())
}

fn validate_file_checksum(label: &str, path: &Path, expected: &str) -> InstallResult<()> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected {
        return Err(format!("{label} checksum mismatch: expected {expected}, got {actual}").into());
    }
    Ok(())
}

fn directory_tree_sha256(root: &Path, ignored_file_names: &[&str]) -> InstallResult<String> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata_is_link_like(&metadata) || !metadata.is_dir() {
        return Err("Chromium installation root must be a regular directory".into());
    }
    let mut files = Vec::new();
    collect_tree_files(root, root, ignored_file_names, 0, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut tree_hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    for (relative, path) in files {
        let mut file = fs::File::open(path)?;
        let mut file_hasher = Sha256::new();
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file_hasher.update(&buffer[..read]);
        }
        let line = format!("{}  ./{relative}\n", hex::encode(file_hasher.finalize()));
        tree_hasher.update(line.as_bytes());
    }
    Ok(hex::encode(tree_hasher.finalize()))
}

fn collect_tree_files(
    root: &Path,
    directory: &Path,
    ignored_file_names: &[&str],
    depth: usize,
    files: &mut Vec<(String, PathBuf)>,
) -> InstallResult<()> {
    if depth > MAX_DIRECTORY_DEPTH {
        return Err("Browser Runtime directory nesting exceeds the depth limit".into());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata_is_link_like(&metadata) {
            return Err("Chromium installation contains a symlink".into());
        }
        let path = entry.path();
        if metadata.is_dir() {
            collect_tree_files(root, &path, ignored_file_names, depth + 1, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err("Chromium installation contains an unsupported file type".into());
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| ignored_file_names.contains(&name))
        {
            continue;
        }
        let relative = path.strip_prefix(root)?;
        let mut normalized = String::new();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                return Err("Chromium installation contains an unsafe path".into());
            };
            let component = component
                .to_str()
                .ok_or("Chromium installation contains a non-UTF-8 path")?;
            if !normalized.is_empty() {
                normalized.push('/');
            }
            normalized.push_str(component);
        }
        if normalized.is_empty() || normalized.contains('\n') || normalized.contains('\r') {
            return Err("Chromium installation contains an unsafe path".into());
        }
        files.push((normalized, path));
    }
    Ok(())
}

fn node_executable(runtime: &Path) -> PathBuf {
    runtime.join(node_executable_relative())
}

fn node_executable_relative() -> &'static Path {
    if cfg!(windows) {
        Path::new("node/node.exe")
    } else {
        Path::new("node/bin/node")
    }
}

fn node_asset() -> InstallResult<NodeAsset> {
    node_asset_for(env::consts::OS, env::consts::ARCH)
}

fn node_asset_for(os: &str, arch: &str) -> InstallResult<NodeAsset> {
    match (os, arch) {
        ("linux", "x86_64") => Ok(NodeAsset {
            platform: "linux-x64",
            archive: "node-v22.23.2-linux-x64.tar.gz",
            sha256: "b294a556e639d64338823920e5866c21c02741742d2e1529ee1a225c1ec9252a",
            tree_sha256: "17209f0302b54fa15c11e8146b7fd81d4913983f389717673563c16450c0664a",
            format: ArchiveFormat::TarGz,
        }),
        ("linux", "aarch64") => Ok(NodeAsset {
            platform: "linux-arm64",
            archive: "node-v22.23.2-linux-arm64.tar.gz",
            sha256: "013b59cfd2819703a6f4a14ab891fc46fc2a4e3f5bcd92de3fb4929b43e35b30",
            tree_sha256: "2c1f747c538ec2030f6244c397c50515d7d1de36567a49af93146507120f469b",
            format: ArchiveFormat::TarGz,
        }),
        ("macos", "x86_64") => Ok(NodeAsset {
            platform: "darwin-x64",
            archive: "node-v22.23.2-darwin-x64.tar.gz",
            sha256: "58e99022c2ff89395576cc7fd4d98cea24bb68081475d5f88b801ee8729fb026",
            tree_sha256: "5cffd17e4ae9e88d82848890efcd22601f23d155fadd0bdd712db29b35e42fd7",
            format: ArchiveFormat::TarGz,
        }),
        ("macos", "aarch64") => Ok(NodeAsset {
            platform: "darwin-arm64",
            archive: "node-v22.23.2-darwin-arm64.tar.gz",
            sha256: "61130f394c1630d211dd50aecc4353d379480f36d3ac913cd85dbba1aed585c6",
            tree_sha256: "f88d538a99ac641162b71fc31793fd2d50b9ab85c17ee90426f64aa56edf8932",
            format: ArchiveFormat::TarGz,
        }),
        ("windows", "x86_64") => Ok(NodeAsset {
            platform: "win-x64",
            archive: "node-v22.23.2-win-x64.zip",
            sha256: "1177b4137ba5adaa56354ae40f1080c7450e8ae09cecb47da459d1c52ac99f97",
            tree_sha256: "a2b3ed54d5e13a0cfad717d02b601a183a54e664508973798c7040290882804e",
            format: ArchiveFormat::Zip,
        }),
        ("windows", "aarch64") => Ok(NodeAsset {
            platform: "win-arm64",
            archive: "node-v22.23.2-win-arm64.zip",
            sha256: "fec025a6da31757e3b6af84c5a1628e9d38442ca99a2161091d78f2fcfa35ef3",
            tree_sha256: "995dd7f49efa6299d2b6d478d87cd977185b5db94407cede87f1e1b2b5eaa13f",
            format: ArchiveFormat::Zip,
        }),
        (os, arch) => {
            Err(format!("Browser Runtime installation is not yet supported on {os}/{arch}").into())
        }
    }
}

fn browser_asset() -> InstallResult<BrowserAsset> {
    browser_asset_for(env::consts::OS, env::consts::ARCH)
}

fn browser_asset_for(os: &str, arch: &str) -> InstallResult<BrowserAsset> {
    match (os, arch) {
        ("linux", "x86_64") => Ok(BrowserAsset {
            archive_path: "linux64/chrome-headless-shell-linux64.zip",
            generation: "1784092784777971",
            etag: "792047b3c2625d7d4b0fc7c4fc67d7ad",
            size: 120_231_126,
            executable_path: "chrome-headless-shell-linux64/chrome-headless-shell",
            tree_sha256: "af6b34857dff7461e65422efb0c6a979a819067073c93d1aa3845c4de86f3a29",
        }),
        ("macos", "x86_64") => Ok(BrowserAsset {
            archive_path: "mac-x64/chrome-headless-shell-mac-x64.zip",
            generation: "1784100539127264",
            etag: "b4e627c43cd54b2420cd3ef4d59e5719",
            size: 103_580_477,
            executable_path: "chrome-headless-shell-mac-x64/chrome-headless-shell",
            tree_sha256: "e92372b3e15b5cda7220f2c7198f3a64933f10a5bfdbec78cc5401db8b39efd3",
        }),
        ("macos", "aarch64") => Ok(BrowserAsset {
            archive_path: "mac-arm64/chrome-headless-shell-mac-arm64.zip",
            generation: "1784095106113769",
            etag: "0ac2e86a513fd52f754ff5b8f10022d4",
            size: 99_275_008,
            executable_path: "chrome-headless-shell-mac-arm64/chrome-headless-shell",
            tree_sha256: "3ba62e8e97a14a75db1bfe6e0202f87d09512f510563bcb50ac80060f590db35",
        }),
        ("windows", "x86_64") => Ok(BrowserAsset {
            archive_path: "win64/chrome-headless-shell-win64.zip",
            generation: "1784095614544917",
            etag: "fc189aa92fef165864813212c1314887",
            size: 120_106_945,
            executable_path: "chrome-headless-shell-win64/chrome-headless-shell.exe",
            tree_sha256: "db500cce5a5531262467ce9049a364924dea9c0eec17e3f968bad09a98e95ae5",
        }),
        (os, arch) => Err(format!(
            "Browser Runtime installation is not supported on {os}/{arch} by the pinned Playwright browser build"
        )
        .into()),
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn archive_paths_are_stripped_safely() {
        assert_eq!(
            strip_path(Path::new("package/lib/index.js"), 1).unwrap(),
            PathBuf::from("lib/index.js")
        );
        assert!(strip_path(Path::new("package/../escape"), 1).is_err());
        assert!(strip_path(Path::new("/absolute/path"), 1).is_err());
        let deep = format!(
            "package/{}",
            vec!["nested"; MAX_DIRECTORY_DEPTH + 1].join("/")
        );
        assert!(strip_path(Path::new(&deep), 1).is_err());
    }

    #[test]
    fn extracted_sizes_are_bounded_per_file_and_cumulatively() {
        let mut total = 0;
        validate_extracted_size(MAX_EXTRACTED_FILE_BYTES, &mut total).unwrap();
        assert!(validate_extracted_size(MAX_EXTRACTED_FILE_BYTES + 1, &mut total).is_err());
        total = MAX_EXTRACTED_BYTES;
        assert!(validate_extracted_size(1, &mut total).is_err());
    }

    #[test]
    fn supported_platform_has_a_pinned_node_asset() {
        if matches!(env::consts::OS, "linux" | "macos" | "windows")
            && matches!(env::consts::ARCH, "x86_64" | "aarch64")
        {
            let asset = node_asset().unwrap();
            assert_eq!(asset.sha256.len(), 64);
            assert_eq!(asset.tree_sha256.len(), 64);
            assert!(asset.archive.contains(NODE_VERSION));
        }
    }

    #[test]
    fn windows_assets_are_pinned_zip_archives() {
        for arch in ["x86_64", "aarch64"] {
            let asset = node_asset_for("windows", arch).unwrap();
            assert!(matches!(asset.format, ArchiveFormat::Zip));
            assert_eq!(asset.sha256.len(), 64);
            assert_eq!(Path::new(asset.archive).extension().unwrap(), "zip");
        }
    }

    #[test]
    fn browser_assets_pin_immutable_object_metadata() {
        for (os, arch) in [
            ("linux", "x86_64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("windows", "x86_64"),
        ] {
            let asset = browser_asset_for(os, arch).unwrap();
            assert!(!asset.generation.is_empty());
            assert_eq!(asset.etag.len(), 32);
            assert!(asset.size < MAX_DOWNLOAD_BYTES);
            assert!(asset.metadata_url().contains(CHROMIUM_VERSION));
            assert_eq!(asset.tree_sha256.len(), 64);
        }
        assert!(browser_asset_for("linux", "aarch64").is_err());
        assert!(browser_asset_for("windows", "aarch64").is_err());
    }

    #[test]
    fn zip_archives_are_extracted_after_stripping_the_root() {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "node-v22.23.2-win-x64/node.exe",
                SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(b"node-test").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let destination = test_directory("zip");
        extract_zip(&archive, &destination, 1).unwrap();
        assert_eq!(
            fs::read(destination.join("node.exe")).unwrap(),
            b"node-test"
        );
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn tar_archives_are_extracted_to_the_exact_stripped_path() {
        use flate2::{Compression, write::GzEncoder};

        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let contents = b"node-test";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                "node-v22.23.2-linux-x64/bin/node",
                &contents[..],
            )
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        let archive = encoder.finish().unwrap();
        let destination = test_directory("tar");
        extract_tar_gz(&archive, &destination, 1).unwrap();
        assert_eq!(fs::read(destination.join("bin/node")).unwrap(), contents);
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn activation_replaces_the_record_without_leaving_temporary_files() {
        let home = test_directory("activate");
        ensure_runtime_home(&home).unwrap();
        activate(&home, "first").unwrap();
        activate(&home, "second").unwrap();
        let active: String =
            serde_json::from_slice(&fs::read(home.join("active.json")).unwrap()).unwrap();
        assert_eq!(active, "second");
        assert!(fs::read_dir(&home).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".active-")
        }));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn stale_staging_directories_are_recovered_without_touching_runtimes() {
        let root = test_directory("staging-cleanup");
        let stale = root.join(".installing-0-stale");
        let stale_cleanup = root.join(".cleanup-0-stale");
        let fresh = root.join(format!(".installing-{}-fresh", now_ms()));
        let runtime = root.join("pw-existing");
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(&stale_cleanup).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::write(stale.join("partial"), b"data").unwrap();
        cleanup_stale_staging(&root).unwrap();
        assert!(!stale.exists());
        assert!(!stale_cleanup.exists());
        assert!(fresh.is_dir());
        assert!(runtime.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn playwright_metadata_is_bound_to_the_installed_revision() {
        let runtime = test_directory("playwright-metadata");
        fs::create_dir_all(runtime.join("playwright")).unwrap();
        let metadata = serde_json::json!({
            "browsers": [{
                "name": "chromium-headless-shell",
                "revision": CHROMIUM_REVISION,
                "browserVersion": CHROMIUM_VERSION
            }]
        });
        fs::write(
            runtime.join("playwright/browsers.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        validate_playwright_browser_metadata(&runtime).unwrap();

        let mismatched = serde_json::json!({
            "browsers": [{
                "name": "chromium-headless-shell",
                "revision": "different",
                "browserVersion": CHROMIUM_VERSION
            }]
        });
        fs::write(
            runtime.join("playwright/browsers.json"),
            serde_json::to_vec(&mismatched).unwrap(),
        )
        .unwrap();
        assert!(validate_playwright_browser_metadata(&runtime).is_err());
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn worker_source_matches_its_pinned_checksum() {
        assert_eq!(
            hex::encode(Sha256::digest(WORKER_SOURCE.as_bytes())),
            WORKER_SHA256
        );
        assert_eq!(PLAYWRIGHT_TREE_SHA256.len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_home_rejects_a_symlinked_runtimes_directory() {
        use std::os::unix::fs::symlink;

        let home = test_directory("home-symlink");
        let outside = test_directory("home-symlink-outside");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, home.join("runtimes")).unwrap();
        assert!(ensure_runtime_home(&home).is_err());
        fs::remove_file(home.join("runtimes")).unwrap();
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_home_rejects_non_sticky_world_writable_components() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = test_directory("home-permissions");
        fs::create_dir_all(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_runtime_home(&home).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(ensure_runtime_home(&home).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn runtime_home_requires_a_normalized_absolute_path() {
        assert!(ensure_directory_no_symlinks(Path::new("relative/browser"), true).is_err());
        let with_parent = env::temp_dir()
            .join("bkmqb-browser-unused")
            .join("..")
            .join("browser");
        assert!(ensure_directory_no_symlinks(&with_parent, true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directories_are_hardened_before_publication() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_directory("directory-hardening");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::set_permissions(root.join("nested"), fs::Permissions::from_mode(0o777)).unwrap();
        fs::write(root.join("nested/file"), b"data").unwrap();
        harden_runtime_directories(&root).unwrap();
        assert_eq!(
            fs::metadata(root.join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn browser_tree_hash_is_deterministic_and_ignores_markers() {
        let root = test_directory("tree-hash");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("z.txt"), b"last").unwrap();
        fs::write(root.join("nested/a.txt"), b"first").unwrap();
        let hash = browser_tree_sha256(&root).unwrap();
        fs::write(root.join("INSTALLATION_COMPLETE"), b"ignored").unwrap();
        fs::write(root.join("nested/DEPENDENCIES_VALIDATED"), b"ignored").unwrap();
        assert_eq!(browser_tree_sha256(&root).unwrap(), hash);
        fs::write(root.join("nested/a.txt"), b"changed").unwrap();
        assert_ne!(browser_tree_sha256(&root).unwrap(), hash);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn browser_tree_hash_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let root = test_directory("tree-symlink");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("target"), b"data").unwrap();
        symlink(root.join("target"), root.join("link")).unwrap();
        assert!(browser_tree_sha256(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    fn test_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "bkmqb-browser-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }
}
