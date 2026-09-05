//! Explicit operational-log configuration.
//!
//! Zuno keeps a small convenience level (`--log-level` / `ZUNO_LOG_LEVEL`) and
//! also accepts the Rust ecosystem's target-aware `RUST_LOG`. The structured
//! SQLite sink is always present. Plaintext files are opt-in because they are easy
//! to copy, index, or leak accidentally; when enabled, every process owns a
//! separate `0600` file.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::level_filters::LevelFilter;

use crate::store::{
    DEFAULT_MAX_AGE_DAYS, DEFAULT_MAX_BYTES, DEFAULT_MAX_RECORDS, ProcessIdentity, RetentionPolicy,
    STRUCTURED_LOG_FILE,
};

pub const ENV_LOG_LEVEL: &str = "ZUNO_LOG_LEVEL";
pub const ENV_PRINT_LOGS: &str = "ZUNO_PRINT_LOGS";
pub const ENV_PLAINTEXT_LOGS: &str = "ZUNO_PLAINTEXT_LOGS";
pub const ENV_RUST_LOG: &str = "RUST_LOG";
pub const ENABLED: &str = "1";
pub const LOG_FILE_PREFIX: &str = "zuno";
pub const LOG_FILE_SUFFIX: &str = "log";

/// AWS SDK targets whose INFO/DEBUG records may contain credential identifiers
/// or request-signing internals.
///
/// This is a security floor, not a noise preference. User `RUST_LOG` directives
/// cannot lower these targets below WARN.
const SENSITIVE_AWS_LOG_TARGETS: &[&str] = &[
    "aws_config",
    "aws_credential_types",
    "aws_runtime",
    "aws_sdk_signin",
    "aws_sdk_sso",
    "aws_sdk_ssooidc",
    "aws_sdk_sts",
    "aws_sigv4",
    "aws_smithy_http_client",
    "aws_smithy_runtime",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "TRACE" => Some(Self::Trace),
            "DEBUG" => Some(Self::Debug),
            "INFO" => Some(Self::Info),
            "WARN" => Some(Self::Warn),
            "ERROR" => Some(Self::Error),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    #[must_use]
    pub const fn level_filter(self) -> LevelFilter {
        match self {
            Self::Trace => LevelFilter::TRACE,
            Self::Debug => LevelFilter::DEBUG,
            Self::Info => LevelFilter::INFO,
            Self::Warn => LevelFilter::WARN,
            Self::Error => LevelFilter::ERROR,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub dir: PathBuf,
    pub level: Option<LogLevel>,
    pub print_logs: Option<bool>,
    pub plaintext_logs: Option<bool>,
    pub directives: Option<String>,
    pub span_events: bool,
    pub max_records: usize,
    pub max_bytes: usize,
    pub max_age: Duration,
}

impl LogConfig {
    #[must_use]
    pub fn from_env(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            level: None,
            print_logs: None,
            plaintext_logs: None,
            directives: None,
            span_events: false,
            max_records: DEFAULT_MAX_RECORDS,
            max_bytes: DEFAULT_MAX_BYTES,
            max_age: Duration::from_secs(DEFAULT_MAX_AGE_DAYS * 24 * 60 * 60),
        }
    }

    #[must_use]
    pub fn with_level(mut self, level: LogLevel) -> Self {
        self.level = Some(level);
        self
    }

    #[must_use]
    pub fn with_print_logs(mut self, print_logs: bool) -> Self {
        self.print_logs = Some(print_logs);
        self
    }

    #[must_use]
    pub fn with_plaintext_logs(mut self, plaintext_logs: bool) -> Self {
        self.plaintext_logs = Some(plaintext_logs);
        self
    }

    #[must_use]
    pub fn with_directives(mut self, directives: impl Into<String>) -> Self {
        self.directives = Some(directives.into());
        self
    }

    #[must_use]
    pub fn with_span_events(mut self, span_events: bool) -> Self {
        self.span_events = span_events;
        self
    }

    #[must_use]
    pub fn with_retention(
        mut self,
        max_records: usize,
        max_bytes: usize,
        max_age: Duration,
    ) -> Self {
        self.max_records = max_records.max(1);
        self.max_bytes = max_bytes.max(1);
        self.max_age = max_age.max(Duration::from_secs(1));
        self
    }

    #[must_use]
    pub fn resolved_level(&self) -> LogLevel {
        self.level.unwrap_or_else(|| {
            std::env::var(ENV_LOG_LEVEL)
                .ok()
                .and_then(|value| LogLevel::parse(&value))
                .unwrap_or_default()
        })
    }

    #[must_use]
    pub fn resolved_print_logs(&self) -> bool {
        self.print_logs.unwrap_or_else(|| enabled(ENV_PRINT_LOGS))
    }

    #[must_use]
    pub fn resolved_plaintext_logs(&self) -> bool {
        self.plaintext_logs
            .unwrap_or_else(|| enabled(ENV_PLAINTEXT_LOGS))
    }

    pub(crate) fn prepare_directory(&self) -> Result<(), crate::LogInitError> {
        std::fs::create_dir_all(&self.dir).map_err(|source| crate::LogInitError::Directory {
            dir: self.dir.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |source| crate::LogInitError::Directory {
                    dir: self.dir.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }

    pub(crate) fn build_filter(
        &self,
    ) -> Result<tracing_subscriber::EnvFilter, crate::LogInitError> {
        let builder = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(self.resolved_level().level_filter().into());
        let has_simple_level_override = self.level.is_some()
            || std::env::var(ENV_LOG_LEVEL)
                .ok()
                .and_then(|value| LogLevel::parse(&value))
                .is_some();
        let directives = self.directives.clone().or_else(|| {
            (!has_simple_level_override)
                .then(|| std::env::var(ENV_RUST_LOG).ok())
                .flatten()
                .filter(|value| !value.trim().is_empty())
        });
        let mut filter = match directives {
            Some(directives) => builder
                .parse(&directives)
                .map_err(|source| crate::LogInitError::Directives { directives, source }),
            None => Ok(builder.parse_lossy("")),
        }?;
        for target in SENSITIVE_AWS_LOG_TARGETS {
            let directive = format!("{target}=warn")
                .parse()
                .expect("the static AWS log directive is valid");
            filter = filter.add_directive(directive);
        }
        Ok(filter)
    }

    pub(crate) fn retention(&self) -> RetentionPolicy {
        RetentionPolicy {
            max_records: self.max_records,
            max_bytes: self.max_bytes,
            max_age: self.max_age,
        }
    }

    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.dir.join(STRUCTURED_LOG_FILE)
    }

    pub(crate) fn open_plaintext(
        &self,
        identity: &ProcessIdentity,
    ) -> Result<(PathBuf, File), crate::LogInitError> {
        let path = self.dir.join(format!(
            "{LOG_FILE_PREFIX}.{}.{}.{}",
            identity.pid, identity.uuid, LOG_FILE_SUFFIX
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|source| crate::LogInitError::Plaintext {
                path: path.clone(),
                source,
            })?;
        Ok((path, file))
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| value == ENABLED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_levels_include_trace() {
        for level in [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            assert_eq!(LogLevel::parse(level.as_str()), Some(level));
            assert_eq!(
                LogLevel::parse(&level.as_str().to_ascii_lowercase()),
                Some(level)
            );
        }
    }

    #[test]
    fn info_is_the_default() {
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn explicit_level_and_sink_overrides_are_typed() {
        let config = LogConfig::from_env("/tmp/zuno-observability-unit")
            .with_level(LogLevel::Trace)
            .with_print_logs(true)
            .with_plaintext_logs(true);
        assert_eq!(config.resolved_level(), LogLevel::Trace);
        assert!(config.resolved_print_logs());
        assert!(config.resolved_plaintext_logs());
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
    fn aws_sdk_credential_targets_are_forced_to_warn_even_under_trace() {
        let filter = LogConfig::from_env("/tmp/zuno-observability-unit")
            .with_directives("trace,aws_config=trace,aws_sigv4=trace")
            .build_filter()
            .expect("filter");
        let rendered = format!("{filter:?}");
        for target in SENSITIVE_AWS_LOG_TARGETS {
            assert!(
                rendered.contains(&format!(
                    "target: Some(\"{target}\"), field_names: [], level: LevelFilter::WARN"
                )),
                "AWS credential target `{target}` lost its WARN floor: {rendered}"
            );
        }
        assert!(
            !rendered.contains(
                "target: Some(\"aws_config\"), field_names: [], level: LevelFilter::TRACE"
            ) && !rendered.contains(
                "target: Some(\"aws_sigv4\"), field_names: [], level: LevelFilter::TRACE"
            ),
            "user directives overrode the AWS credential floor: {rendered}"
        );
    }

    #[test]
    fn structured_store_path_is_stable() {
        let config = LogConfig::from_env("/tmp/zuno-observability-unit");
        assert_eq!(
            config.database_path(),
            Path::new("/tmp/zuno-observability-unit").join(STRUCTURED_LOG_FILE)
        );
    }
}
