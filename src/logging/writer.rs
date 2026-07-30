//! Session-aware JSONL writer, zstd archiver, and capacity enforcement.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use chrono::{Datelike as _, Local};
use fs2::FileExt as _;

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const CAPACITY_LOW_WATER_PERCENT: u64 = 90;

#[derive(Debug)]
pub(super) struct ManagedLogWriter {
    root: PathBuf,
    max_file_bytes: u64,
    max_total_bytes: u64,
    zstd_level: i32,
    session_id: String,
    segment: u32,
    current: Option<CurrentFile>,
    next_maintenance: Instant,
    _lock: File,
}

#[derive(Debug)]
struct CurrentFile {
    file: File,
    path: PathBuf,
    day: String,
    bytes_written: u64,
}

impl ManagedLogWriter {
    pub(super) fn new(
        base_directory: &Path,
        kind: &str,
        max_file_bytes: u64,
        max_total_bytes: u64,
        zstd_level: i32,
        session_id: &str,
    ) -> io::Result<Self> {
        create_private_directory(base_directory)?;
        let root = base_directory.join(kind);
        create_private_directory(&root)?;
        let lock = acquire_log_lock(&root)?;
        archive_closed_logs(&root, None, zstd_level)?;
        enforce_capacity(&root, None, max_total_bytes)?;
        Ok(Self {
            root,
            max_file_bytes,
            max_total_bytes,
            zstd_level,
            session_id: session_id.to_owned(),
            segment: 0,
            current: None,
            next_maintenance: Instant::now() + MAINTENANCE_INTERVAL,
            _lock: lock,
        })
    }

    fn ensure_file(&mut self, incoming_bytes: usize) -> io::Result<()> {
        let now = Local::now();
        let day = now.format("%Y-%m-%d").to_string();
        let should_rotate = self.current.as_ref().is_none_or(|current| {
            current.day != day
                || current.bytes_written.saturating_add(incoming_bytes as u64) > self.max_file_bytes
        });
        if should_rotate {
            self.rotate()?;
            self.open_file(now, day)?;
        }
        if Instant::now() >= self.next_maintenance {
            self.maintain()?;
            self.next_maintenance = Instant::now() + MAINTENANCE_INTERVAL;
        }
        Ok(())
    }

    fn open_file(&mut self, now: chrono::DateTime<Local>, day: String) -> io::Result<()> {
        let month_directory = self
            .root
            .join(format!("{:04}-{:02}", now.year(), now.month()));
        create_private_directory(&month_directory)?;
        loop {
            let timestamp = now.format("%Y%m%dT%H%M%S%.3f%z");
            let filename = format!(
                "{timestamp}-p{}-{}-s{:03}.jsonl",
                std::process::id(),
                self.session_id,
                self.segment
            );
            self.segment = self.segment.saturating_add(1);
            let path = month_directory.join(filename);
            match open_private_file(&path) {
                Ok(file) => {
                    self.current = Some(CurrentFile {
                        file,
                        path,
                        day,
                        bytes_written: 0,
                    });
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        let Some(mut current) = self.current.take() else {
            return Ok(());
        };
        current.file.flush()?;
        current.file.sync_data()?;
        drop(current.file);
        archive_file(&current.path, self.zstd_level)?;
        enforce_capacity(&self.root, None, self.max_total_bytes)
    }

    fn maintain(&mut self) -> io::Result<()> {
        let active = self.current.as_ref().map(|current| current.path.as_path());
        archive_closed_logs(&self.root, active, self.zstd_level)?;
        enforce_capacity(&self.root, active, self.max_total_bytes)
    }
}

impl Write for ManagedLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let incoming = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if incoming > self.max_file_bytes || incoming > self.max_total_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log record exceeds the configured file or total capacity",
            ));
        }
        self.ensure_file(buffer.len())?;
        let active = self.current.as_ref().map(|current| current.path.as_path());
        let projected = managed_log_size(&self.root)?.saturating_add(incoming);
        if projected > self.max_total_bytes {
            enforce_capacity(
                &self.root,
                active,
                self.max_total_bytes.saturating_sub(incoming),
            )?;
            if managed_log_size(&self.root)?.saturating_add(incoming) > self.max_total_bytes {
                return Err(io::Error::other(
                    "active log cannot fit within the configured total capacity",
                ));
            }
        }
        let current = self
            .current
            .as_mut()
            .ok_or_else(|| io::Error::other("log file was not opened"))?;
        let written = current.file.write(buffer)?;
        current.bytes_written = current.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.current
            .as_mut()
            .map_or(Ok(()), |current| current.file.flush())
    }
}

