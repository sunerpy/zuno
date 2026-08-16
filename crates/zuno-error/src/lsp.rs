//! Failures talking to a language server.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::time::Duration;

/// A failure talking to a language server.
///
/// [`LspError::NotInstalled`] is separate from [`LspError::Spawn`] on purpose.
/// Both surface as a spawn failure at the OS level, but only one of them has a fix
/// worth offering the user, and telling them apart by matching
/// `std::io::ErrorKind::NotFound` against a rendered message is precisely the
/// pattern this crate exists to remove.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// The server binary is not on `PATH`. `command` is what was looked for, so
    /// the reporter can name the package to install.
    #[error("language server {server} is not installed (looked for {command})")]
    NotInstalled { server: String, command: String },

    /// The server binary exists but could not be started.
    #[error("language server {server} could not be started with {command}")]
    Spawn {
        server: String,
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// The process started but the initialize handshake failed.
    #[error("language server {server} failed to initialize")]
    Initialize {
        server: String,
        #[source]
        source: BoxSource,
    },

    /// A message could not be decoded. Framing bugs live here.
    #[error("language server {server} sent a message that could not be decoded")]
    Protocol {
        server: String,
        #[source]
        source: serde_json::Error,
    },

    /// The server did not answer in time. Retryable.
    #[error("language server {server} did not respond within {elapsed:?}")]
    Timeout { server: String, elapsed: Duration },

    /// The process exited. Retryable: language servers crash routinely and are
    /// expected to be restarted. `code` is carried because a clean exit and a
    /// signal death are different diagnoses.
    #[error("language server {server} exited (code={code:?})")]
    Exited { server: String, code: Option<i32> },
}

impl LspError {
    /// The name of the server that failed.
    #[must_use]
    pub fn server(&self) -> &str {
        match self {
            Self::NotInstalled { server, .. }
            | Self::Spawn { server, .. }
            | Self::Initialize { server, .. }
            | Self::Protocol { server, .. }
            | Self::Timeout { server, .. }
            | Self::Exited { server, .. } => server,
        }
    }

    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when restarting or re-asking may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }

    /// True when installing something would fix this.
    #[must_use]
    pub fn is_missing_binary(&self) -> bool {
        matches!(self, Self::NotInstalled { .. })
    }
}

impl Recoverable for LspError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Timeout { .. } | Self::Exited { .. } => Recovery::Retry { after: None },
            Self::NotInstalled { .. }
            | Self::Spawn { .. }
            | Self::Initialize { .. }
            | Self::Protocol { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<LspError> {
        vec![
            LspError::NotInstalled {
                server: "typescript".to_owned(),
                command: "typescript-language-server".to_owned(),
            },
            LspError::Spawn {
                server: "typescript".to_owned(),
                command: "typescript-language-server".to_owned(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
            },
            LspError::Initialize {
                server: "typescript".to_owned(),
                source: Box::new(std::io::Error::other("bad capabilities")),
            },
            LspError::Protocol {
                server: "typescript".to_owned(),
                source: serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
            },
            LspError::Timeout {
                server: "typescript".to_owned(),
                elapsed: Duration::from_secs(10),
            },
            LspError::Exited {
                server: "typescript".to_owned(),
                code: Some(1),
            },
        ]
    }

    #[test]
    fn every_variant_names_its_server() {
        for e in every_variant() {
            assert_eq!(e.server(), "typescript", "{e}");
        }
    }

    #[test]
    fn timeout_and_exit_retry_and_the_rest_do_not() {
        for e in every_variant() {
            let expected = matches!(e, LspError::Timeout { .. } | LspError::Exited { .. });
            assert_eq!(e.is_retryable(), expected, "{e}");
        }
    }

    #[test]
    fn a_missing_binary_is_distinguishable_without_reading_a_message() {
        let missing = LspError::NotInstalled {
            server: "gopls".to_owned(),
            command: "gopls".to_owned(),
        };
        let unstartable = LspError::Spawn {
            server: "gopls".to_owned(),
            command: "gopls".to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        assert!(missing.is_missing_binary());
        assert!(!unstartable.is_missing_binary());
        assert_eq!(
            missing.to_string(),
            "language server gopls is not installed (looked for gopls)"
        );
    }

    #[test]
    fn exit_carries_the_status_code() {
        let e = LspError::Exited {
            server: "gopls".to_owned(),
            code: Some(2),
        };
        assert_eq!(e.to_string(), "language server gopls exited (code=Some(2))");
        let LspError::Exited { code, .. } = &e else {
            panic!("constructed an Exited, matched something else");
        };
        assert_eq!(*code, Some(2));
    }
}
