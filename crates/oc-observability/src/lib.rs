//! Tracing subscriber setup, log file rotation, and structured span conventions.
//!
//! # The one rule
//!
//! **No log byte ever reaches stdout.** The default sink is a rolling file; the
//! optional terminal sink is stderr. Nothing in this crate's library code writes to
//! stdout at all, and `tests/no_stdout_in_library.rs` scans the source to keep it
//! that way.
//!
//! The rule is not stylistic. Two interfaces this workspace has to provide frame
//! JSON on stdout — the Agent Client Protocol, and any future stdio protocol — and
//! there a single stray log byte is not noise, it is a parse error in the middle of
//! a message. The editor on the other end sees a corrupt frame and disconnects.
//! Because the failure looks like a mysterious disconnect rather than a log leak, it
//! is expensive to diagnose and easy to reintroduce, which is why the guarantee is
//! pinned by a test that captures a real child process's stdout as bytes
//! (`tests/stdout_purity.rs`) rather than by review.
//!
//! There is one trap worth naming: `tracing_subscriber::fmt::layer()` writes to
//! **stdout** by default. Every layer built here therefore sets `with_writer`
//! explicitly, and omitting it would silently break the guarantee.
//!
//! # Getting started
//!
//! ```no_run
//! # fn main() -> Result<(), oc_observability::LogInitError> {
//! use oc_observability::{LogConfig, LogLevel};
//!
//! // The directory is a parameter. See "The log directory" below.
//! let config = LogConfig::from_env("/tmp/opencode/log").with_level(LogLevel::Debug);
//!
//! // Hold the handle for as long as the process should keep logging.
//! let _logging = oc_observability::init(config)?;
//!
//! tracing::info!("this lands in the rolling log file, never on stdout");
//! # Ok(())
//! # }
//! ```
//!
//! # The returned handle is load-bearing
//!
//! [`init`] returns a [`LogHandle`] that owns the [`tracing_appender`] worker guard.
//! **Dropping it stops all file logging**, because the guard's `Drop` is what shuts
//! the background writer thread down and flushes it. A `main` that writes
//!
//! ```text
//! let _ = oc_observability::init(config)?;   // WRONG: dropped immediately
//! ```
//!
//! gets a process with no logs and no error to explain it. Bind it to a named local
//! that lives as long as the process:
//!
//! ```text
//! let _logging = oc_observability::init(config)?;   // right
//! ```
//!
//! # The log directory
//!
//! [`LogConfig::dir`] is a parameter rather than a call to `oc_paths::log()`,
//! because `oc-paths` does not expose `log()` yet. The value the CLI must pass is
//! the oracle's, `$XDG_DATA_HOME/opencode/log`
//! (`packages/core/src/global.ts:11,23`). Resolving XDG here instead would create a
//! second, drifting copy of a layout that has exactly one owner.
//!
//! # Anti-patterns this crate exists to avoid
//!
//! - **A hand-rolled logger with no spans.** `.omo/refs/jcode`'s own `AGENTS.md:36-39`
//!   documents the consequence: "`crate::logging::info` writes to a log file, not
//!   stderr, so instrumenting a code path with it produces no visible output under
//!   `--trace`." The fix is not a better hand-rolled logger, it is
//!   `tracing-subscriber`, whose stderr sink is one layer and whose spans attribute
//!   a deep event to its session and turn for free.
//! - **Behaviour that depends on `cfg!(test)`.**
//!   `.omo/refs/jcode/crates/jcode-app-core/src/agent/streaming.rs:6-13` picks a
//!   50 ms keep-alive under test and 30 s otherwise, so the timing its tests prove
//!   is timing that never ships. Nothing in this crate reads `cfg!(test)`;
//!   everything is decided by [`LogConfig`] and the two environment variables.
//! - **Silently dropped records.** The non-blocking writer is lossy by
//!   construction, so a slow disk drops lines rather than stalling a turn. That is
//!   the right trade for an agent, but an invisible one, so
//!   [`LogHandle::dropped_lines`] makes the count observable.

mod config;
mod error;
pub mod span;
pub mod tool;

