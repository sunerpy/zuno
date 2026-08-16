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
//! the cause. Failures the oracle tolerates *by design* — a partially unreadable
//! `git add`, a failed `diff`, a failed `gc` — stay tolerated.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use zuno_paths::node_path;

use crate::error::{Result, SnapshotError};
use crate::git::{self, Argv, CFG, CORE, QUOTE};
use crate::lock;
use crate::refcount::StoreKey;

/// `prune` in `packages/opencode/src/snapshot/index.ts:23` — the argument to
/// `git gc --prune=`.
pub const PRUNE: &str = "7.days";

/// `limit` in `packages/opencode/src/snapshot/index.ts:24` — untracked files
/// larger than this are excluded instead of stored.
pub const LARGE_FILE_LIMIT: u64 = 2 * 1024 * 1024;

/// The `git gc` argument vector the hourly schedule runs, for assertions.
pub const GC_ARGS: [&str; 2] = ["gc", "--prune=7.days"];

/// A snapshot patch: the tree it was taken against and the absolute paths that
/// changed since. Mirrors the oracle's `Patch` schema.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Patch {
    /// The tree hash the diff was taken against.
    pub hash: String,
    /// Absolute, forward-slashed paths of the changed files.
    pub files: Vec<String>,
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
    /// The worktree root. Hashed to form the second path component.
    pub worktree: PathBuf,
    /// The directory operations are scoped to; a subdirectory of the worktree, or
    /// the worktree itself.
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

    /// Scope operations to a subdirectory of the worktree.
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
    pub fn track(&self) -> Result<Option<String>> {
        if !self.enabled() {
            return Ok(None);
        }
        let _guard = lock::acquire(&self.git_dir);

        let existed = self.git_dir.is_dir();
        self.create_dir(&self.git_dir)?;
        if !existed {
            self.init()?;
        }
        self.add()?;

        let mut argv = self.scoped(&[]);
        argv.push("write-tree");
        let output = self.run(&argv, &self.location.directory, None)?;
        if !output.ok() {
            return Err(SnapshotError::git(
                &argv.display(),
                output.status,
                output.stderr,
            ));
        }
        let hash = output.text(&argv.display())?.trim().to_owned();
        tracing::info!(hash = %hash, git_dir = %self.git_dir.display(), "tracking");
        Ok(Some(hash))
    }

    /// The files that changed since `hash`, as absolute forward-slashed paths.
    pub fn patch(&self, hash: &str) -> Result<Patch> {
        let _guard = lock::acquire(&self.git_dir);
        self.add()?;

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
        self.add()?;

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
            tracing::warn!(code = ?output.code(), stderr = %output.stderr, "failed to drop snapshot files");
        }
        Ok(())
    }

    /// Stage exactly `files`. A partial failure is logged and tolerated, matching
    /// the oracle: one unreadable file must not cost the whole snapshot.
    fn stage(&self, files: &[String]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut argv = self.scoped(CFG);
        argv.extend([
            "add",
            "--all",
            "--sparse",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]);
        let output = self.run(
            &argv,
            &self.location.worktree,
            Some(&git::top_level_literal_pathspecs(files)),
        )?;
        if !output.ok() {
            tracing::warn!(code = ?output.code(), stderr = %output.stderr, "failed to add snapshot files");
        }
        Ok(())
    }

    /// Bring the store's index up to date with the worktree.
    fn add(&self) -> Result<()> {
        self.sync(&[])?;

        let mut modified = self.scoped(QUOTE);
        modified.extend(["diff-files", "--name-only", "-z", "--", "."]);
        let modified_out = self.run(&modified, &self.location.directory, None)?;

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
        let others_out = self.run(&others, &self.location.directory, None)?;

        if !modified_out.ok() || !others_out.ok() {
            tracing::warn!(
                modified = ?modified_out.code(),
                others = ?others_out.code(),
                "failed to list snapshot files"
            );
            return Ok(());
        }

        let tracked = git::split_nul(&modified_out.text(&modified.display())?);
        let untracked = git::split_nul(&others_out.text(&others.display())?);
        let untracked_set: HashSet<&String> = untracked.iter().collect();

        let mut seen = HashSet::new();
        let all: Vec<String> = tracked
            .iter()
            .chain(untracked.iter())
            .filter(|item| seen.insert((*item).clone()))
            .cloned()
            .collect();
        if all.is_empty() {
            return Ok(());
        }

        let ignored = self.ignore(&all)?;
        if !ignored.is_empty() {
            let drop: Vec<String> = all
                .iter()
                .filter(|item| ignored.contains(*item))
                .cloned()
                .collect();
            tracing::info!(
                count = drop.len(),
                "removing gitignored files from snapshot"
            );
            self.drop_cached(&drop)?;
        }

        let allow: Vec<String> = all
            .into_iter()
            .filter(|item| !ignored.contains(item))
            .collect();
        if allow.is_empty() {
            return Ok(());
        }

        // An untracked file over the limit is excluded rather than stored, so a
        // stray multi-gigabyte artifact cannot bloat the object database.
        let block: Vec<String> = allow
            .iter()
            .filter(|item| untracked_set.contains(*item) && self.is_large(item))
            .cloned()
            .collect();
        self.sync(&block)?;

        let block: HashSet<&String> = block.iter().collect();
        let stage: Vec<String> = allow
            .iter()
            .filter(|item| !block.contains(*item))
            .cloned()
            .collect();
        self.stage(&stage)
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
