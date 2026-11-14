//! Read-only access to writer-managed runtime archives. Never accepts a path.
#[path = "log_archives_read.rs"]
mod read;
use super::auth::{self, AuthState};
use crate::logging::live::LogBuffer;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde::Serialize;
use serde_json::json;
use sha2::Sha256;
use std::{path::Path, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone)]
struct Archives {
    logs: LogBuffer,
    epoch: Arc<str>,
    slots: Arc<Semaphore>,
}
#[derive(Debug, Serialize)]
struct ArchiveFile {
    selector: String,
    name: String,
    month: String,
    bytes: u64,
    compressed: bool,
    available: bool,
}
#[derive(Debug, Serialize)]
struct Inventory {
    enabled: bool,
    files: Vec<ArchiveFile>,
    truncated: bool,
}
const MAX_ENUMERATED: usize = 4096;
const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_COMPRESSED_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn router(logs: LogBuffer, auth: AuthState) -> Router {
    Router::new()
        .route("/api/logs/archives", get(list))
        .route("/api/logs/archives/page", get(read::page))
        .with_state(Archives {
            logs,
            epoch: uuid::Uuid::new_v4().to_string().into(),
            slots: Arc::new(Semaphore::new(2)),
        })
        .route_layer(middleware::from_fn_with_state(auth, auth::authenticate))
}
async fn list(State(state): State<Archives>) -> Response {
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "历史日志读取繁忙，请稍后重试",
        );
    };
    // The permit stays with the blocking job even if its HTTP waiter disappears.
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        state.inventory()
    })
    .await
    {
        Ok(Ok(inventory)) => Json(inventory).into_response(),
        Ok(Err(_)) => failure(StatusCode::SERVICE_UNAVAILABLE, "历史运行日志目录不可用"),
        Err(error) => {
            tracing::warn!(error = %error, "archive inventory worker failed");
            failure(StatusCode::SERVICE_UNAVAILABLE, "历史日志读取任务失败")
        }
    }
}
fn failure(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}

impl Archives {
    fn inventory(&self) -> std::io::Result<Inventory> {
        let Some(directory) = self.logs.archive_directory() else {
            return Ok(Inventory {
                enabled: false,
                files: vec![],
                truncated: false,
            });
        };
        self.inventory_at(&directory.join("runtime"))
    }

    #[cfg(unix)]
    fn inventory_at(&self, root: &Path) -> std::io::Result<Inventory> {
        use rustix::fs::Dir;
        let root = safe_directory(root)?;
        let mut files = Vec::new();
        let mut count = 0;
        let mut truncated = false;
        'months: for month in Dir::read_from(&root)? {
            let month = month?;
            count += 1;
            if count > MAX_ENUMERATED {
                truncated = true;
                break;
            }
            let Ok(name) = month.file_name().to_str() else {
                continue;
            };
            if !crate::logging::is_month_directory(name) {
                continue;
            }
            let Ok(directory) = open_relative(&root, name, true) else {
                continue;
            };
            for file in Dir::read_from(&directory)? {
                let file = file?;
                count += 1;
                if count > MAX_ENUMERATED || files.len() >= MAX_FILES {
                    truncated = true;
                    break 'months;
                }
                let Ok(filename) = file.file_name().to_str() else {
                    continue;
                };
                if filename.len() > 192 || !crate::logging::is_managed_log_filename(filename) {
                    continue;
                }
                let Ok(file) = open_relative(&directory, filename, false) else {
                    continue;
                };
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    continue;
                }
                let compressed = Path::new(filename)
                    .extension()
                    .is_some_and(|ext| ext == "zst");
                files.push(ArchiveFile {
                    selector: self.selector(name, filename, &metadata),
                    name: filename.to_owned(),
                    month: name.to_owned(),
                    bytes: metadata.len(),
                    compressed,
                    available: metadata.len()
                        <= if compressed {
                            MAX_COMPRESSED_BYTES
                        } else {
                            MAX_FILE_BYTES
                        },
                });
            }
        }
        files.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(Inventory {
            enabled: true,
            files,
            truncated,
        })
    }

    #[cfg(not(unix))]
    fn inventory_at(&self, _root: &Path) -> std::io::Result<Inventory> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe archive access unavailable",
        ))
    }

    #[cfg(unix)]
    fn selector(&self, month: &str, name: &str, metadata: &std::fs::Metadata) -> String {
        use std::os::unix::fs::MetadataExt as _;
        let mut hash = Hmac::<Sha256>::new_from_slice(self.epoch.as_bytes())
            .expect("HMAC accepts any key length");
        hash.update(b"archive-file-v1");
        hash.update(month.as_bytes());
        hash.update(name.as_bytes());
        for part in [
            metadata.dev().to_le_bytes(),
            metadata.ino().to_le_bytes(),
            metadata.len().to_le_bytes(),
            metadata.mtime().to_le_bytes(),
            metadata.mtime_nsec().to_le_bytes(),
            metadata.ctime().to_le_bytes(),
            metadata.ctime_nsec().to_le_bytes(),
        ] {
            hash.update(&part);
        }
        hex::encode(hash.finalize().into_bytes())
    }
}

#[cfg(unix)]
fn open_relative<P: rustix::path::Arg>(
    parent: &std::fs::File,
    name: P,
    directory: bool,
) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let flags = OFlags::RDONLY
        | OFlags::CLOEXEC
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | if directory {
            OFlags::DIRECTORY
        } else {
            OFlags::empty()
        };
    Ok(openat(parent, name, flags, Mode::empty())?.into())
}
#[cfg(unix)]
fn safe_directory(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::path::Component;
    let path = std::path::absolute(path)?;
    let mut directory: std::fs::File = rustix::fs::open(
        "/",
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?
    .into();
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                directory = open_relative(&directory, "..", true)?;
            }
            Component::Normal(name) => {
                directory = open_relative(&directory, name, true)?;
            }
            Component::Prefix(_) => return Err(std::io::Error::other("invalid archive directory")),
        }
    }
    Ok(directory)
}

#[cfg(all(test, unix))]
#[path = "log_archives_tests.rs"]
mod tests;
