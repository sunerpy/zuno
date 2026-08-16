//! Failures from installing the logging subscriber.
//!
//! # Why this error is not in `zuno-error`
//!
//! `zuno-error` is the workspace taxonomy and every variant there carries the data a
//! *recovery decision* needs. Logging setup has no recovery: it happens once, at
//! process start, before anything is running that could retry. Folding it into
//! [`zuno_error::ConfigError::Io`] was considered and rejected — that variant renders
//! `"config file {path} could not be read"`, which is factually wrong for a log
//! directory that could not be created, and `zuno-error`'s own contract is that a
//! config error names the config file at fault.
//!
//! So this is a local, typed, `thiserror`-derived error. It implements
//! [`zuno_error::Recoverable`] so that a caller holding one can ask the same question
//! it asks of every other workspace error, and the answer is always
//! [`zuno_error::Recovery::Fail`].

use std::path::PathBuf;
use zuno_error::{Recoverable, Recovery};

/// A failure while installing the logging subscriber.
///
/// Every variant carries the directory or directive string it failed on, because a
/// logging failure that cannot say *where* it was trying to write is not
/// actionable — and the process is about to run with no diagnostics at all, which
/// is the worst possible moment to be vague.
#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    /// The log directory did not exist and could not be created.
    ///
    /// `std::io::ErrorKind` survives in the source, so a caller can distinguish a
    /// read-only filesystem from a permission problem.
    #[error("log directory {dir} could not be created")]
    Directory {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The directory exists but the rolling file appender could not open a file in
    /// it.
    #[error("rolling log file appender could not be opened in {dir}")]
    Appender {
        dir: PathBuf,
        #[source]
        source: tracing_appender::rolling::InitError,
    },

    /// Programmatic filter directives were syntactically invalid.
    ///
    /// This cannot be triggered by [`crate::LogLevel`], which is a closed enum. It
    /// only fires for a raw directive string passed through
    /// [`crate::LogConfig::directives`].
    #[error("log filter directives {directives:?} are not valid")]
    Directives {
        directives: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
}

impl LogInitError {
    /// The action this failure calls for, which is always to surface it.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }
}

impl Recoverable for LogInitError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Directory { .. } | Self::Appender { .. } | Self::Directives { .. } => {
                Recovery::Fail
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_failure_keeps_the_io_error_kind() {
        let e = LogInitError::Directory {
            dir: PathBuf::from("/nope/log"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let LogInitError::Directory { source, .. } = &e else {
            panic!("constructed a Directory, matched something else");
        };
        assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            e.to_string(),
            "log directory /nope/log could not be created"
        );
    }

    #[test]
    fn no_logging_failure_is_retryable() {
        let e = LogInitError::Directory {
            dir: PathBuf::from("a"),
            source: std::io::Error::other("x"),
        };
        assert_eq!(e.recovery(), Recovery::Fail);
        assert!(!e.recovery().is_retry());
    }
}
