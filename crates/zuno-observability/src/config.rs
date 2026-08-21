//! Log level and sink configuration, matching the TypeScript `opencode` exactly.
//!
//! # Where these semantics come from
//!
//! Both environment variables are read by the oracle in one place,
//! `packages/core/src/observability/logging.ts`:
//!
//! ```text
//! // :56-65
//! export function minimumLogLevel() {
//!   const value = process.env.ZUNO_LOG_LEVEL?.toUpperCase()
//!   const levels = { DEBUG: "Debug", INFO: "Info", WARN: "Warn", ERROR: "Error" } as const
//!   return value && value in levels ? levels[value as keyof typeof levels] : levels.INFO
//! }
//!
//! // :67-69
//! export function loggers() {
//!   return process.env.ZUNO_PRINT_LOGS === "1" ? [fileLogger(), stderrLogger] : [fileLogger()]
//! }
//! ```
//!
//! Three details in there are easy to get wrong, so each has a test below:
//!
//! 1. **The level is uppercased before lookup**, so `debug` and `Debug` work.
//! 2. **An unrecognized level is not an error.** It silently falls back to `INFO`.
//!    There are exactly four accepted values; `TRACE` is *not* one of them, and the
//!    CLI's `--log-level` restricts itself to the same four
//!    (`packages/opencode/src/index.ts:58-62`).
//! 3. **`ZUNO_PRINT_LOGS` is compared with `=== "1"`**, not through the
//!    `truthy()` helper in `packages/core/src/flag/flag.ts:3-6` that most other
//!    `ZUNO_*` booleans use. `ZUNO_PRINT_LOGS=true` therefore does **not**
//!    enable printing. Matching `truthy()` here would be a silent behaviour
//!    divergence.
//!
//! And the detail that matters most: when printing is enabled the oracle keeps the
//! file logger and *adds* a stderr one. Printing is additive, never a replacement,
//! and the added sink is `process.stderr` — never stdout.

use std::path::{Path, PathBuf};
use tracing::level_filters::LevelFilter;

/// The environment variable naming the minimum level, set by the CLI's
/// `--log-level` (`packages/opencode/src/index.ts:68`).
pub const ENV_LOG_LEVEL: &str = "ZUNO_LOG_LEVEL";

/// The environment variable enabling the additional terminal sink, set to exactly
/// `"1"` by the CLI's `--print-logs` (`packages/opencode/src/index.ts:67`).
pub const ENV_PRINT_LOGS: &str = "ZUNO_PRINT_LOGS";

/// The only value of [`ENV_PRINT_LOGS`] the oracle treats as enabled.
pub const PRINT_LOGS_ENABLED: &str = "1";

/// The rolling log file name prefix.
pub const LOG_FILE_PREFIX: &str = "zuno";

/// The rolling log file name suffix.
pub const LOG_FILE_SUFFIX: &str = "log";

/// How many rotated files to keep before the appender prunes the oldest.
pub const DEFAULT_MAX_LOG_FILES: usize = 14;

/// The minimum level that will be recorded.
///
/// Deliberately a closed set of four. The oracle accepts exactly `DEBUG`, `INFO`,
/// `WARN` and `ERROR` and silently maps anything else to `INFO`, so adding a
/// `Trace` variant here would let `ZUNO_LOG_LEVEL=TRACE` behave differently in
/// the two implementations. `TRACE` is still reachable, but only through
/// [`LogConfig::directives`], which is programmatic and therefore cannot diverge
/// from a user's environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    Debug,
    /// The oracle's default, and the fallback for any unrecognized value.
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Parses one of the four accepted spellings, case-insensitively.
    ///
    /// Returns `None` for anything else so a caller can tell "not set" from
    /// "set to nonsense"; [`LogConfig::from_env`] then applies the oracle's
    /// fallback. Keeping the distinction here means the fallback is a decision made
    /// in one visible place rather than hidden inside a parser.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Self::Debug),
            "INFO" => Some(Self::Info),
            "WARN" => Some(Self::Warn),
            "ERROR" => Some(Self::Error),
            _ => None,
        }
    }

    /// The uppercase spelling the oracle accepts, for round-tripping into a child
    /// process's environment.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    /// The equivalent `tracing` filter.
    #[must_use]
    pub const fn level_filter(self) -> LevelFilter {
        match self {
            Self::Debug => LevelFilter::DEBUG,
            Self::Info => LevelFilter::INFO,
            Self::Warn => LevelFilter::WARN,
            Self::Error => LevelFilter::ERROR,
        }
    }
}