pub use crate::config::{
    DEFAULT_MAX_LOG_FILES, ENV_LOG_LEVEL, ENV_PRINT_LOGS, LOG_FILE_PREFIX, LOG_FILE_SUFFIX,
    LogConfig, LogLevel, PRINT_LOGS_ENABLED, Rotation,
};
pub use crate::error::LogInitError;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// How many records may queue for the background writer before it starts dropping.
///
/// `tracing-appender`'s own default is 128_000. This is lower on purpose: a
/// backlog that large means the disk is far behind, and the useful record at that
/// point is the drop count, not another minute of stale history held in memory by a
/// process that is already under pressure.
const BUFFERED_LINES_LIMIT: usize = 8_192;

// Compile-time, not a test: the budget is a property of the constant, so the build
// should fail rather than a test suite reporting it after the fact.
const _: () = assert!(
    BUFFERED_LINES_LIMIT > 0 && BUFFERED_LINES_LIMIT < 128_000,
    "the buffer must hold something, and a backlog approaching tracing-appender's \
     128_000 default is memory held by a process already under pressure"
);

/// Set exactly once, by the [`init`] call that installs the global subscriber.
///
/// This is *not* an implicit initializer. Nothing reads or writes it at load time;
/// it is only touched inside [`init`], which a caller has to invoke deliberately.
/// Its whole job is to make a second [`init`] cheap and quiet rather than a panic or
/// a second background writer thread nothing writes to.
static INSTALLED: OnceLock<()> = OnceLock::new();

/// True once some [`init`] call has installed the global subscriber.
#[must_use]
pub fn is_initialized() -> bool {
    INSTALLED.get().is_some()
}

/// Installs the global tracing subscriber and returns the handle that keeps it alive.
///
/// # Idempotent
///
/// Calling this twice is not an error and does not panic. The second call installs
/// nothing, allocates no appender, and returns a handle whose
/// [`LogHandle::installed`] is `false`. Both the CLI and the test suite call it, and
/// a test binary that runs two tests in one process would otherwise abort.
///
/// # Sinks
///
/// - A rolling file in [`LogConfig::dir`], always.
/// - Additionally stderr, when `--print-logs` or `OPENCODE_PRINT_LOGS=1` asked for
///   it. This mirrors `packages/core/src/observability/logging.ts:67-69`, where
///   printing *adds* a stderr logger and never replaces the file one.
///
/// Never stdout, under any configuration.
///
/// # Errors
///
/// [`LogInitError::Directory`] if the log directory cannot be created,
/// [`LogInitError::Appender`] if no file can be opened in it, and
/// [`LogInitError::Directives`] if [`LogConfig::directives`] is not valid filter
/// syntax. All three are fatal: a process that cannot write diagnostics should say
/// so at startup rather than run blind.
pub fn init(config: LogConfig) -> Result<LogHandle, LogInitError> {
    let level = config.resolved_level();
    let print_logs = config.resolved_print_logs();

    // Fast path for the common repeated call: return before building anything, so a
    // second `init` costs no file handle and no worker thread.
    if is_initialized() {
        return Ok(LogHandle::not_installed(config.dir, level, print_logs));
    }

    // Both fallible steps run before any global state is touched, so a failure here
    // leaves the process exactly as it was and a later `init` can still succeed.
    let filter = config.build_filter()?;
    let appender = config.build_appender()?;

    let (writer, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(BUFFERED_LINES_LIMIT)
        // Lossy on purpose: the alternative is backpressure, which would let a slow
        // disk stall the async runtime mid-turn. `LogHandle::dropped_lines` keeps
        // the cost visible.
        .lossy(true)
        .thread_name("oc-log-writer")
        .finish(appender);
    let dropped = writer.error_counter();

    let span_events = if config.span_events {
        // Both edges. A span that opened and never closed is the signature of a
        // hang, and that only shows up if `NEW` is recorded too.
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };

    // `with_writer` is mandatory: `fmt::layer()` defaults to stdout, which is the
    // one destination this crate must never touch.
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        // A log file is not a terminal. ANSI escapes here would corrupt it for
        // every reader that is not a terminal emulator.
        .with_ansi(false)
        .with_target(true)
        .with_level(true)
        .with_span_events(span_events.clone());

    // Same rule: an explicit stderr writer, never the stdout default.
    let stderr_layer = print_logs.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .with_target(true)
            .with_level(true)
            .with_span_events(span_events.clone())
    });

    // Claim the install. `OnceLock::set` is atomic, so of two racing callers exactly
    // one proceeds and the loser tears its appender down instead of registering a
    // second subscriber.
    if INSTALLED.set(()).is_err() {
        drop(guard);
        return Ok(LogHandle::not_installed(config.dir, level, print_logs));
    }

    // `try_init` rather than `init`: a subscriber installed by something outside this
    // crate is a reason to step aside, not to abort the process.
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok();

    if !installed {
        drop(guard);
        return Ok(LogHandle::not_installed(config.dir, level, print_logs));
    }

    Ok(LogHandle {
        guard: Some(guard),
        dropped: Some(dropped),
        installed: true,
        dir: config.dir,
        level,
        print_logs,
    })
}

