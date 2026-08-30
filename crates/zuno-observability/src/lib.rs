//! Zuno operational logging.
//!
//! Model-visible prompts, inputs, and tool results belong in durable session
//! events. This crate records operational metadata instead: process lifecycle,
//! turn/provider/tool correlation, failures, timing, and resource diagnostics.
//! The default sink is a bounded SQLite database shared safely by concurrent
//! processes. Logs never write to stdout.

mod config;
mod error;
pub mod frame;
pub mod memory;
pub mod span;
mod store;
pub mod tool;
pub mod watchdog;

pub use crate::config::{
    ENABLED, ENV_LOG_LEVEL, ENV_PLAINTEXT_LOGS, ENV_PRINT_LOGS, ENV_RUST_LOG, LOG_FILE_PREFIX,
    LOG_FILE_SUFFIX, LogConfig, LogLevel,
};
pub use crate::error::LogInitError;
pub use crate::store::{
    DEFAULT_MAX_AGE_DAYS, DEFAULT_MAX_BYTES, DEFAULT_MAX_RECORDS, STRUCTURED_LOG_FILE,
};

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

const PLAINTEXT_QUEUE_CAPACITY: usize = 8_192;

static INIT_LOCK: Mutex<()> = Mutex::new(());
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[must_use]
pub fn is_initialized() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

pub fn init(config: LogConfig) -> Result<LogHandle, LogInitError> {
    let _init = INIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let level = config.resolved_level();
    let print_logs = config.resolved_print_logs();
    let plaintext_logs = config.resolved_plaintext_logs();
    let database_path = config.database_path();

    if is_initialized() {
        return Ok(LogHandle::not_installed(
            config.dir,
            database_path,
            level,
            print_logs,
            plaintext_logs,
        ));
    }

    config.prepare_directory()?;
    let filter = config.build_filter()?;
    let identity = store::ProcessIdentity::new();
    let store = store::start(database_path.clone(), identity.clone(), config.retention()).map_err(
        |source| LogInitError::Database {
            path: database_path.clone(),
            source,
        },
    )?;

    let span_events = if config.span_events {
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };

    let (file_layer, file_guard, file_dropped, plaintext_path) = if plaintext_logs {
        let (path, file) = config.open_plaintext(&identity)?;
        let (writer, guard) = NonBlockingBuilder::default()
            .buffered_lines_limit(PLAINTEXT_QUEUE_CAPACITY)
            .lossy(true)
            .thread_name("zuno-plaintext-log")
            .finish(file);
        let dropped = writer.error_counter();
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_span_events(span_events.clone());
        (Some(layer), Some(guard), Some(dropped), Some(path))
    } else {
        (None, None, None, None)
    };

    let stderr_layer = print_logs.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .with_target(true)
            .with_level(true)
            .with_span_events(span_events)
    });

    let store::StoreRuntime {
        layer: store_layer,
        guard: store_guard,
        dropped: store_dropped,
        failures: store_failures,
    } = store;
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(store_layer)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();
    if !installed {
        drop(file_guard);
        drop(store_guard);
        return Ok(LogHandle::not_installed(
            config.dir,
            database_path,
            level,
            print_logs,
            plaintext_logs,
        ));
    }
    INSTALLED.store(true, Ordering::Release);

    let handle = LogHandle {
        store_guard: Some(store_guard),
        file_guard,
        store_dropped: Some(store_dropped),
        store_failures: Some(store_failures),
        file_dropped,
        installed: true,
        dir: config.dir,
        database_path,
        plaintext_path,
        process_uuid: Some(identity.uuid.clone()),
        process_id: Some(identity.pid),
        level,
        print_logs,
        plaintext_logs,
    };
    tracing::info!(
        target: "zuno_observability",
        event = "process.started",
        process_uuid = %identity.uuid,
        pid = identity.pid,
        version = env!("CARGO_PKG_VERSION"),
        "zuno process logging started"
    );
    Ok(handle)
}

#[derive(Debug)]
#[must_use = "keep the LogHandle alive for the lifetime of the process"]
pub struct LogHandle {
    store_guard: Option<store::StoreGuard>,
    file_guard: Option<WorkerGuard>,
    store_dropped: Option<Arc<AtomicUsize>>,
    store_failures: Option<Arc<AtomicUsize>>,
    file_dropped: Option<ErrorCounter>,
    installed: bool,
    dir: PathBuf,
    database_path: PathBuf,
    plaintext_path: Option<PathBuf>,
    process_uuid: Option<String>,
    process_id: Option<u32>,
    level: LogLevel,
    print_logs: bool,
    plaintext_logs: bool,
}

impl LogHandle {
    fn not_installed(
        dir: PathBuf,
        database_path: PathBuf,
        level: LogLevel,
        print_logs: bool,
        plaintext_logs: bool,
    ) -> Self {
        Self {
            store_guard: None,
            file_guard: None,
            store_dropped: None,
            store_failures: None,
            file_dropped: None,
            installed: false,
            dir,
            database_path,
            plaintext_path: None,
            process_uuid: None,
            process_id: None,
            level,
            print_logs,
            plaintext_logs,
        }
    }

    #[must_use]
    pub fn installed(&self) -> bool {
        self.installed
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    #[must_use]
    pub fn plaintext_path(&self) -> Option<&Path> {
        self.plaintext_path.as_deref()
    }

    #[must_use]
    pub fn process_uuid(&self) -> Option<&str> {
        self.process_uuid.as_deref()
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    #[must_use]
    pub fn level(&self) -> LogLevel {
        self.level
    }

    #[must_use]
    pub fn print_logs(&self) -> bool {
        self.print_logs
    }

    #[must_use]
    pub fn plaintext_logs(&self) -> bool {
        self.plaintext_logs
    }

    #[must_use]
    pub fn dropped_lines(&self) -> Option<usize> {
        self.installed.then(|| {
            let structured = self
                .store_dropped
                .as_ref()
                .map_or(0, |dropped| dropped.load(Ordering::Relaxed));
            let plaintext = self
                .file_dropped
                .as_ref()
                .map_or(0, ErrorCounter::dropped_lines);
            structured.saturating_add(plaintext)
        })
    }

    #[must_use]
    pub fn write_failures(&self) -> Option<usize> {
        self.store_failures
            .as_ref()
            .map(|failures| failures.load(Ordering::Relaxed))
    }
}

impl Drop for LogHandle {
    fn drop(&mut self) {
        if !self.installed {
            return;
        }
        tracing::info!(
            target: "zuno_observability",
            event = "process.stopping",
            dropped_records = self.dropped_lines().unwrap_or_default(),
            write_failures = self.write_failures().unwrap_or_default(),
            "zuno process logging stopping"
        );
        drop(self.file_guard.take());
        drop(self.store_guard.take());

        let dropped = self.dropped_lines().unwrap_or_default();
        let failures = self.write_failures().unwrap_or_default();
        if dropped > 0 || failures > 0 {
            eprintln!(
                "zuno logging incomplete: dropped_records={dropped} write_failures={failures}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_the_crate_installs_nothing() {
        assert!(!is_initialized());
    }

    #[test]
    fn a_non_installing_handle_reports_no_runtime_identity() {
        let handle = LogHandle::not_installed(
            PathBuf::from("/tmp/x"),
            PathBuf::from("/tmp/x/logs.sqlite"),
            LogLevel::Warn,
            true,
            false,
        );
        assert!(!handle.installed());
        assert_eq!(handle.process_uuid(), None);
        assert_eq!(handle.process_id(), None);
        assert_eq!(handle.dropped_lines(), None);
        assert_eq!(handle.write_failures(), None);
    }
}
