//! Multi-sink tracing initialization and log lifecycle management.

mod console;
mod writer;

use std::io::{self, IsTerminal as _};

use thiserror::Error;
use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{
    EnvFilter, Layer as _, filter::filter_fn, fmt, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

use crate::config::LoggingConfig;
use console::{ConsoleFormat, ConsoleLanguage};
use writer::{ManagedLogWriter, session_id};

pub(crate) const MESSAGE_LOG_TARGET: &str = "bkm::messages";
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub(crate) enum LoggingError {
    #[error("invalid console log filter: {0}")]
    ConsoleFilter(#[source] tracing_subscriber::filter::ParseError),
    #[error("invalid file log filter: {0}")]
    FileFilter(#[source] tracing_subscriber::filter::ParseError),
    #[error("failed to initialize log files: {0}")]
    File(#[from] io::Error),
    #[error("failed to install tracing subscriber: {0}")]
    Subscriber(String),
}

#[derive(Debug, Default)]
pub(crate) struct LoggingGuards {
    workers: Vec<WorkerGuard>,
    counters: Vec<ErrorCounter>,
}

impl Drop for LoggingGuards {
    fn drop(&mut self) {
        self.workers.clear();
        let dropped = self
            .counters
            .iter()
            .map(ErrorCounter::dropped_lines)
            .sum::<usize>();
        if dropped > 0 {
            eprintln!("logging dropped {dropped} lines because file writers could not keep up");
        }
    }
}

pub(crate) fn init(config: &LoggingConfig) -> Result<LoggingGuards, LoggingError> {
    let language = ConsoleLanguage::parse(&config.console.language);
    let ansi = config.console.ansi && std::io::stderr().is_terminal();
    let console_filter =
        EnvFilter::try_new(&config.console.filter).map_err(LoggingError::ConsoleFilter)?;
    let show_console_messages = config.console.message_content;
    let console_layer = config.console.enabled.then(|| {
        fmt::layer()
            .event_format(ConsoleFormat::new(language, ansi))
            .with_filter(console_filter)
            .with_filter(filter_fn(move |metadata| {
                show_console_messages || metadata.target() != MESSAGE_LOG_TARGET
            }))
    });

    let mut guards = LoggingGuards::default();
    let session = session_id();
    let file_filter = config
        .files
        .enabled
        .then(|| EnvFilter::try_new(&config.files.filter))
        .transpose()
        .map_err(LoggingError::FileFilter)?;
    let runtime_layer = if config.files.enabled {
        let writer = ManagedLogWriter::new(
            &config.files.directory,
            "runtime",
            mib_bytes(config.files.runtime_max_file_mb, "runtime file")?,
            mib_bytes(config.files.runtime_max_total_mb, "runtime total")?,
            config.files.zstd_level,
            &session,
        )?;
        let (writer, guard, counter) = non_blocking(writer, config.files.buffer_lines);
        guards.workers.push(guard);
        guards.counters.push(counter);
        Some(
            fmt::layer()
                .json()
                .flatten_event(true)
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(file_filter.clone().expect("file filter was validated"))
                .with_filter(filter_fn(|metadata| {
                    metadata.target() != MESSAGE_LOG_TARGET
                })),
        )
    } else {
        None
    };

    let message_layer = if config.files.enabled && config.files.message_content {
        let writer = ManagedLogWriter::new(
            &config.files.directory,
            "messages",
            mib_bytes(config.files.messages_max_file_mb, "messages file")?,
            mib_bytes(config.files.messages_max_total_mb, "messages total")?,
            config.files.zstd_level,
            &session,
        )?;
        let (writer, guard, counter) = non_blocking(writer, config.files.buffer_lines);
        guards.workers.push(guard);
        guards.counters.push(counter);
        Some(
            fmt::layer()
                .json()
                .flatten_event(true)
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(file_filter.expect("file filter was validated"))
                .with_filter(filter_fn(|metadata| {
                    metadata.target() == MESSAGE_LOG_TARGET
                })),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(console_layer)
        .with(runtime_layer)
        .with(message_layer)
        .try_init()
        .map_err(|error| LoggingError::Subscriber(error.to_string()))?;
    Ok(guards)
}

fn mib_bytes(value: u64, name: &str) -> Result<u64, LoggingError> {
    value.checked_mul(MIB).ok_or_else(|| {
        LoggingError::File(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} log capacity exceeds the supported byte limit"),
        ))
    })
}

fn non_blocking(
    writer: ManagedLogWriter,
    buffer_lines: usize,
) -> (
    tracing_appender::non_blocking::NonBlocking,
    WorkerGuard,
    ErrorCounter,
) {
    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(buffer_lines)
        .lossy(true)
        .finish(writer);
    let counter = writer.error_counter();
    (writer, guard, counter)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::SystemTime};

    use tracing_subscriber::{filter::filter_fn, fmt, layer::SubscriberExt as _};

    use super::*;

    #[test]
    fn structured_file_sinks_separate_runtime_and_message_content() {
        let directory = temporary_directory();
        let runtime = ManagedLogWriter::new(&directory, "runtime", MIB, MIB, 3, "test").unwrap();
        let messages = ManagedLogWriter::new(&directory, "messages", MIB, MIB, 3, "test").unwrap();
        let (runtime, runtime_guard, _) = non_blocking(runtime, 64);
        let (messages, messages_guard, _) = non_blocking(messages, 64);
        let runtime_layer = fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(runtime)
            .with_filter(filter_fn(|metadata| {
                metadata.target() != MESSAGE_LOG_TARGET
            }));
        let message_layer = fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(messages)
            .with_filter(filter_fn(|metadata| {
                metadata.target() == MESSAGE_LOG_TARGET
            }));
        let subscriber = tracing_subscriber::registry()
            .with(runtime_layer)
            .with(message_layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event_type = "READY", "runtime-event");
            tracing::info!(
                target: MESSAGE_LOG_TARGET,
                content = "hello",
                "message-event"
            );
        });
        drop(runtime_guard);
        drop(messages_guard);

        let runtime = read_log(&directory.join("runtime"));
        let messages = read_log(&directory.join("messages"));
        assert!(runtime.contains("runtime-event"));
        assert!(!runtime.contains("message-event"));
        assert!(messages.contains("message-event"));
        assert!(messages.contains("hello"));
        assert!(!messages.contains("runtime-event"));
        fs::remove_dir_all(directory).unwrap();
    }

    fn read_log(root: &std::path::Path) -> String {
        let month = fs::read_dir(root).unwrap().next().unwrap().unwrap().path();
        let file = fs::read_dir(month).unwrap().next().unwrap().unwrap().path();
        fs::read_to_string(file).unwrap()
    }

    fn temporary_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bkm-log-sinks-{}-{}",
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