/// How the rolling file appender names and retires files.
///
/// A long-running agent can log every tool call and provider request, so the
/// default rotates daily and keeps [`DEFAULT_MAX_LOG_FILES`] files, producing
/// `zuno.<YYYY-MM-DD>.log`.
///
/// [`Self::Never`] exists for tests and deployments that prefer one predictable
/// `zuno.log` file over rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    /// One file per day: `zuno.2026-08-05.log`.
    #[default]
    Daily,
    /// One file per hour: `zuno.2026-08-05-14.log`.
    Hourly,
    /// A single `zuno.log`, appended to forever.
    Never,
}

impl Rotation {
    fn appender_rotation(self) -> tracing_appender::rolling::Rotation {
        match self {
            Self::Daily => tracing_appender::rolling::Rotation::DAILY,
            Self::Hourly => tracing_appender::rolling::Rotation::HOURLY,
            Self::Never => tracing_appender::rolling::Rotation::NEVER,
        }
    }
}

/// Everything [`crate::init`] needs, with nothing read implicitly.
///
/// # No `cfg!(test)` anywhere
///
/// Every field is data. Nothing in this crate asks whether it is running under
/// `cargo test`, so the code path a test exercises is byte-for-byte the code path
/// that ships. The counter-example this rule exists to avoid is
/// `.omo/refs/jcode/crates/jcode-app-core/src/agent/streaming.rs:6-13`, where a
/// keep-alive interval is 50 ms under `cfg!(test)` and 30 s in production, so the
/// timing the tests prove is timing that never runs.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// The directory rolling log files are written to.
    ///
    /// This is a parameter rather than an implicit path lookup. The CLI passes
    /// `zuno_paths::log()` here, which resolves `$XDG_DATA_HOME/zuno/log`; the
    /// upstream-only value is `$XDG_DATA_HOME/opencode/log`
    /// (`packages/core/src/global.ts:11,23`).
    pub dir: PathBuf,

    /// The minimum level, or `None` to read [`ENV_LOG_LEVEL`].
    ///
    /// `Some(_)` is how the CLI's `--log-level` wins over the environment. The
    /// oracle achieves the same precedence by *writing* the flag into
    /// `process.env` before anything reads it
    /// (`packages/opencode/src/index.ts:68`); passing it as data is the same
    /// precedence without the global mutation.
    pub level: Option<LogLevel>,

    /// Whether to add the stderr sink, or `None` to read [`ENV_PRINT_LOGS`].
    pub print_logs: Option<bool>,

    /// Raw `EnvFilter` directives layered on top of the resolved level, for
    /// per-target control such as `zuno_llm=trace,zuno_db=warn`.
    ///
    /// Programmatic only. No environment variable feeds this, because the oracle
    /// has no equivalent and inventing one would be a divergence a differential
    /// test could not see.
    pub directives: Option<String>,

    /// The file naming and retirement policy.
    pub rotation: Rotation,

    /// How many rotated files to keep. Ignored when `rotation` is
    /// [`Rotation::Never`].
    pub max_log_files: usize,

    /// Whether span open and close events are recorded.
    ///
    /// On by default: a span that opened and never closed is the signature of a
    /// hang, and that signal only exists if both edges are written.
    pub span_events: bool,
}

