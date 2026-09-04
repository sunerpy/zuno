//! [`AuthError`] — what can go wrong reading or writing a credential file.
//!
//! # Why the variants live here and not in `zuno-error`
//!
//! `zuno-error` is the workspace taxonomy and has no auth-storage domain in it;
//! adding one means editing that crate, which task 24 does not own. So the
//! variants live here and follow the same doctrine, verbatim from
//! `zuno-error/src/lib.rs`:
//!
//! - **No catch-all.** No `Other(String)`, no `Unknown { message }`. Seven
//!   variants, each a distinct thing that happened.
//! - **Anything a decision needs is a field.** Every variant carries the
//!   [`PathBuf`] it failed on, and every variant with a lower-level failure
//!   underneath it carries that in `#[source]` position, so `ErrorKind` and the
//!   JSON line/column survive to whoever reports it.
//!   [`AuthError::Unresolved`] has no source: several reads failed, and the finding
//!   is that they disagreed with the filesystem rather than any one of their errors.
//!
//! There is deliberately **no** variant for a credential file that holds no bytes.
//! An empty file holds no entry a write could destroy, and every read of it is
//! either a display (`zuno auth list`, `zuno models`) or the read half of the write
//! that repairs it (`zuno auth login`, a token refresh). Reporting it as a failure
//! denied all of them to exactly the users whose file the shipped 0.6.6 truncate
//! window emptied. It is [`crate::StoreDamage`] instead: data that travels with a
//! successful read.
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
//!
//! [`AuthError::Unresolved`] is the one that describes a transient condition, and it
//! is still [`Recovery::Fail`]. The retry that helps is the user running the command
//! again, not a mechanical repeat inside a credential write: that write is an
//! at-most-once side effect, and a loop that re-reads and re-publishes on its own is
//! how a lost update becomes automatic. The message says so.

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

    /// The file is there and could not be opened, so what it holds is unknown.
    ///
    /// Only a read whose result would be written back raises this. Concluding
    /// "absent, therefore no credentials" from an open that failed inside another
    /// process's publication is how a write comes to publish a store holding only
    /// its own entry, so an unconfirmable file is reported as unresolved instead. On
    /// Windows a `ReplaceFileW` in flight produces exactly that answer for a file
    /// that is present and complete.
    #[error(
        "credential file {path} exists but could not be read while it was being replaced, so \
             what it holds is unknown; nothing was written — run the command again"
    )]
    Unresolved {
        /// The file whose contents could not be resolved.
        path: PathBuf,
    },

    /// The containing directory would not accept the file publication needs to create.
    ///
    /// Publishing writes a sibling and renames it, so it needs permission to create a
    /// file in the directory — where an in-place truncate needed only permission on the
    /// file. A hardened layout (data directory `0555`, `auth.json` `0600`) that could
    /// refresh a token before therefore fails here, and it is the directory that
    /// refused, not the credential file. Naming the file instead would send the
    /// operator to `chmod` the wrong thing.
    ///
    /// Raised on Unix only. Off Unix the publication is
    /// `zuno_atomic_file::replace`'s, which reports one `io::Error` for the whole
    /// operation and gives this module nothing to attribute, so the same condition
    /// arrives as [`AuthError::Write`] there.
    #[error(
        "credential file {path} could not be published because directory {directory} would \
             not accept a new file; publication writes a sibling and renames it, so the \
             directory needs write permission"
    )]
    Directory {
        /// The credential file that was being published.
        path: PathBuf,
        /// The directory that refused.
        directory: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
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
            | Self::Unresolved { path }
            | Self::Directory { path, .. }
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
            | Self::Unresolved { .. }
            | Self::Directory { .. }
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
            AuthError::Unresolved { path: path.clone() },
            AuthError::Directory {
                path: path.clone(),
                directory: PathBuf::from("/tmp"),
                source: std::io::Error::from(ErrorKind::PermissionDenied),
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

    /// The message is the only place a user is told what to do about a store this
    /// build refused to write over, so the text has to carry the remedy and state
    /// that nothing was destroyed by reporting it.
    #[test]
    fn the_unresolved_message_carries_a_remedy() {
        let path = PathBuf::from("/tmp/auth.json");
        let unresolved = AuthError::Unresolved { path }.to_string();
        for expected in [
            "while it was being replaced",
            "nothing was written",
            "run the command again",
        ] {
            assert!(
                unresolved.contains(expected),
                "{unresolved} is missing {expected:?}"
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
