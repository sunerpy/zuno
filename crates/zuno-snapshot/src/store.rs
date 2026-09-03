//! The per-project Git object store.
//!
//! Ports `packages/opencode/src/snapshot/index.ts`. One store serves **every**
//! session that works in the same `(project id, worktree)` pair; snapshots inside
//! it are addressed by tree hash. That is why [`crate::refcount`] exists at all —
//! a store may only be deleted once no surviving session references it.
//!
//! # Upstream-only observed path contract
//!
//! Verified by running the real binary (`opencode 1.18.12`,
//! `debug snapshot track`) under a temporary `XDG_DATA_HOME`:
//!
//! ```text
//! $XDG_DATA_HOME/opencode/snapshot/<projectID>/<sha1(worktree path string)>
//! ```
//!
//! Zuno keeps the structure but places it under its independent
//! `$XDG_DATA_HOME/zuno/snapshot` root. In both cases `projectID` is resolved by
//! `zuno_paths::project::resolve_project` and the
//! second component `sha1::hex` of the worktree's absolute path *string* — not
//! normalized, not canonicalized, no trailing-slash handling. `zuno-paths` already
//! implements both halves as [`zuno_paths::Layout::snapshot_store`], so this crate
//! consumes it rather than re-deriving it.
//!
//! # Divergences from the oracle
//!
//! The oracle logs and swallows failures of `write-tree`, `read-tree` and
//! `checkout-index`, which makes `track()` return the empty string as if it were a
//! hash and makes `restore()` a silent no-op. Both are reported as typed errors
//! here: a snapshot hash that is silently empty breaks revert later, far away from
//! the cause.
//!
//! Staging diverges too. The oracle tolerates a failed `git add` on the theory that
//! one unreadable file should not cost a whole snapshot; that theory is wrong,
//! because `git add` writes its index once at the end and a single unreadable path
//! aborts the command before it does, so *nothing* is staged. Zuno asks git for the
//! behaviour the tolerance assumed — `--ignore-errors` — and treats any exit code
//! other than 0 (clean) or 1 (some paths skipped) as a failure. See [`Store::stage`].
//!
//! Failures that genuinely cannot corrupt a capture — a failed `diff`, a failed `gc`
//! — stay tolerated.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use zuno_paths::node_path;

use crate::error::{Result, SnapshotError};
use crate::git::{self, Argv, CFG, CORE, QUOTE};
use crate::lock;
use crate::refcount::StoreKey;
use crate::turn::{
    FileOperation, RestoredFile, TurnCapture, TurnCheckpoint, TurnRestore, TurnRestoreReport,
};

/// `prune` in `packages/opencode/src/snapshot/index.ts:23` — the argument to
/// `git gc --prune=`.
pub const PRUNE: &str = "7.days";

/// `limit` in `packages/opencode/src/snapshot/index.ts:24` — untracked files
/// larger than this are excluded instead of stored.
pub const LARGE_FILE_LIMIT: u64 = 2 * 1024 * 1024;

/// The `git gc` argument vector the hourly schedule runs, for assertions.
pub const GC_ARGS: [&str; 2] = ["gc", "--prune=7.days"];

/// The uncertain-restore record's file name inside a store's git directory.
pub const UNCERTAIN_RESTORE_FILE: &str = "zuno-restore-uncertain.json";

/// What the next `git add` will stage, and what it deliberately will not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StagePlan {
    stage: Vec<String>,
    exclusions: CaptureExclusions,
}

/// A snapshot patch: the tree it was taken against and the absolute paths that
/// changed since. Mirrors the oracle's `Patch` schema.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Patch {
    /// The tree hash the diff was taken against.
    pub hash: String,
    /// Absolute, forward-slashed paths of the changed files.
    pub files: Vec<String>,
}

/// The paths a capture deliberately left out of its tree.
///
/// Every path listed here is invisible to [`Store::restore_turn`]: it is in neither
/// captured tree, so `/undo` and `/redo` cannot move it and will not mention it
/// unless the client reports these fields. Exclusion is a design decision, not a
/// failure — the point of recording it is that the user can be told.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureExclusions {
    #[serde(default)]
    ignored: Vec<String>,
    #[serde(default)]
    oversized: Vec<String>,
    #[serde(default)]
    unreadable: Vec<String>,
}

impl CaptureExclusions {
    /// Paths left out because the user's repository ignores them.
    ///
    /// Ignore rules are an ownership boundary: a file the repository declines to
    /// track is one the snapshot store declines to own.
    #[must_use]
    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    /// Untracked paths left out for exceeding [`LARGE_FILE_LIMIT`].
    #[must_use]
    pub fn oversized(&self) -> &[String] {
        &self.oversized
    }

    /// Paths `git add` could not read, and therefore skipped.
    ///
    /// The capture is still complete for every *other* path, which is why staging
    /// runs with `--ignore-errors`; these paths simply keep whatever content the
    /// tree held before.
    #[must_use]
    pub fn unreadable(&self) -> &[String] {
        &self.unreadable
    }

