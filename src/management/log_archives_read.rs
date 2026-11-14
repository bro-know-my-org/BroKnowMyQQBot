use super::{ArchiveFile, Archives, failure};
use crate::logging::{archive_normalize::normalize, live::LogEntry};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit as _, Mac as _};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::io::{self, BufRead, BufReader, Read};

const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LINES: u64 = 100_000;
const MAX_PAGE_SCAN: u64 = 5000;
const MAX_LINE: usize = 16 * 1024;
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct PageQuery {
    selector: String,
    cursor: Option<String>,
    limit: Option<usize>,
}
#[derive(Debug, Serialize)]
struct Page {
    selector: String,
    entries: Vec<LogEntry>,
    next_cursor: Option<String>,
    skipped: u64,
    scanned: u64,
    limited: bool,
}
type Failure = (StatusCode, &'static str);
const CHANGED: Failure = (
    StatusCode::CONFLICT,
    "归档已变化或轮转，请刷新文件列表后重试",
);
const UNAVAILABLE: Failure = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "归档损坏、不可读或超出解压窗口限制",
);
const INVALID: Failure = (
    StatusCode::BAD_REQUEST,
    "历史日志选择器、游标或分页参数无效",
);

pub(super) async fn page(
    State(state): State<Archives>,
    Query(query): Query<PageQuery>,
) -> Response {
    if query.selector.len() != 64
        || !query.selector.bytes().all(|b| b.is_ascii_hexdigit())
        || !(1..=200).contains(&query.limit.unwrap_or(200))
        || query.cursor.as_ref().is_some_and(|c| c.len() > 160)
    {
        return failure(INVALID.0, INVALID.1);
    }
    let Ok(permit) = state.slots.clone().try_acquire_owned() else {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "历史日志读取繁忙，请稍后重试",
        );
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        state.read_page(&query)
    })
    .await
    {
        Ok(Ok(page)) => Json(page).into_response(),
        Ok(Err((status, message))) => failure(status, message),
        Err(error) => {
            tracing::warn!(error = %error, "archive page worker failed");
            failure(StatusCode::SERVICE_UNAVAILABLE, "历史日志读取任务失败")
        }
    }
}
impl Archives {
    fn cursor_mac(&self, selector: &str, offset: u64) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.epoch.as_bytes())
            .expect("HMAC accepts any key length");
        mac.update(b"archive-cursor-v1");
        mac.update(selector.as_bytes());
        mac.update(&offset.to_le_bytes());
        mac
    }
    fn cursor(&self, selector: &str, offset: u64) -> String {
        URL_SAFE_NO_PAD.encode(format!(
            "{offset}:{}",
            hex::encode(self.cursor_mac(selector, offset).finalize().into_bytes())
        ))
    }
    fn offset(&self, query: &PageQuery) -> Result<u64, Failure> {
        let Some(cursor) = &query.cursor else {
            return Ok(0);
        };
        let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| INVALID)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| INVALID)?;
        let (number, tag) = text.split_once(':').ok_or(INVALID)?;
        let offset = number.parse::<u64>().map_err(|_| INVALID)?;
        let tag = hex::decode(tag).map_err(|_| INVALID)?;
        if offset > MAX_LINES || number != offset.to_string() {
            return Err(INVALID);
        }
        self.cursor_mac(&query.selector, offset)
            .verify_slice(&tag)
            .map_err(|_| INVALID)?;
        Ok(offset)
    }
    #[cfg(unix)]
    fn open_archive(&self, item: &ArchiveFile) -> Result<std::fs::File, Failure> {
        let root = self
            .logs
            .archive_directory()
            .ok_or(CHANGED)?
            .join("runtime");
        let directory = super::safe_directory(&root).map_err(|_| CHANGED)?;
        let month =
            super::open_relative(&directory, item.month.as_str(), true).map_err(|_| CHANGED)?;
        let file = super::open_relative(&month, item.name.as_str(), false).map_err(|_| CHANGED)?;
        let metadata = file.metadata().map_err(|_| CHANGED)?;
        if !metadata.is_file() || self.selector(&item.month, &item.name, &metadata) != item.selector
        {
            return Err(CHANGED);
        }
        Ok(file)
    }
    #[cfg(unix)]
    fn read_page(&self, query: &PageQuery) -> Result<Page, Failure> {
        let offset = self.offset(query)?;
        let inventory = self.inventory().map_err(|_| UNAVAILABLE)?;
        let item = inventory
            .files
            .into_iter()
            .find(|file| file.selector == query.selector)
            .ok_or(CHANGED)?;
        if !item.available {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "归档大小超出读取上限"));
        }
        let file = self.open_archive(&item)?;
        // Bound compressed input and zstd's internal window before reading output.
        let input = (&file).take(item.bytes);
        let input: Box<dyn Read + '_> = if item.compressed {
            let mut decoder = zstd::stream::read::Decoder::new(input).map_err(|_| UNAVAILABLE)?;
            decoder.window_log_max(23).map_err(|_| UNAVAILABLE)?;
            Box::new(decoder)
        } else {
            Box::new(input)
        };
        let mut reader = BufReader::new(input.take(MAX_SCAN_BYTES + 1));
        let mut page = Page {
            selector: item.selector.clone(),
            entries: vec![],
            next_cursor: None,
            skipped: 0,
            scanned: 0,
            limited: false,
        };
        let mut lines = 0;
        let mut bytes = 0;
        while lines < MAX_LINES
            && page.entries.len() < query.limit.unwrap_or(200)
            && page.scanned < MAX_PAGE_SCAN
        {
            let Some((line, valid)) =
                bounded_line(&mut reader, &mut bytes).map_err(|_| UNAVAILABLE)?
            else {
                break;
            };
            lines += 1;
            if bytes > MAX_SCAN_BYTES {
                page.limited = true;
                break;
            }
            if lines <= offset {
                continue;
            }
            page.scanned += 1;
            let row = valid
                .then(|| serde_json::from_slice(&line).ok())
                .flatten()
                .and_then(|v| normalize(&v, lines, &self.logs));
            if let Some(row) = row {
                page.entries.push(row);
            } else {
                page.skipped += 1;
            }
        }
        let remaining = !page.limited && !reader.fill_buf().map_err(|_| UNAVAILABLE)?.is_empty();
        page.limited |= lines >= MAX_LINES && remaining;
        if !page.limited && remaining {
            page.next_cursor = Some(self.cursor(&item.selector, lines));
        }
        // Check both the opened inode and its current writer-managed directory entry.
        if self.selector(
            &item.month,
            &item.name,
            &file.metadata().map_err(|_| CHANGED)?,
        ) != item.selector
        {
            return Err(CHANGED);
        }
        let _verified = self.open_archive(&item)?;
        Ok(page)
    }
    #[cfg(not(unix))]
    fn read_page(&self, _query: &PageQuery) -> Result<Page, Failure> {
        Err(UNAVAILABLE)
    }
}

fn bounded_line(reader: &mut impl BufRead, bytes: &mut u64) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((!line.is_empty() || oversized).then_some((line, false)));
        }
        let newline = buffer.iter().position(|b| *b == b'\n');
        let count = newline.map_or(buffer.len(), |n| n + 1);
        *bytes += count as u64;
        if !oversized && line.len() + count <= MAX_LINE {
            line.extend_from_slice(&buffer[..count]);
        } else {
            oversized = true;
            line.clear();
        }
        reader.consume(count);
        if newline.is_some() || *bytes > MAX_SCAN_BYTES {
            return Ok(Some((line, !oversized && newline.is_some())));
        }
    }
}