/// Keeps the logging subscriber alive. **Dropping this stops file logging.**
///
/// The handle owns [`tracing_appender`]'s `WorkerGuard`, whose `Drop` shuts down and
/// flushes the background writer thread. That makes the handle's lifetime the
/// process's logging lifetime: bind it to a named local in `main` and let it fall
/// out of scope at exit, which also flushes the last records on the way out.
#[derive(Debug)]
#[must_use = "dropping the LogHandle stops all file logging; bind it to a named local"]
pub struct LogHandle {
    /// `None` when this call did not install the subscriber, so there is no worker
    /// thread of ours to keep alive.
    guard: Option<WorkerGuard>,
    dropped: Option<ErrorCounter>,
    installed: bool,
    dir: PathBuf,
    level: LogLevel,
    print_logs: bool,
}

impl LogHandle {
    fn not_installed(dir: PathBuf, level: LogLevel, print_logs: bool) -> Self {
        Self {
            guard: None,
            dropped: None,
            installed: false,
            dir,
            level,
            print_logs,
        }
    }

    /// True when *this* call installed the subscriber.
    ///
    /// `false` means a subscriber was already in place, so this handle carries no
    /// worker guard and dropping it changes nothing.
    #[must_use]
    pub fn installed(&self) -> bool {
        self.installed
    }

    /// The directory rolling files are written to.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The level in force, after the flag-over-environment precedence was applied.
    #[must_use]
    pub fn level(&self) -> LogLevel {
        self.level
    }

    /// Whether the stderr sink is attached.
    #[must_use]
    pub fn print_logs(&self) -> bool {
        self.print_logs
    }

    /// Records dropped because the writer's queue was full, or `None` when this
    /// handle did not install the writer.
    ///
    /// A non-zero value means the log is incomplete. Worth surfacing rather than
    /// leaving as an unexplained gap in a file someone is trying to debug from.
    #[must_use]
    pub fn dropped_lines(&self) -> Option<usize> {
        self.dropped.as_ref().map(ErrorCounter::dropped_lines)
    }

    /// Hands the worker guard to a caller that wants to park it somewhere else.
    ///
    /// The same warning applies: whatever holds the returned guard decides when file
    /// logging stops.
    #[must_use]
    pub fn into_guard(mut self) -> Option<WorkerGuard> {
        self.guard.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The install marker must not be set by merely loading the crate. If it were,
    /// the first real `init` would take the not-installed path and the process would
    /// run with no logging at all.
    ///
    /// This also documents that initialization is explicit: no `lazy_static`, no
    /// `OnceCell` side effect at import time.
    #[test]
    fn loading_the_crate_installs_nothing() {
        // Only meaningful before any test in this binary calls `init`. The unit
        // tests in this module deliberately never do; the child-process tests in
        // `tests/` own that path, where a real process's stdout can be captured.
        assert!(!is_initialized());
    }

    #[test]
    fn a_handle_that_installed_nothing_reports_so_and_holds_no_guard() {
        let handle = LogHandle::not_installed(PathBuf::from("/tmp/x"), LogLevel::Warn, true);
        assert!(!handle.installed());
        assert_eq!(handle.dir(), Path::new("/tmp/x"));
        assert_eq!(handle.level(), LogLevel::Warn);
        assert!(handle.print_logs());
        assert_eq!(handle.dropped_lines(), None);
        assert!(handle.into_guard().is_none());
    }
}