fn archive_closed_logs(root: &Path, active: Option<&Path>, zstd_level: i32) -> io::Result<()> {
    for path in known_log_files(root)? {
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && active.is_none_or(|active| active != path)
        {
            archive_file(&path, zstd_level)?;
        }
    }
    Ok(())
}

fn archive_file(source: &Path, zstd_level: i32) -> io::Result<()> {
    let destination = source.with_extension("jsonl.zst");
    if destination.exists() {
        validate_archive_matches_source(source, &destination)?;
        fs::remove_file(source)?;
        return Ok(());
    }
    let temporary = destination.with_extension("zst.tmp");
    remove_stale_temporary(&temporary)?;
    let mut input = File::open(source)?;
    let mut output = open_private_file(&temporary)?;
    if let Err(error) = zstd::stream::copy_encode(&mut input, &mut output, zstd_level) {
        drop(output);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    output.flush()?;
    output.sync_all()?;
    drop(output);
    fs::rename(&temporary, &destination)?;
    fs::remove_file(source)
}

fn validate_archive_matches_source(source: &Path, archive: &Path) -> io::Result<()> {
    let mut source = BufReader::new(File::open(source)?);
    let mut archive = BufReader::new(zstd::stream::read::Decoder::new(File::open(archive)?)?);
    loop {
        let source_buffer = source.fill_buf()?;
        let archive_buffer = archive.fill_buf()?;
        if source_buffer.is_empty() || archive_buffer.is_empty() {
            return if source_buffer.is_empty() && archive_buffer.is_empty() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "existing log archive does not match its source",
                ))
            };
        }
        let compared = source_buffer.len().min(archive_buffer.len());
        if source_buffer[..compared] != archive_buffer[..compared] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing log archive does not match its source",
            ));
        }
        source.consume(compared);
        archive.consume(compared);
    }
}

fn managed_log_size(root: &Path) -> io::Result<u64> {
    known_log_files(root)?
        .into_iter()
        .try_fold(0_u64, |total, path| {
            Ok(total.saturating_add(fs::metadata(path)?.len()))
        })
}