    /// Whether anything at all was left out.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ignored.is_empty() && self.oversized.is_empty() && self.unreadable.is_empty()
    }

    /// Every excluded path exactly once, sorted.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .ignored
            .iter()
            .chain(self.oversized.iter())
            .chain(self.unreadable.iter())
            .cloned()
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// A one-clause account of what was left out, or `None` when nothing was.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut reasons: Vec<String> = Vec::new();
        if !self.oversized.is_empty() {
            reasons.push(format!(
                "{} over the {} MiB untracked-file limit",
                self.oversized.len(),
                LARGE_FILE_LIMIT / (1024 * 1024)
            ));
        }
        if !self.ignored.is_empty() {
            reasons.push(format!("{} matching an ignore rule", self.ignored.len()));
        }
        if !self.unreadable.is_empty() {
            reasons.push(format!("{} unreadable", self.unreadable.len()));
        }
        Some(format!(
            "{} path(s) are outside this snapshot and were not restored: {}",
            self.paths().len(),
            reasons.join(", ")
        ))
    }

    /// Fold another capture's exclusions in, keeping each path once.
    pub(crate) fn merge(&mut self, other: Self) {
        merge_paths(&mut self.ignored, other.ignored);
        merge_paths(&mut self.oversized, other.oversized);
        merge_paths(&mut self.unreadable, other.unreadable);
    }
}

fn merge_paths(into: &mut Vec<String>, from: Vec<String>) {
    into.extend(from);
    into.sort();
    into.dedup();
}

/// One tracked worktree capture: the tree that was written, and what it omits.
///
/// [`Store::track`] returns only the hash, which is what makes an incomplete
/// capture easy to mistake for a complete one. This type keeps the two facts
/// together so a caller that wants to report the omissions can.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    tree: String,
    exclusions: CaptureExclusions,
}

impl Capture {
    /// The tree hash `write-tree` produced.
    #[must_use]
    pub fn tree(&self) -> &str {
        &self.tree
    }

    /// What this capture left out.
    #[must_use]
    pub const fn exclusions(&self) -> &CaptureExclusions {
        &self.exclusions
    }

    /// Discard the exclusions and keep the hash.
    #[must_use]
    pub fn into_tree(self) -> String {
        self.tree
    }
}

/// A persisted record of a restore that rewrote files and then could not confirm
/// the boundary it was moving to.
///
/// Written into the store's own directory as `zuno-restore-uncertain.json`. It is
/// durable on purpose: the process that produced the uncertainty may not survive to
/// explain it, and the worktree it describes is the user's.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UncertainRestore {
    /// Which direction was being restored.
    pub restore: TurnRestore,
    /// The tree the worktree held before the transition started.
    pub from: String,
    /// The tree the transition was moving toward.
    pub to: String,
    /// The tree observed after the interrupted transition, when it could be read.
    #[serde(default)]
    pub observed: Option<String>,
    /// The rendered failure that interrupted the transition, for the record.
    pub cause: String,
    /// Seconds since the Unix epoch, or `None` if the clock was unreadable.
    #[serde(default)]
    pub recorded_unix_seconds: Option<u64>,
}

/// The outcome of one garbage-collection pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GcOutcome {
    /// `git gc --prune=7.days` ran and exited zero.
    Collected,
    /// Snapshots are disabled for this location, so nothing ran.
    Disabled,
    /// The store does not exist yet, so nothing ran.
    Missing,
    /// `git gc` exited non-zero. Tolerated, exactly as the oracle tolerates it:
    /// a store that cannot be compacted is still a usable store.
    Failed {
        /// The exit code, or `None` on a signal.
        code: Option<i32>,
        /// Captured standard error.
        stderr: String,
    },
}

/// Where a store lives and what it is allowed to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The snapshot root — `zuno_paths::snapshot_root()` in production.
    pub root: PathBuf,
    /// The project id, the first path component under the root.
    pub project_id: String,
    /// The worktree root. Hashed to form the second path component, and the scope of
    /// every capture: a snapshot tree always describes the whole worktree.
    pub worktree: PathBuf,
    /// The startup directory, which narrows the [`Store::patch`] *report* only.
    ///
    /// It deliberately does not narrow capture. `write-tree` takes no pathspec and
    /// restoration diffs and applies across the whole worktree, so staging only this
    /// directory would produce a whole-worktree tree that omitted every change
    /// outside it — the checkpoint would describe a worktree that never existed.
    pub directory: PathBuf,
    /// Whether the project is backed by Git. `false` disables snapshots entirely,
    /// matching the oracle's `state.vcs !== "git"` guard.
    pub git: bool,
    /// The resolved `snapshot` config value. `false` disables snapshots.
    pub enabled: bool,
}

impl Location {
    /// A location covering a whole worktree, with snapshots enabled.
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        project_id: impl Into<String>,
        worktree: impl Into<PathBuf>,
    ) -> Self {
        let worktree = worktree.into();
        Self {
            root: root.into(),
            project_id: project_id.into(),
            directory: worktree.clone(),
            worktree,
            git: true,
            enabled: true,
        }
    }

    /// Resolve a location from a starting directory using the same project
    /// discovery the oracle uses, against the process-wide snapshot root.
    #[must_use]
    pub fn discover(start: &Path) -> Self {
        Self::discover_in(zuno_paths::snapshot_root(), start)
    }

    /// [`Location::discover`] against an explicit snapshot root, for tests and for
    /// callers that already resolved a layout.
    #[must_use]
    pub fn discover_in(root: impl Into<PathBuf>, start: &Path) -> Self {
        let project = zuno_paths::project::resolve_project(start);
        let worktree = project.directory.clone();
        Self {
            root: root.into(),
            project_id: project.id,
            directory: start.to_path_buf(),
            worktree,
            git: project.vcs.is_some(),
            enabled: true,
        }
    }

    /// Narrow the [`Store::patch`] report to a subdirectory of the worktree.
    ///
    /// Captures stay whole-worktree; see [`Location::directory`].
    #[must_use]
    pub fn with_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.directory = directory.into();
        self
    }

    /// Apply the resolved `snapshot` config value.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Override Git backing, for a project the caller already knows is not a
    /// repository.
    #[must_use]
    pub fn with_git(mut self, git: bool) -> Self {
        self.git = git;
        self
    }

    /// The store key this location maps to.
    #[must_use]
    pub fn key(&self) -> StoreKey {
        StoreKey::new(&self.project_id, &self.worktree)
    }
}

