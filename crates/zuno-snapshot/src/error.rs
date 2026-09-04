//! Typed failures for the snapshot store.
//!
//! # Why this is a crate-local error and not an `zuno_error::Error` variant
//!
//! `zuno-error` deliberately has no `Other(String)` catch-all, no `Io` variant and
//! no process/exec variant, and its aggregate `Error` is not `#[non_exhaustive]`
//! — adding a variant to it is a breaking change for every exhaustive `match` in
//! the workspace. `zuno-paths` set the precedent for that situation by owning
//! `PathsError` locally, and this crate follows it: a Git invocation failure is a
//! snapshot-domain failure, and misfiling it as `ToolError::Failed` (a *model*
//! tool failing) or `LspError::Spawn` would lie about where it came from.
//!
//! Every variant names the operation it failed during, because an I/O failure
//! always happens *while doing something*.

use std::path::PathBuf;
use std::process::ExitStatus;

use crate::turn::TurnRestore;

/// A snapshot-store failure.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A caller attempted to restore a checkpoint for a disabled store.
    ///
    /// Refusing is safer than treating the request as a successful no-op: the
    /// latter would let a UI claim that user files were restored when none were.
    #[error("refusing to restore a turn because snapshots are disabled")]
    SnapshotsDisabled,

    /// The captured worktree no longer equals the boundary restoration expects.
    ///
    /// This catches manual edits, deletions, and any other captured change before
    /// the first target file is touched. The caller gets the full drift list so it
    /// can explain the refusal instead of reducing it to a generic failure.
    #[error(
        "refusing to overwrite worktree drift: expected tree {expected}, found {actual}; changed paths: {}",
        files.join(", ")
    )]
    WorktreeDrift {
        /// Tree hash restoration required as its source boundary.
        expected: String,
        /// Tree hash captured from the current worktree.
        actual: String,
        /// Forward-slashed worktree-relative paths that drifted.
        files: Vec<String>,
    },

    /// At least one path the transition would touch is now ignored by the user's
    /// repository. Ignore rules are an ownership boundary, so guessing that an
    /// ignored generated file is disposable would be unsafe.
    #[error("refusing to modify files that are now gitignored: {}", files.join(", "))]
    IgnoredFiles {
        /// Forward-slashed worktree-relative ignored paths.
        files: Vec<String>,
    },

    /// Git reported success but the resulting tree did not equal the requested
    /// boundary. This is an invariant failure, not a result a caller may present as
    /// a successful partial undo.
    ///
    /// On its own this variant only ever escapes from a transition that applied no
    /// patch at all; once a patch has been applied the same mismatch is wrapped in
    /// [`SnapshotError::RestoreUncertain`], because by then files have moved.
    #[error("turn restore verification failed: expected tree {expected}, found {actual}")]
    RestoreVerification {
        /// Requested target tree.
        expected: String,
        /// Tree captured after applying the transition.
        actual: String,
    },

    /// Restoration rewrote worktree files and then could not confirm that the
    /// requested boundary was reached.
    ///
    /// This is an **uncertain outcome**, not a refusal: some files hold their
    /// pre-restore content and some hold their post-restore content, and the store
    /// index may describe either. `git apply --index --check` does not test whether
    /// the target paths are writable, so a patch that passes the preflight can still
    /// die half-written (verified on git 2.43.0). A client must never render this as
    /// "rejected", "refused" or "nothing changed", and nothing may replay the call:
    /// the persisted evidence file exists so the next step is inspection of
    /// authoritative worktree state.
    #[error(
        "{restore} rewrote files and then could not reach tree {expected} (worktree now {observed}); \
         the worktree is in an uncertain state — inspect {evidence} before restoring again",
        observed = actual.as_deref().unwrap_or("unknown"),
        evidence = evidence.display()
    )]
    RestoreUncertain {
        /// Which direction was being restored.
        restore: TurnRestore,
        /// The tree the transition was moving toward.
        expected: String,
        /// The tree observed afterward, or `None` when even that could not be read.
        actual: Option<String>,
        /// The persisted evidence record describing what was observed.
        evidence: PathBuf,
        /// The failure that interrupted the transition.
        #[source]
        source: Box<SnapshotError>,
    },

    /// An earlier restore against this store left an unresolved uncertain outcome.
    ///
    /// Refusing every later restore is the "require authoritative-state inspection"
    /// half of handling an uncertain outcome: the worktree no longer matches either
    /// captured boundary, so a second transition would compound the damage. Nothing
    /// is modified, and the evidence stays on disk until it is explicitly resolved
    /// through [`crate::Store::resolve_uncertain_restore`].
    #[error(
        "refusing to {restore}: an earlier restore left the worktree in an uncertain state; \
         inspect {evidence} and resolve it before restoring again",
        evidence = evidence.display()
    )]
    RestoreUnresolved {
        /// The direction that was refused.
        restore: TurnRestore,
        /// The persisted evidence record that must be inspected.
        evidence: PathBuf,
    },

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

    /// The worktree root's bytes are not valid UTF-8, so no absolute path can be
    /// reported for it.
    ///
    /// [`Store::patch`](crate::Store::patch) is the one report built by joining this
    /// root onto Git's worktree-relative paths, and a lossy conversion there names
    /// files that do not exist — a `U+FFFD` path whose `Path::exists()` is false,
    /// which is worse than reporting nothing. Capture, restore and undo build no
    /// absolute paths and keep working; the refusal is scoped to the report, and
    /// renaming the directory clears it.
    ///
    /// The root itself is deliberately absent from the message: rendering it needs
    /// the very lossy conversion this variant exists to refuse.
    #[error(
        "refusing to report snapshot paths: the worktree root is not valid utf-8 \
         (valid up to byte {valid_up_to})"
    )]
    UndecodableWorktree {
        /// How many leading bytes of the root did decode, so the offending path
        /// component can be located without printing the root.
        valid_up_to: usize,
    },

    /// A filesystem operation on the store itself failed — creating the object
    /// directory, writing `info/exclude`, seeding `objects/info/alternates`, or
    /// reading back a persisted uncertain-restore record.
    #[error("failed to {operation} {}", path.display())]
    Store {
        /// What was being attempted, as a verb phrase: `create`, `write`, `read`,
        /// `parse`, `remove`.
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

    /// Whether this failure is *provably* free of worktree modification.
    ///
    /// A client may only say "refused", "rejected" or "nothing changed" when this
    /// returns `true`. Every failure raised by
    /// [`Store::restore_turn`](crate::Store::restore_turn) before the mutating
    /// `git apply` is reported as itself, and every failure at or after it is
    /// wrapped in [`SnapshotError::RestoreUncertain`] — so this single predicate is
    /// the honest boundary between the two, and no client has to re-derive it from
    /// rendered text.
    #[must_use]
    pub const fn worktree_untouched(&self) -> bool {
        !matches!(self, Self::RestoreUncertain { .. })
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