fn enforce_capacity(root: &Path, active: Option<&Path>, max_total_bytes: u64) -> io::Result<()> {
    let mut files = known_log_files(root)?
        .into_iter()
        .map(|path| {
            let size = fs::metadata(&path)?.len();
            Ok((path, size))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut total = files
        .iter()
        .fold(0_u64, |total, (_, size)| total.saturating_add(*size));
    if total <= max_total_bytes {
        return Ok(());
    }
    files.sort_by(|(left, _), (right, _)| {
        managed_log_timestamp(left)
            .cmp(&managed_log_timestamp(right))
            .then_with(|| left.cmp(right))
    });
    let target = max_total_bytes.saturating_mul(CAPACITY_LOW_WATER_PERCENT) / 100;
    for (path, size) in files {
        if total <= target {
            break;
        }
        if active.is_some_and(|active| active == path) {
            continue;
        }
        fs::remove_file(&path)?;
        total = total.saturating_sub(size);
    }
    remove_empty_month_directories(root)
}

fn known_log_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for month in fs::read_dir(root)? {
        let month = month?;
        let month_name = month.file_name();
        if !is_month_directory(&month_name.to_string_lossy())
            || month.file_type()?.is_symlink()
            || !month.file_type()?.is_dir()
        {
            continue;
        }
        for entry in fs::read_dir(month.path())? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_managed_log_filename(&name) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn is_month_directory(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 7
        && bytes[4] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..].iter().all(u8::is_ascii_digit)
}

fn is_managed_log_filename(name: &str) -> bool {
    let Some(stem) = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let Some((timestamp, suffix)) = stem.split_once("-p") else {
        return false;
    };
    if chrono::DateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.3f%z").is_err() {
        return false;
    }
    let Some((process_id, remainder)) = suffix.split_once('-') else {
        return false;
    };
    let Some((session, segment)) = remainder.rsplit_once("-s") else {
        return false;
    };
    !process_id.is_empty()
        && process_id.bytes().all(|byte| byte.is_ascii_digit())
        && !session.is_empty()
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && segment.len() >= 3
        && segment.bytes().all(|byte| byte.is_ascii_digit())
}

fn remove_stale_temporary(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path)
        }
        Ok(_) => Err(io::Error::other(format!(
            "log archive temporary path is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_empty_month_directories(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && fs::read_dir(entry.path())?.next().is_none() {
            fs::remove_dir(entry.path())?;
        }
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "log directory component is not a real directory: {}",
                        current.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn acquire_log_lock(root: &Path) -> io::Result<File> {
    let path = root.join(".writer.lock");
    if fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log writer lock path is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options.open(path)?;
    lock.try_lock_exclusive()
        .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error))?;
    Ok(lock)
}

fn managed_log_timestamp(path: &Path) -> Option<i64> {
    let name = path.file_name()?.to_str()?;
    let timestamp = name.split_once("-p")?.0;
    chrono::DateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.3f%z")
        .ok()
        .map(|value| value.timestamp_millis())
}

fn open_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

pub(super) fn session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn archives_jsonl_with_zstd_and_preserves_content() {
        let root = temporary_directory("archive");
        let month = root.join("2026-07");
        create_private_directory(&month).unwrap();
        let source = month.join("20260730T000000.000+0800-p1-test-s000.jsonl");
        fs::write(&source, b"{\"event\":\"test\"}\n").unwrap();

        archive_file(&source, 3).unwrap();

        let archive = PathBuf::from(format!("{}.zst", source.display()));
        let mut archive_reader =
            zstd::stream::read::Decoder::new(File::open(archive).unwrap()).unwrap();
        let mut restored = String::new();
        archive_reader.read_to_string(&mut restored).unwrap();
        assert_eq!(restored, "{\"event\":\"test\"}\n");
        assert!(!source.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capacity_cleanup_deletes_oldest_archive_first() {
        let root = temporary_directory("capacity");
        let month = root.join("2026-07");
        create_private_directory(&month).unwrap();
        let oldest = month.join("20260701T000000.000+0800-p1-a-s000.jsonl.zst");
        let newest = month.join("20260702T000000.000+0800-p1-b-s000.jsonl.zst");
        fs::write(&oldest, vec![0_u8; 60]).unwrap();
        fs::write(&newest, vec![0_u8; 60]).unwrap();

        enforce_capacity(&root, None, 100).unwrap();

        assert!(!oldest.exists());
        assert!(newest.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writer_rotates_and_archives_when_file_limit_is_reached() {
        let base = temporary_directory("rotation");
        let mut writer = ManagedLogWriter::new(&base, "runtime", 16, 1_024, 3, "session").unwrap();

        writer.write_all(b"first-record\n").unwrap();
        writer.write_all(b"second-record\n").unwrap();
        writer.flush().unwrap();

        let files = known_log_files(&base.join("runtime")).unwrap();
        assert!(
            files
                .iter()
                .any(|path| path.to_string_lossy().ends_with(".jsonl.zst"))
        );
        assert!(
            files
                .iter()
                .any(|path| path.to_string_lossy().ends_with(".jsonl"))
        );
        drop(writer);
        fs::remove_dir_all(base).unwrap();
    }

    fn temporary_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bkm-log-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