/// A Git object store shared by every session working in one worktree.
#[derive(Clone, Debug)]
pub struct Store {
    location: Location,
    git_dir: PathBuf,
}

impl Store {
    /// Open a store handle. Touches no filesystem; the store is created lazily by
    /// the first [`Store::track`].
    #[must_use]
    pub fn open(location: Location) -> Self {
        let git_dir = location.key().path_in(&location.root);
        Self { location, git_dir }
    }

    /// The store's git directory — `snapshot/<projectID>/<sha1(worktree)>`.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The location this store was opened for.
    #[must_use]
    pub fn location(&self) -> &Location {
        &self.location
    }

    /// The `(project id, worktree hash)` key, for reference counting.
    #[must_use]
    pub fn key(&self) -> StoreKey {
        self.location.key()
    }

    /// Whether the store directory exists on disk yet.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.git_dir.is_dir()
    }

    /// `enabled()` — Git-backed and not disabled by config.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.location.git && self.location.enabled
    }

    /// Stage the worktree into the store's private index and write a tree.
    ///
    /// Returns the tree hash, or `None` when snapshots are disabled for this
    /// location. Creates and initializes the store on first use.
    ///
    /// The hash alone cannot say what the capture left out; use [`Store::capture`]
    /// when that matters.
    pub fn track(&self) -> Result<Option<String>> {
        Ok(self.capture()?.map(Capture::into_tree))
    }

    /// [`Store::track`], keeping the omissions alongside the tree hash.
    pub fn capture(&self) -> Result<Option<Capture>> {
        if !self.enabled() {
            return Ok(None);
        }
        let _guard = lock::acquire(&self.git_dir);

        self.track_locked().map(Some)
    }

    /// Capture the tree before one complete user turn.
    ///
    /// The caller must keep the returned value across every provider step and tool
    /// call in that turn, then finish it exactly once at the terminal turn boundary
    /// — including when the turn failed, which is what [`TurnCapture::finish_with`]
    /// is for. No capture is returned when snapshots are disabled.
    pub fn begin_turn(&self) -> Result<Option<TurnCapture>> {
        let Some(before) = self.capture()? else {
            return Ok(None);
        };
        let (tree, exclusions) = (before.tree().to_owned(), before.exclusions().clone());
        Ok(Some(TurnCapture::new(self.clone(), tree, exclusions)))
    }

    /// Move one complete turn checkpoint backward or forward, but only when the
    /// current captured worktree is still exactly the expected source tree.
    ///
    /// Safety is deliberately whole-worktree and fail-closed. A manual edit to any
    /// captured path refuses the entire operation before the patch is applied;
    /// files are never restored one-by-one around a conflict. The generated patch
    /// is applied through Git with both the private index and worktree checked, so
    /// additions and deletions are handled as well as content replacement.
    ///
    /// # Refusal versus uncertainty
    ///
    /// Every failure raised before the mutating `git apply` leaves the worktree
    /// byte-for-byte untouched, and is returned as itself. Every failure at or after
    /// it is wrapped in [`SnapshotError::RestoreUncertain`], persisted, and blocks
    /// further restores until resolved — because `git apply --index --check` does not
    /// test writability, so a patch that passes the preflight can still die
    /// half-written. [`SnapshotError::worktree_untouched`] is the predicate a client
    /// must use before saying "nothing changed".
    pub fn restore_turn(
        &self,
        checkpoint: &TurnCheckpoint,
        restore: TurnRestore,
    ) -> Result<TurnRestoreReport> {
        if !self.enabled() {
            return Err(SnapshotError::SnapshotsDisabled);
        }
        let _guard = lock::acquire(&self.git_dir);
        // An earlier uncertain outcome must be inspected before this store is allowed
        // to rewrite user files again; the worktree it left behind matches neither
        // boundary, so a second transition would compound the damage.
        if self.uncertain_restore()?.is_some() {
            return Err(SnapshotError::RestoreUnresolved {
                restore,
                evidence: self.uncertain_path(),
            });
        }
        let (expected, target) = checkpoint.transition(restore);
        let current = self.track_locked()?;
        if current.tree() != expected {
            return Err(SnapshotError::WorktreeDrift {
                files: self.changed_paths(expected, current.tree())?,
                expected: expected.to_owned(),
                actual: current.into_tree(),
            });
        }

        let files = self.transition_files(expected, target)?;
        let affected: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        let mut ignored: Vec<String> = self.ignore(&affected)?.into_iter().collect();
        if !ignored.is_empty() {
            ignored.sort();
            return Err(SnapshotError::IgnoredFiles { files: ignored });
        }

        let after = if files.is_empty() {
            current
        } else {
            let patch = self.transition_patch(expected, target)?;
            // `git apply --index` repeats this same content check. Running both
            // closes the ordinary check/apply window while retaining a no-mutation
            // preflight whose failure is guaranteed to leave every file untouched.
            self.apply_transition(&patch, true)?;

            // Past this line the worktree may already have been rewritten, so no
            // failure can be reported as a refusal.
            match self.apply_transition(&patch, false) {
                Ok(()) => match self.track_locked() {
                    Ok(after) if after.tree() == target => after,
                    Ok(after) => {
                        let observed = after.into_tree();
                        return Err(self.record_uncertain(
                            restore,
                            expected,
                            target,
                            Some(observed.clone()),
                            SnapshotError::RestoreVerification {
                                expected: target.to_owned(),
                                actual: observed,
                            },
                        ));
                    }
                    Err(cause) => {
                        return Err(self.record_uncertain(restore, expected, target, None, cause));
                    }
                },
                Err(cause) => {
                    // The apply died part-way through writing. Capture whatever it
                    // left so the persisted record names the real state, and so the
                    // mixed tree survives as a recoverable object in the store.
                    let observed = self.track_locked().ok().map(Capture::into_tree);
                    return Err(self.record_uncertain(restore, expected, target, observed, cause));
                }
            }
        };

        if after.tree() != target {
            // Only reachable when the transition had no files to apply, so nothing
            // was written and this is a clean invariant failure.
            return Err(SnapshotError::RestoreVerification {
                expected: target.to_owned(),
                actual: after.into_tree(),
            });
        }
        // What the restore could not move is what neither boundary captured, so the
        // report carries the checkpoint's own exclusions, widened by anything the
        // verifying capture had to leave out as well.
        let mut exclusions = checkpoint.exclusions().clone();
        exclusions.merge(after.exclusions().clone());
        Ok(TurnRestoreReport::new(
            restore, expected, target, files, exclusions,
        ))
    }

    /// The persisted uncertain-restore record for this store, if one exists.
    ///
    /// This is the authoritative-state inspection an uncertain outcome demands. A
    /// record that cannot be decoded is reported as a failure rather than ignored:
    /// unreadable recovery evidence is not the same as no incident.
    pub fn uncertain_restore(&self) -> Result<Option<UncertainRestore>> {
        let path = self.uncertain_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SnapshotError::Store {
                    operation: "read",
                    path,
                    source,
                });
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| SnapshotError::Store {
                operation: "parse",
                path,
                source: std::io::Error::other(error),
            })
    }

    /// Discard the persisted uncertain-restore record, unblocking further restores.
    ///
    /// Call this only once the worktree has actually been inspected. Returns whether
    /// a record was removed.
    pub fn resolve_uncertain_restore(&self) -> Result<bool> {
        let path = self.uncertain_path();
        match fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "resolved uncertain restore");
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SnapshotError::Store {
                operation: "remove",
                path,
                source,
            }),
        }
    }

    /// Where the uncertain-restore record lives: inside the store, never inside the
    /// user's repository.
    #[must_use]
    pub fn uncertain_path(&self) -> PathBuf {
        self.git_dir.join(UNCERTAIN_RESTORE_FILE)
    }

    fn track_locked(&self) -> Result<Capture> {
        let existed = self.git_dir.is_dir();
        self.create_dir(&self.git_dir)?;
        if !existed {
            self.init()?;
        }
        let exclusions = self.add()?;

        let mut argv = self.scoped(&[]);
        argv.push("write-tree");
        // `write-tree` has no pathspec: it always writes the whole index, so the
        // worktree root is the only cwd that describes what this produces.
        let output = self.run(&argv, &self.location.worktree, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }
        let hash = output.text(&argv.display())?.trim().to_owned();
        tracing::info!(hash = %hash, git_dir = %self.git_dir.display(), "tracking");
        Ok(Capture {
            tree: hash,
            exclusions,
        })
    }

    /// Persist what an interrupted transition left behind and classify it as
    /// uncertain.
    ///
    /// Existing evidence is never overwritten — the first incident is the one that
    /// explains the worktree. A failure to write the record cannot downgrade the
    /// outcome, so it is logged and the uncertain error is returned regardless.
    fn record_uncertain(
        &self,
        restore: TurnRestore,
        from: &str,
        to: &str,
        observed: Option<String>,
        cause: SnapshotError,
    ) -> SnapshotError {
        let path = self.uncertain_path();
        tracing::error!(
            restore = %restore,
            from,
            to,
            observed = ?observed,
            evidence = %path.display(),
            cause = %cause,
            "restore left the worktree in an uncertain state"
        );
        if !path.exists() {
            let record = UncertainRestore {
                restore,
                from: from.to_owned(),
                to: to.to_owned(),
                observed: observed.clone(),
                cause: cause.to_string(),
                recorded_unix_seconds: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|since| since.as_secs()),
            };
            if let Err(error) = self.write_uncertain(&path, &record) {
                tracing::error!(
                    evidence = %path.display(),
                    %error,
                    "failed to persist uncertain restore evidence"
                );
            }
        }
        SnapshotError::RestoreUncertain {
            restore,
            expected: to.to_owned(),
            actual: observed,
            evidence: path,
            source: Box::new(cause),
        }
    }

    /// Write the record through a sibling temporary file so a crash mid-write cannot
    /// leave a half-serialized record where a whole one is expected.
    fn write_uncertain(&self, path: &Path, record: &UncertainRestore) -> Result<()> {
        let text = serde_json::to_string_pretty(record).map_err(|error| SnapshotError::Store {
            operation: "write",
            path: path.to_path_buf(),
            source: std::io::Error::other(error),
        })?;
        let staging = path.with_extension("json.new");
        self.write(&staging, &format!("{text}\n"))?;
        fs::rename(&staging, path).map_err(|source| SnapshotError::Store {
            operation: "write",
            path: path.to_path_buf(),
            source,
        })
    }

    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        self.diff_paths(from, to, None)
    }

    fn transition_files(&self, from: &str, to: &str) -> Result<Vec<RestoredFile>> {
        let mut files = Vec::new();
        for (filter, operation) in [
            ("A", FileOperation::Created),
            ("MT", FileOperation::Modified),
            ("D", FileOperation::Deleted),
        ] {
            files.extend(
                self.diff_paths(from, to, Some(filter))?
                    .into_iter()
                    .map(|path| RestoredFile { path, operation }),
            );
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn diff_paths(&self, from: &str, to: &str, filter: Option<&str>) -> Result<Vec<String>> {
        let mut argv = self.scoped(QUOTE);
        argv.extend(["diff", "--name-only", "-z", "--no-renames"]);
        if let Some(filter) = filter {
            argv.push(format!("--diff-filter={filter}"));
        }
        argv.push(from).push(to).extend(["--", "."]);
        let output = self.run(&argv, &self.location.worktree, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }
        let mut files = git::split_nul(&output.text(&argv.display())?);
        files.sort();
        Ok(files)
    }

    fn transition_patch(&self, from: &str, to: &str) -> Result<Vec<u8>> {
        let mut argv = self.scoped(CFG);
        argv.extend([
            "diff",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-renames",
        ])
        .push(from)
        .push(to)
        .extend(["--", "."]);
        let output = self.run(&argv, &self.location.worktree, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }
        Ok(output.stdout)
    }

    fn apply_transition(&self, patch: &[u8], check: bool) -> Result<()> {
        let mut argv = self.scoped(CFG);
        argv.push("apply").push("--index").push("--binary");
        if check {
            argv.push("--check");
        }
        let output = self.run(&argv, &self.location.worktree, Some(patch))?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }
        Ok(())
    }

    /// The files that changed since `hash`, as absolute forward-slashed paths.
    ///
    /// The *capture* covers the whole worktree; this report is narrowed to
    /// [`Location::directory`].
    pub fn patch(&self, hash: &str) -> Result<Patch> {
        let _guard = lock::acquire(&self.git_dir);
        // Exclusions belong to the capture that produced a tree and reach callers
        // through `Store::capture`; this is a reporting view over one.
        drop(self.add()?);

        let mut argv = self.scoped(QUOTE);
        argv.extend(["diff", "--cached", "--no-ext-diff", "--name-only"])
            .push(hash)
            .extend(["--", "."]);
        let output = self.run(&argv, &self.location.directory, None)?;
        if !output.ok() {
            tracing::warn!(hash, code = ?output.code(), "failed to get diff");
            return Ok(Patch {
                hash: hash.to_owned(),
                files: Vec::new(),
            });
        }

        let files: Vec<String> = output
            .text(&argv.display())?
            .trim()
            .split('\n')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect();
        let ignored = self.ignore(&files)?;
        let worktree = self.location.worktree.to_string_lossy().into_owned();

        Ok(Patch {
            hash: hash.to_owned(),
            files: files
                .into_iter()
                .filter(|item| !ignored.contains(item))
                .map(|item| node_path::join(&worktree, &item).replace('\\', "/"))
                .collect(),
        })
    }

    /// The unified diff of the worktree against `hash`.
    pub fn diff(&self, hash: &str) -> Result<String> {
        let _guard = lock::acquire(&self.git_dir);
        drop(self.add()?);

        let mut argv = self.scoped(QUOTE);
        argv.extend(["diff", "--cached", "--no-ext-diff"])
            .push(hash)
            .extend(["--", "."]);
        let output = self.run(&argv, &self.location.worktree, None)?;
        if !output.ok() {
            tracing::warn!(hash, code = ?output.code(), stderr = %output.stderr, "failed to get diff");
            return Ok(String::new());
        }
        Ok(output.text(&argv.display())?.trim().to_owned())
    }

    /// Restore every file recorded in `snapshot` back into the worktree.
    ///
    /// `read-tree` followed by `checkout-index -a -f`, exactly as upstream: file
    /// *content* is restored, and files created after the snapshot are left alone.
    /// Deleting those is revert's job, not restore's.
    pub fn restore(&self, snapshot: &str) -> Result<()> {
        let _guard = lock::acquire(&self.git_dir);
        tracing::info!(snapshot, "restore");

        let mut read = self.scoped(CORE);
        read.push("read-tree").push(snapshot);
        let output = self.run(&read, &self.location.worktree, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &read.display(),
                output.status,
                output.stderr,
            ));
        }

        let mut checkout = self.scoped(CORE);
        checkout.extend(["checkout-index", "-a", "-f"]);
        let output = self.run(&checkout, &self.location.worktree, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &checkout.display(),
                output.status,
                output.stderr,
            ));
        }
        Ok(())
    }

    /// `git gc --prune=7.days` inside the store.
    ///
    /// Reclaims unreachable objects. It never removes the store directory itself —
    /// deciding that a whole store is dead is reference counting's job, and
    /// nothing in this method deletes a directory.
    pub fn gc(&self) -> Result<GcOutcome> {
        if !self.enabled() {
            return Ok(GcOutcome::Disabled);
        }
        if !self.git_dir.is_dir() {
            return Ok(GcOutcome::Missing);
        }
        let _guard = lock::acquire(&self.git_dir);

        let mut argv = self.scoped(&[]);
        argv.extend(GC_ARGS);
        let output = self.run(&argv, &self.location.directory, None)?;
        if !output.ok() {
            tracing::warn!(code = ?output.code(), stderr = %output.stderr, "cleanup failed");
            return Ok(GcOutcome::Failed {
                code: output.code(),
                stderr: output.stderr,
            });
        }
        tracing::info!(prune = PRUNE, "cleanup");
        Ok(GcOutcome::Collected)
    }

    // -- internals ----------------------------------------------------------

    /// `[...flags, "--git-dir", gitdir, "--work-tree", worktree]`, the prefix the
    /// oracle builds as `[...quote, ...args([...])]`.
    fn scoped(&self, flags: &[&str]) -> Argv {
        let mut argv = Argv::flags(flags);
        argv.push("--git-dir")
            .push(&self.git_dir)
            .push("--work-tree")
            .push(&self.location.worktree);
        argv
    }

    fn run(&self, argv: &Argv, cwd: &Path, stdin: Option<&[u8]>) -> Result<git::Output> {
        git::run(argv, cwd, &[], stdin)
    }

    fn create_dir(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path).map_err(|source| SnapshotError::Store {
            operation: "create",
            path: path.to_path_buf(),
            source,
        })
    }

    fn write(&self, path: &Path, contents: &str) -> Result<()> {
        fs::write(path, contents).map_err(|source| SnapshotError::Store {
            operation: "write",
            path: path.to_path_buf(),
            source,
        })
    }

    /// `git init` with `GIT_DIR`/`GIT_WORK_TREE`, then the eight `config` writes,
    /// then object-database seeding.
    fn init(&self) -> Result<()> {
        let mut argv = Argv::new();
        argv.push("init");
        let env: [(&OsStr, &OsStr); 2] = [
            (OsStr::new("GIT_DIR"), self.git_dir.as_os_str()),
            (
                OsStr::new("GIT_WORK_TREE"),
                self.location.worktree.as_os_str(),
            ),
        ];
        let output = git::run(&argv, &self.location.worktree, &env, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }

        for (key, value) in Self::INIT_CONFIG {
            let mut argv = Argv::new();
            argv.push("--git-dir")
                .push(&self.git_dir)
                .push("config")
                .push(key)
                .push(value);
            let output = self.run(&argv, &self.location.worktree, None)?;
            if !output.ok() {
                return Err(SnapshotError::git(
                    &argv.display(),
                    output.status,
                    output.stderr,
                ));
            }
        }

        self.seed()?;
        tracing::info!(git_dir = %self.git_dir.display(), "initialized");
        Ok(())
    }

    /// The store configuration written on first init, in oracle order
    /// (`index.ts:322-333`). The last four bound the first `add` on a very large
    /// worktree.
    const INIT_CONFIG: [(&'static str, &'static str); 8] = [
        ("core.autocrlf", "false"),
        ("core.longpaths", "true"),
        ("core.symlinks", "true"),
        ("core.fsmonitor", "false"),
        ("feature.manyFiles", "true"),
        ("index.version", "4"),
        ("index.threads", "true"),
        ("core.untrackedCache", "true"),
    ];

    /// Share the source repository's object database and index so blobs that are
    /// already hashed are not hashed again. On a very large checkout this is the
    /// difference between a bounded first `add` and minutes of rehashing.
    ///
    /// Entirely best-effort: every failure here degrades to a full rehash.
    fn seed(&self) -> Result<()> {
        if !self.location.git {
            return Ok(());
        }
        let Some(source) = self.rev_parse_path(&["--git-common-dir"])? else {
            return Ok(());
        };

        let source_objects = source.join("objects");
        let chained =
            fs::read_to_string(source_objects.join("info").join("alternates")).unwrap_or_default();
        let mut alternates: Vec<String> = Vec::new();
        for candidate in std::iter::once(source_objects.to_string_lossy().into_owned())
            .chain(chained.lines().map(|line| line.trim().to_owned()))
            .filter(|line| !line.is_empty())
        {
            if Path::new(&candidate).exists() {
                alternates.push(candidate);
            }
        }
        if alternates.is_empty() {
            return Ok(());
        }

        let info = self.git_dir.join("objects").join("info");
        self.create_dir(&info)?;
        self.write(
            &info.join("alternates"),
            &format!("{}\n", alternates.join("\n")),
        )?;

        let source_index = source.join("index");
        if source_index.is_file() {
            // A missing or incompatible index just falls back to a full add.
            let _ = fs::copy(&source_index, self.git_dir.join("index"));
        }
        Ok(())
    }

    /// `git rev-parse --path-format=absolute --git-path|--git-common-dir …`,
    /// returning `None` when Git declines or the path does not exist. The oracle
    /// reads `result.text.trim()` without checking the exit code, so a failure and
    /// an empty answer are the same thing.
    fn rev_parse_path(&self, tail: &[&str]) -> Result<Option<PathBuf>> {
        let mut argv = Argv::new();
        argv.push("rev-parse")
            .push("--path-format=absolute")
            .extend(tail);
        let output = self.run(&argv, &self.location.worktree, None)?;
        if !output.ok() {
            return Ok(None);
        }
        let text = output.text(&argv.display())?;
        let path = PathBuf::from(text.trim());
        if text.trim().is_empty() || !path.exists() {
            return Ok(None);
        }
        Ok(Some(path))
    }

    /// Mirror the source repository's `info/exclude` into the store, plus one
    /// `/`-anchored entry per path in `extra`.
    fn sync(&self, extra: &[String]) -> Result<()> {
        let source = self.rev_parse_path(&["--git-path", "info/exclude"])?;
        let mut parts: Vec<String> = Vec::new();
        if let Some(file) = source {
            let text = fs::read_to_string(&file).unwrap_or_default();
            let trimmed = text.trim_end();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_owned());
            }
        }
        for item in extra {
            parts.push(format!("/{}", item.replace('\\', "/")));
        }

        let info = self.git_dir.join("info");
        self.create_dir(&info)?;
        let text = parts.join("\n");
        let contents = if text.is_empty() {
            String::new()
        } else {
            format!("{text}\n")
        };
        self.write(&info.join("exclude"), &contents)
    }

    /// Which of `files` the *source* repository ignores.
    ///
    /// Resolved against the source repo's own `.git`, with `--no-index` so the
    /// answer stays pattern-based even for a path Git already tracks. Exit code 1
    /// means "none matched" and is not a failure.
    fn ignore(&self, files: &[String]) -> Result<HashSet<String>> {
        if files.is_empty() {
            return Ok(HashSet::new());
        }
        // `check-ignore` reads a leading colon as pathspec magic but accepts and
        // echoes back a protective `./` prefix.
        let probes: Vec<String> = files
            .iter()
            .map(|item| {
                if item.starts_with(':') {
                    format!("./{item}")
                } else {
                    item.clone()
                }
            })
            .collect();

        let mut argv = Argv::flags(QUOTE);
        argv.push("--git-dir")
            .push(self.location.worktree.join(".git"))
            .push("--work-tree")
            .push(&self.location.worktree)
            .extend(["check-ignore", "--no-index", "--stdin", "-z"]);
        let output = self.run(
            &argv,
            &self.location.worktree,
            Some(&git::nul_terminated(&probes)),
        )?;
        if !output.ok() && output.code() != Some(1) {
            return Ok(HashSet::new());
        }

        Ok(git::split_nul(&output.text(&argv.display())?)
            .into_iter()
            .map(|item| {
                if item.starts_with("./:") {
                    item[2..].to_owned()
                } else {
                    item
                }
            })
            .collect())
    }

    /// Drop newly-ignored paths from the store's index so they are not re-added.
    ///
    /// `--ignore-unmatch` already makes "no such index entry" exit zero (verified on
    /// git 2.43.0), so a non-zero exit is a real failure. Tolerating it would leave
    /// the next `write-tree` describing a tree that contains a path the user's ignore
    /// rules put out of bounds.
    fn drop_cached(&self, files: &[String]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut argv = self.scoped(CFG);
        argv.extend([
            "rm",
            "--cached",
            "-f",
            "--ignore-unmatch",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]);
        let output = self.run(
            &argv,
            &self.location.worktree,
            Some(&git::top_level_literal_pathspecs(files)),
        )?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }
        Ok(())
    }

    /// Stage exactly `files`, and report which of them Git could not read.
    ///
    /// # Why this may not tolerate a failed `add`
    ///
    /// `git add` writes its index once, at the end. Without `--ignore-errors` a
    /// single unreadable path aborts the whole command with exit 128 and the index is
    /// never written, so *nothing* is staged — not the unreadable file, and not the
    /// files the agent actually edited. A tolerated failure therefore does not cost
    /// one file; it makes the following `write-tree` hand back the previous tree
    /// while the caller believes it captured the current one, and a later `/undo`
    /// restores the wrong content or reports no changes.
    ///
    /// So staging asks for exactly the behaviour the tolerance was pretending to
    /// have: `--ignore-errors` skips the paths it cannot index and stages the rest,
    /// reporting "some paths were skipped" as exit **1** while a genuine failure keeps
    /// its own code. Verified on git 2.43.0: mode-000 file present, without the flag
    /// exit 128 and the tree equals the baseline; with it exit 1 and the tree contains
    /// every readable edit.
    fn stage(&self, files: &[String]) -> Result<Vec<String>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut argv = self.scoped(CFG);
        argv.extend([
            "add",
            "--all",
            "--sparse",
            "--ignore-errors",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]);
        let output = self.run(
            &argv,
            &self.location.worktree,
            Some(&git::top_level_literal_pathspecs(files)),
        )?;
        match output.code() {
            Some(0) => Ok(Vec::new()),
            Some(1) => {
                // Which paths were skipped is read back from the index rather than
                // parsed out of `stderr`: git's per-path error text is translated,
                // and the index is authoritative in every locale.
                let skipped = self.unstaged_among(files)?;
                tracing::warn!(
                    count = skipped.len(),
                    stderr = %output.stderr,
                    "git add skipped unreadable snapshot paths"
                );
                Ok(skipped)
            }
            _ => Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            )),
        }
    }

    /// Which of `requested` still differ from the index after staging them.
    fn unstaged_among(&self, requested: &[String]) -> Result<Vec<String>> {
        let requested: HashSet<&String> = requested.iter().collect();
        let (tracked, untracked) = self.list_changes()?;
        let mut skipped: Vec<String> = tracked
            .into_iter()
            .chain(untracked)
            .filter(|path| requested.contains(path))
            .collect();
        skipped.sort();
        skipped.dedup();
        Ok(skipped)
    }

    /// The worktree paths that differ from the store's index, and the ones the store
    /// has never seen.
    ///
    /// # Why this is scoped to the worktree root
    ///
    /// `write-tree` takes no pathspec, so a capture is always a *whole-worktree*
    /// tree, and [`Store::restore_turn`] diffs and applies across the whole worktree
    /// too. Listing only the startup directory therefore did not produce a smaller
    /// snapshot — it produced a whole-worktree tree that silently omitted every
    /// change outside that directory, which is the same class of bug as a tolerated
    /// `git add`. Both halves now use the root, so the tree means what the restore
    /// path assumes it means.
    fn list_changes(&self) -> Result<(Vec<String>, Vec<String>)> {
        let mut modified = self.scoped(QUOTE);
        modified.extend(["diff-files", "--name-only", "-z", "--", "."]);
        let modified_out = self.run(&modified, &self.location.worktree, None)?;
        if !modified_out.ok() {
            return Err(SnapshotError::git(
                &modified.display(),
                modified_out.status,
                modified_out.stderr,
            ));
        }

        let mut others = self.scoped(QUOTE);
        others.extend([
            "ls-files",
            "--full-name",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ]);
        let others_out = self.run(&others, &self.location.worktree, None)?;
        if !others_out.ok() {
            return Err(SnapshotError::git(
                &others.display(),
                others_out.status,
                others_out.stderr,
            ));
        }

        Ok((
            git::split_nul(&modified_out.text(&modified.display())?),
            git::split_nul(&others_out.text(&others.display())?),
        ))
    }

    /// What the next `add` will stage, and what it will leave out.
    fn plan(&self) -> Result<StagePlan> {
        self.sync(&[])?;
        let (tracked, untracked) = self.list_changes()?;
        let untracked_set: HashSet<&String> = untracked.iter().collect();

        let mut seen = HashSet::new();
        let all: Vec<String> = tracked
            .iter()
            .chain(untracked.iter())
            .filter(|item| seen.insert((*item).clone()))
            .cloned()
            .collect();
        if all.is_empty() {
            return Ok(StagePlan::default());
        }

        let ignored_set = self.ignore(&all)?;
        let mut ignored: Vec<String> = all
            .iter()
            .filter(|item| ignored_set.contains(*item))
            .cloned()
            .collect();
        ignored.sort();
        if !ignored.is_empty() {
            tracing::info!(
                count = ignored.len(),
                "removing gitignored files from snapshot"
            );
            self.drop_cached(&ignored)?;
        }

        let allow: Vec<String> = all
            .into_iter()
            .filter(|item| !ignored_set.contains(item))
            .collect();

        // An untracked file over the limit is excluded rather than stored, so a
        // stray multi-gigabyte artifact cannot bloat the object database.
        let mut oversized: Vec<String> = allow
            .iter()
            .filter(|item| untracked_set.contains(*item) && self.is_large(item))
            .cloned()
            .collect();
        oversized.sort();
        self.sync(&oversized)?;

        let blocked: HashSet<&String> = oversized.iter().collect();
        let stage: Vec<String> = allow
            .iter()
            .filter(|item| !blocked.contains(*item))
            .cloned()
            .collect();
        Ok(StagePlan {
            stage,
            exclusions: CaptureExclusions {
                ignored,
                oversized,
                unreadable: Vec::new(),
            },
        })
    }

    /// Bring the store's index up to date with the worktree, reporting what the
    /// resulting tree does not contain.
    fn add(&self) -> Result<CaptureExclusions> {
        let mut plan = self.plan()?;
        let skipped = match self.stage(&plan.stage) {
            Ok(skipped) => skipped,
            Err(first) => {
                // A path listed a moment ago can vanish before `git add` reaches it —
                // a build artifact, an editor temp file. `git add` then dies on the
                // unmatched pathspec even under `--ignore-errors` (git 2.43.0). That
                // is a stale plan, not a broken worktree, so the plan is rebuilt once
                // from authoritative state; staging is idempotent, so replaying it is
                // safe. If nothing moved, the failure is real and propagates.
                let retry = self.plan()?;
                if retry.stage == plan.stage {
                    return Err(first);
                }
                tracing::warn!(
                    error = %first,
                    "retrying snapshot staging after the worktree changed underneath it"
                );
                plan = retry;
                self.stage(&plan.stage)?
            }
        };
        let mut exclusions = plan.exclusions;
        merge_paths(&mut exclusions.unreadable, skipped);
        Ok(exclusions)
    }

    fn is_large(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.location.worktree.join(relative))
            .ok()
            .is_some_and(|meta| meta.is_file() && meta.len() > LARGE_FILE_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location() -> Location {
        Location::new("/data/snapshot", "proj", "/it's a/work tree")
    }

    #[test]
    fn the_store_path_is_project_id_then_worktree_hash() {
        let store = Store::open(location());
        let expected =
            Path::new("/data/snapshot")
                .join("proj")
                .join(zuno_paths::Layout::worktree_hash(Path::new(
                    "/it's a/work tree",
                )));
        assert_eq!(store.git_dir(), expected);
    }

    #[test]
    fn scoped_puts_config_flags_before_the_git_dir() {
        let store = Store::open(location());
        let rendered = store.scoped(QUOTE).display();
        assert_eq!(&rendered[..QUOTE.len()], QUOTE);
        assert_eq!(rendered[QUOTE.len()], "--git-dir");
        assert_eq!(rendered[QUOTE.len() + 2], "--work-tree");
        assert_eq!(rendered[QUOTE.len() + 3], "/it's a/work tree");
    }

    #[test]
    fn disabled_locations_do_not_track_or_gc() {
        let store = Store::open(location().with_enabled(false));
        assert_eq!(store.track().expect("track"), None);
        assert_eq!(store.gc().expect("gc"), GcOutcome::Disabled);

        let store = Store::open(location().with_git(false));
        assert!(!store.enabled());
        assert_eq!(store.track().expect("track"), None);
    }

    #[test]
    fn gc_on_a_missing_store_is_a_no_op() {
        let store = Store::open(location());
        assert_eq!(store.gc().expect("gc"), GcOutcome::Missing);
        assert!(!store.exists());
    }

    #[test]
    fn oracle_gc_parameters_are_carried_verbatim() {
        assert_eq!(PRUNE, "7.days");
        assert_eq!(GC_ARGS, ["gc", format!("--prune={PRUNE}").as_str()]);
        assert_eq!(LARGE_FILE_LIMIT, 2 * 1024 * 1024);
    }

    #[test]
    fn init_config_matches_the_oracle_list_and_order() {
        assert_eq!(
            Store::INIT_CONFIG.map(|(key, _)| key),
            [
                "core.autocrlf",
                "core.longpaths",
                "core.symlinks",
                "core.fsmonitor",
                "feature.manyFiles",
                "index.version",
                "index.threads",
                "core.untrackedCache",
            ]
        );
    }
}
