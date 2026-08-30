//! [`AuthError`] — what can go wrong reading or writing a credential file.
//!
//! # Why the variants live here and not in `zuno-error`
//!
//! `zuno-error` is the workspace taxonomy and has no auth-storage domain in it;
//! adding one means editing that crate, which task 24 does not own. So the
//! variants live here and follow the same doctrine, verbatim from
//! `zuno-error/src/lib.rs`:
//!
//! - **No catch-all.** No `Other(String)`, no `Unknown { message }`. Five
//!   variants, each a distinct thing that happened.
//! - **Anything a decision needs is a field.** Every variant carries the
//!   [`PathBuf`] it failed on, and the concrete `std::io::Error` or
//!   `serde_json::Error` in `#[source]` position, so `ErrorKind` and the JSON
//!   line/column survive to whoever reports it.
//! - **Not `#[non_exhaustive]`.** Every consumer is in this workspace; an added
//!   variant should break each match until its author decides what it means.
//! - **No `String` message field**, so nothing downstream can be tempted to
//!   classify a failure by scraping its rendered text.
//!
//! It implements [`zuno_error::Recoverable`], so a caller holding a heterogeneous
//! set of failures routes this one the same way it routes a
//! [`zuno_error::ProviderError`].
//!
//! # No variant is retryable
//!
//! A credential file does not become readable, or its JSON well-formed, because
//! you asked twice. Every variant is [`Recovery::Fail`]: surface it. In
//! particular none of them is `Reauthenticate` — that is the answer when a
//! *provider* rejects a credential (`ProviderError::Auth`), whereas these are
//! failures to reach the store at all, and a fresh login would write to the same
//! unwritable path.

use std::path::PathBuf;

use zuno_error::{Recoverable, Recovery};

/// A failure reading or writing `auth.json` or `mcp-auth.json`.
///
/// Never contains a credential value: the paths and the underlying
/// `io`/`serde_json` errors are all that is carried, and `serde_json` truncates
/// its own snippets to a position rather than quoting the document.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The file exists but could not be read. `std::io::ErrorKind` survives in
    /// the source, so a reporter can distinguish `PermissionDenied` from the
    /// rest.
    #[error("credential file {path} could not be read")]
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The file is not well-formed JSON. The concrete `serde_json::Error`
    /// preserves line and column so a reporter can point at the damage.
    #[error("credential file {path} is not valid JSON")]
    Malformed {
        /// The file that failed to parse.
        path: PathBuf,
        /// The parse failure, carrying line and column.
        #[source]
        source: serde_json::Error,
    },

    /// The file could not be written.
    #[error("credential file {path} could not be written")]
    Write {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The credential map could not be encoded to JSON. Distinct from
    /// [`AuthError::Write`] because nothing about the filesystem is wrong.
    #[error("credential data for {path} could not be encoded")]
    Serialize {
        /// The file the encoding was destined for.
        path: PathBuf,
        /// The encoding failure.
        #[source]
        source: serde_json::Error,
    },

    /// The file was written but its mode could not be restricted to `0600`.
    ///
    /// A separate variant because the credential is now on disk readable by
    /// somebody else, which is a disclosure to report rather than a write to
    /// retry.
    #[error("credential file {path} could not be restricted to mode 0600")]
    Permissions {
        /// The file whose mode could not be set.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
}

impl AuthError {
    /// The file this failure concerns.
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Read { path, .. }
            | Self::Malformed { path, .. }
            | Self::Write { path, .. }
            | Self::Serialize { path, .. }
            | Self::Permissions { path, .. } => path,
        }
    }

    /// The action this failure calls for, which is always to surface it.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when the identical operation may be attempted again. Never, here.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::is_retryable(self)
    }
}

impl Recoverable for AuthError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Read { .. }
            | Self::Malformed { .. }
            | Self::Write { .. }
            | Self::Serialize { .. }
            | Self::Permissions { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn every_variant() -> Vec<AuthError> {
        let path = PathBuf::from("/tmp/auth.json");
        let json = serde_json::from_str::<serde_json::Value>("{ nope").expect_err("parse error");
        vec![
            AuthError::Read {
                path: path.clone(),
                source: std::io::Error::new(ErrorKind::PermissionDenied, "denied"),
            },
            AuthError::Malformed {
                path: path.clone(),
                source: json,
            },
            AuthError::Write {
                path: path.clone(),
                source: std::io::Error::new(ErrorKind::StorageFull, "full"),
            },
            AuthError::Serialize {
                path: path.clone(),
                source: serde_json::from_str::<serde_json::Value>("[").expect_err("parse error"),
            },
            AuthError::Permissions {
                path,
                source: std::io::Error::from(ErrorKind::PermissionDenied),
            },
        ]
    }

    #[test]
    fn every_variant_names_its_file_and_fails() {
        for error in every_variant() {
            assert_eq!(error.path(), &PathBuf::from("/tmp/auth.json"));
            assert_eq!(error.recovery(), Recovery::Fail);
            assert!(!error.is_retryable());
            assert_eq!(error.retry_after(), None);
            assert!(
                error.to_string().contains("/tmp/auth.json"),
                "{error} should name the file"
            );
        }
    }

    /// The `ErrorKind` a caller would branch on has to survive the wrapping.
    #[test]
    fn the_io_kind_survives_in_the_source() {
        let error = AuthError::Read {
            path: PathBuf::from("/tmp/auth.json"),
            source: std::io::Error::new(ErrorKind::PermissionDenied, "denied"),
        };
        let source = std::error::Error::source(&error).expect("source");
        let io = source
            .downcast_ref::<std::io::Error>()
            .expect("io error preserved");
        assert_eq!(io.kind(), ErrorKind::PermissionDenied);
    }
}
