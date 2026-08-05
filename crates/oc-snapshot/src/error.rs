//! Typed failures for the snapshot store.
//!
//! # Why this is a crate-local error and not an `oc_error::Error` variant
//!
//! `oc-error` deliberately has no `Other(String)` catch-all, no `Io` variant and
//! no process/exec variant, and its aggregate `Error` is not `#[non_exhaustive]`
//! — adding a variant to it is a breaking change for every exhaustive `match` in
//! the workspace. `oc-paths` set the precedent for that situation by owning
//! `PathsError` locally, and this crate follows it: a Git invocation failure is a
//! snapshot-domain failure, and misfiling it as `ToolError::Failed` (a *model*
//! tool failing) or `LspError::Spawn` would lie about where it came from.
//!
//! Every variant names the operation it failed during, because an I/O failure
//! always happens *while doing something*.

use std::path::PathBuf;
use std::process::ExitStatus;

/// A snapshot-store failure.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// `git` could not be spawned at all — not installed, not executable, or the
    /// working directory does not exist.
    #[error("failed to spawn `git {}` in {}", args.join(" "), cwd.display())]
    Spawn {
        /// The argument vector passed to `git`, for diagnosis.
        args: Vec<String>,
        /// The working directory the command was to run in.
        cwd: PathBuf,
        /// The underlying spawn failure.
        #[source]
        source: std::io::Error,
    },

    /// `git` ran and exited non-zero for an operation whose failure cannot be
    /// tolerated.
    #[error("`git {}` failed with {code}: {stderr}", args.join(" "), code = describe(*code), stderr = stderr.trim())]
    Git {
        /// The argument vector passed to `git`.
        args: Vec<String>,
        /// The exit code, or `None` when the process was killed by a signal.
        code: Option<i32>,
        /// Captured standard error, trimmed when displayed.
        stderr: String,
    },

    /// `git` produced output that is not valid UTF-8. Snapshot paths are read
    /// back with `core.quotepath=false`, so this means a genuinely undecodable
    /// byte sequence rather than an escaped path.
    #[error("`git {}` produced output that is not valid utf-8", args.join(" "))]
    Encoding {
        /// The argument vector passed to `git`.
        args: Vec<String>,
        /// The underlying decode failure.
        #[source]
        source: std::string::FromUtf8Error,
    },

    /// A filesystem operation on the store itself failed — creating the object
    /// directory, writing `info/exclude`, or seeding `objects/info/alternates`.
    #[error("failed to {operation} {}", path.display())]
    Store {
        /// What was being attempted, as a verb phrase: `create`, `write`, `read`.
        operation: &'static str,
        /// The path being operated on.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The snapshot root could not be scanned while counting store references.
    #[error("failed to scan snapshot root {}", root.display())]
    Scan {
        /// The snapshot root that could not be read.
        root: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
}

/// A `Result` specialised to [`SnapshotError`].
pub type Result<T, E = SnapshotError> = std::result::Result<T, E>;

fn describe(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit code {code}"),
        None => "a signal".to_owned(),
    }
}

impl SnapshotError {
    /// Build a [`SnapshotError::Git`] from a finished command.
    pub(crate) fn git(args: &[String], status: ExitStatus, stderr: String) -> Self {
        Self::Git {
            args: args.to_vec(),
            code: status.code(),
            stderr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_failure_names_the_command_and_the_code() {
        let error = SnapshotError::Git {
            args: vec!["write-tree".to_owned()],
            code: Some(128),
            stderr: "fatal: not a git repository\n".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "`git write-tree` failed with exit code 128: fatal: not a git repository"
        );
    }

    #[test]
    fn a_signal_death_is_reported_as_a_signal() {
        let error = SnapshotError::Git {
            args: vec!["gc".to_owned()],
            code: None,
            stderr: String::new(),
        };
        assert_eq!(error.to_string(), "`git gc` failed with a signal: ");
    }

    #[test]
    fn store_failures_name_the_operation_and_the_path() {
        let error = SnapshotError::Store {
            operation: "create",
            path: PathBuf::from("/data/snapshot/p/h"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert_eq!(error.to_string(), "failed to create /data/snapshot/p/h");
    }
}