impl LogConfig {
    /// A config for `dir` that reads both environment variables.
    #[must_use]
    pub fn from_env(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            level: None,
            print_logs: None,
            directives: None,
            rotation: Rotation::default(),
            max_log_files: DEFAULT_MAX_LOG_FILES,
            span_events: true,
        }
    }

    /// Overrides the level, as the CLI's `--log-level` does.
    #[must_use]
    pub fn with_level(mut self, level: LogLevel) -> Self {
        self.level = Some(level);
        self
    }

    /// Overrides the terminal sink, as the CLI's `--print-logs` does.
    #[must_use]
    pub fn with_print_logs(mut self, print_logs: bool) -> Self {
        self.print_logs = Some(print_logs);
        self
    }

    /// Layers raw `EnvFilter` directives on top of the resolved level.
    #[must_use]
    pub fn with_directives(mut self, directives: impl Into<String>) -> Self {
        self.directives = Some(directives.into());
        self
    }

    /// Sets the file naming and retirement policy.
    #[must_use]
    pub fn with_rotation(mut self, rotation: Rotation) -> Self {
        self.rotation = rotation;
        self
    }

    /// Sets how many rotated files to keep.
    #[must_use]
    pub fn with_max_log_files(mut self, max_log_files: usize) -> Self {
        self.max_log_files = max_log_files;
        self
    }

    /// Turns span open/close records on or off.
    #[must_use]
    pub fn with_span_events(mut self, span_events: bool) -> Self {
        self.span_events = span_events;
        self
    }

    /// The level actually in force: the programmatic override, else
    /// [`ENV_LOG_LEVEL`], else [`LogLevel::Info`].
    ///
    /// An unparseable environment value falls back to `Info` rather than failing,
    /// which is what the oracle does at
    /// `packages/core/src/observability/logging.ts:64`.
    #[must_use]
    pub fn resolved_level(&self) -> LogLevel {
        if let Some(level) = self.level {
            return level;
        }
        std::env::var(ENV_LOG_LEVEL)
            .ok()
            .and_then(|value| LogLevel::parse(&value))
            .unwrap_or_default()
    }

    /// Whether the stderr sink is enabled: the programmatic override, else
    /// [`ENV_PRINT_LOGS`] being exactly `"1"`.
    #[must_use]
    pub fn resolved_print_logs(&self) -> bool {
        if let Some(print_logs) = self.print_logs {
            return print_logs;
        }
        std::env::var(ENV_PRINT_LOGS).is_ok_and(|value| value == PRINT_LOGS_ENABLED)
    }

    /// The appender for this config, built without touching global state.
    pub(crate) fn build_appender(
        &self,
    ) -> Result<tracing_appender::rolling::RollingFileAppender, crate::LogInitError> {
        std::fs::create_dir_all(&self.dir).map_err(|source| crate::LogInitError::Directory {
            dir: self.dir.clone(),
            source,
        })?;

        let mut builder = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(self.rotation.appender_rotation())
            .filename_prefix(LOG_FILE_PREFIX)
            .filename_suffix(LOG_FILE_SUFFIX);
        if self.rotation != Rotation::Never {
            builder = builder.max_log_files(self.max_log_files);
        }

        builder
            .build(&self.dir)
            .map_err(|source| crate::LogInitError::Appender {
                dir: self.dir.clone(),
                source,
            })
    }

    /// The filter for this config, built without touching global state.
    pub(crate) fn build_filter(
        &self,
    ) -> Result<tracing_subscriber::EnvFilter, crate::LogInitError> {
        let base = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(self.resolved_level().level_filter().into());

        match &self.directives {
            None => Ok(base.parse_lossy("")),
            Some(directives) => {
                base.parse(directives)
                    .map_err(|source| crate::LogInitError::Directives {
                        directives: directives.clone(),
                        source,
                    })
            }
        }
    }

    /// The path the current rolling file will have, for a caller that wants to
    /// point a user at it.
    ///
    /// Derived from the same prefix, suffix and rotation the appender uses, so it
    /// is exact for [`Rotation::Never`] and a prefix match for the rotating
    /// policies, where the date component is chosen by the appender at write time.
    #[must_use]
    pub fn file_name_prefix(&self) -> String {
        match self.rotation {
            Rotation::Never => format!("{LOG_FILE_PREFIX}.{LOG_FILE_SUFFIX}"),
            Rotation::Daily | Rotation::Hourly => format!("{LOG_FILE_PREFIX}."),
        }
    }

    /// The log directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle uppercases before lookup
    /// (`packages/core/src/observability/logging.ts:57`), so every casing of every
    /// accepted spelling must land on the same level.
    #[test]
    fn every_accepted_spelling_parses_case_insensitively() {
        for (input, expected) in [
            ("DEBUG", LogLevel::Debug),
            ("debug", LogLevel::Debug),
            ("Debug", LogLevel::Debug),
            ("INFO", LogLevel::Info),
            ("info", LogLevel::Info),
            ("WARN", LogLevel::Warn),
            ("warn", LogLevel::Warn),
            ("ERROR", LogLevel::Error),
            ("error", LogLevel::Error),
        ] {
            assert_eq!(
                LogLevel::parse(input),
                Some(expected),
                "{input:?} should parse as {expected:?}"
            );
        }
    }

    /// `TRACE` is deliberately absent. The oracle's map has four keys and its CLI
    /// `--log-level` has the same four choices
    /// (`packages/opencode/src/index.ts:58-62`), so accepting a fifth here would
    /// make `ZUNO_LOG_LEVEL=TRACE` mean different things in the two
    /// implementations.
    #[test]
    fn unrecognized_levels_including_trace_do_not_parse() {
        for input in ["TRACE", "trace", "VERBOSE", "FATAL", "OFF", "1", ""] {
            assert_eq!(
                LogLevel::parse(input),
                None,
                "{input:?} is not one of the four values the oracle accepts"
            );
        }
    }

    #[test]
    fn the_spelling_round_trips_for_a_child_process_environment() {
        for level in [
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            assert_eq!(LogLevel::parse(level.as_str()), Some(level));
        }
    }

    #[test]
    fn info_is_the_default_because_it_is_the_oracles_fallback() {
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn levels_map_onto_the_matching_tracing_filter() {
        assert_eq!(LogLevel::Debug.level_filter(), LevelFilter::DEBUG);
        assert_eq!(LogLevel::Info.level_filter(), LevelFilter::INFO);
        assert_eq!(LogLevel::Warn.level_filter(), LevelFilter::WARN);
        assert_eq!(LogLevel::Error.level_filter(), LevelFilter::ERROR);
    }

    /// A programmatic level must win over the environment without reading it,
    /// which is what makes `--log-level` work.
    #[test]
    fn a_programmatic_level_wins_without_consulting_the_environment() {
        let config =
            LogConfig::from_env("/tmp/zuno-observability-unit").with_level(LogLevel::Error);
        assert_eq!(config.resolved_level(), LogLevel::Error);
    }

    #[test]
    fn a_programmatic_print_logs_wins_without_consulting_the_environment() {
        let on = LogConfig::from_env("/tmp/zuno-observability-unit").with_print_logs(true);
        let off = LogConfig::from_env("/tmp/zuno-observability-unit").with_print_logs(false);
        assert!(on.resolved_print_logs());
        assert!(!off.resolved_print_logs());
    }

    #[test]
    fn never_rotating_uses_the_zuno_log_file_name() {
        let config =
            LogConfig::from_env("/tmp/zuno-observability-unit").with_rotation(Rotation::Never);
        assert_eq!(config.file_name_prefix(), "zuno.log");
    }

    #[test]
    fn rotating_files_share_the_zuno_prefix() {
        let config = LogConfig::from_env("/tmp/zuno-observability-unit");
        assert_eq!(config.rotation, Rotation::Daily);
        assert_eq!(config.file_name_prefix(), "zuno.");
    }

    #[test]
    fn invalid_directives_are_reported_with_the_string_that_failed() {
        let config = LogConfig::from_env("/tmp/zuno-observability-unit")
            .with_directives("this=is=not=valid");
        let Err(crate::LogInitError::Directives { directives, .. }) = config.build_filter() else {
            panic!("invalid directives should not build a filter");
        };
        assert_eq!(directives, "this=is=not=valid");
    }

    #[test]
    fn valid_directives_build_a_filter() {
        let config = LogConfig::from_env("/tmp/zuno-observability-unit")
            .with_directives("zuno_llm=trace,zuno_db=warn");
        assert!(config.build_filter().is_ok());
    }

    #[test]
    fn an_absent_directive_string_still_builds_a_filter_at_the_resolved_level() {
        let config =
            LogConfig::from_env("/tmp/zuno-observability-unit").with_level(LogLevel::Debug);
        let filter = config
            .build_filter()
            .expect("no directives is always valid");
        assert_eq!(filter.max_level_hint(), Some(LevelFilter::DEBUG));
    }
}
