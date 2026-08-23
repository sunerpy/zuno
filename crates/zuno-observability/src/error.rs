use std::path::PathBuf;

use zuno_error::{Recoverable, Recovery};

#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    #[error("log directory {dir} could not be prepared")]
    Directory {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("structured log database {path} could not be opened: {source}")]
    Database {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("plaintext log file {path} could not be opened")]
    Plaintext {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("log filter directives {directives:?} are not valid")]
    Directives {
        directives: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
}

impl LogInitError {
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }
}

impl Recoverable for LogInitError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Directory { .. }
            | Self::Database { .. }
            | Self::Plaintext { .. }
            | Self::Directives { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_logging_failure_is_retryable() {
        let error = LogInitError::Directory {
            dir: PathBuf::from("/nope/log"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        assert_eq!(error.recovery(), Recovery::Fail);
        assert!(!error.recovery().is_retry());
        assert!(error.to_string().contains("/nope/log"));
    }
}
