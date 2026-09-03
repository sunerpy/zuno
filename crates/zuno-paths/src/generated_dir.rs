//! The one creator of a generated directory inside a worktree, and the exclusion it
//! writes in the same call.
//!
//! # Why the directory excludes itself
//!
//! [`crate::ensure_managed_block`] writes the repository-private exclude file, once per
//! host, from a list of patterns. That is the right mechanism and it is not enough on
//! its own. `info/exclude` only suppresses an *untracked* path, so once a generated
//! directory has been committed — force-added, or committed by a release that did not
//! exclude it yet — the block has no effect on it ever again. The call can also fail
//! for ordinary reasons: no git on the machine, a directory that is not a repository
//! yet, an ownership check git refuses. Every one of those is reported as a note and
//! the session continues, which is right, and leaves the writing unguarded.
//!
//! So the guard is moved to the only moment that cannot be skipped: the call that
//! creates the directory. [`GeneratedDirectory::ensure`] creates
//! `<worktree>/.zuno/<name>/` and publishes `<worktree>/.zuno/<name>/.gitignore`
//! containing `*` in the same call. From then on git hides the contents no matter how
//! a commit is spelled — an alias, a script the analyzer cannot see into, a Makefile
//! target, `git add -A` — and no matter whether the exclude block was ever written.
//! The two mechanisms are independent on purpose: the block keeps the directory out of
//! `git status` even before anything is written into it, and this file keeps its
//! contents out of a commit even when the block is gone.
//!
//! This is a new file in a directory Zuno itself generates, not an edit to the user's
//! own `.gitignore`. [`crate::exclude`] explains why appending to a tracked file the
//! repository's history owns is not Zuno's to do; that reasoning does not reach a file
//! inside a directory that exists only because Zuno wrote it. Because the pattern
//! matches the file itself, `git status` does not report it either.
//!
//! # Why the root is the worktree and not the session directory
//!
//! A session can start anywhere inside a checkout. The exclude patterns are anchored at
//! the worktree root — `.zuno/*` in `info/exclude` is read relative to it — and so is
//! [`crate::generated::classify`]. A writer that joined `.zuno/` onto the session's own
//! directory therefore produced `<worktree>/sub/.zuno/tool-output/`, which no pattern
//! covers and which the delivery check does not recognise: exactly the state a session
//! started in a subdirectory used to commit. [`GeneratedDirectory::resolve`] resolves
//! the root through the very function [`crate::project::resolve_project`] uses, so the
//! directory that gets created and the pattern that hides it agree by construction.
//!
//! Only the generated root moves. Nothing here changes a sandbox boundary, a default
//! working directory, or the directory a relative `workdir` resolves against: those are
//! authorization surfaces that belong to the session's directory, not to its
//! repository.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::generated::GeneratedPath;
use crate::project::worktree_root;

/// What a generated directory's own `.gitignore` contains.
///
/// `*` matches every entry in the directory, including this file, so the exclusion is
/// complete and invisible at once. The comment is for whoever finds the file and wants
/// to know what wrote it.
///
/// LF on every platform. A git pattern is compared with the newline stripped either
/// way, so CRLF would change nothing about the matching and would make the file differ
/// between the machines sharing one checkout, which is a rewrite on every call for no
/// gain.
pub const SELF_EXCLUDE_CONTENT: &str = "\
# Written by Zuno. This directory holds generated working state, not source, so its
# contents stay out of git even when the repository-private exclude block does not.
*
";

/// The filename git reads a directory's own exclusions from.
pub const SELF_EXCLUDE_FILE: &str = ".gitignore";

/// One of Zuno's generated directories, rooted at the worktree that owns it.
///
/// Constructed cheaply and created lazily: nothing touches the filesystem until
/// [`GeneratedDirectory::ensure`] is called, which is the rule every other path getter
/// in this crate follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedDirectory {
    path: PathBuf,
    generated: &'static GeneratedPath,
}

impl GeneratedDirectory {
    /// The directory for `generated` in the worktree containing `directory`.
    ///
    /// `directory` is a session's working directory, anywhere inside the checkout. The
    /// root comes from [`worktree_root`], which resolves it exactly as
    /// [`crate::project::resolve_project`] does — including rejecting a `.git` that git
    /// itself rejects. It spawns git, so resolve when a service is opened rather than
    /// per write. A `directory` in no repository keeps its own place: there is no
    /// worktree to root against, and a session outside version control still needs
    /// somewhere to write.
    #[must_use]
    pub fn resolve(directory: &Path, generated: &'static GeneratedPath) -> Self {
        let root = worktree_root(directory).unwrap_or_else(|| directory.to_path_buf());
        Self::in_worktree(&root, generated)
    }

    /// The directory for `generated` in `worktree`, for a caller that already resolved
    /// the worktree root.
    #[must_use]
    pub fn in_worktree(worktree: &Path, generated: &'static GeneratedPath) -> Self {
        let path = generated
            .segments()
            .fold(worktree.to_path_buf(), |path, segment| path.join(segment));
        Self { path, generated }
    }

    /// Where the directory is, whether or not it exists yet.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The registry entry this directory holds.
    #[must_use]
    pub fn generated(&self) -> &'static GeneratedPath {
        self.generated
    }

    /// Create the directory and its own `.gitignore`, and report where it is.
    ///
    /// Idempotent, and cheap to repeat: the file is read first and rewritten only when
    /// its contents differ, so a caller may call this before every write. A user who
    /// edits the file gets it back on the next write, because what it excludes is not
    /// theirs to relax — the alternative is a session quietly committing its own
    /// scratch state again.
    ///
    /// The `.gitignore` is published through a same-directory write-then-rename, so an
    /// interrupted write cannot leave a half-written pattern that excludes nothing.
    ///
    /// # Errors
    ///
    /// [`GeneratedDirectoryError`] naming the path that failed, when the directory
    /// cannot be created or the exclusion cannot be written. A caller that cannot
    /// proceed without the directory must treat this as fatal: continuing would write
    /// generated state into a place nothing excludes.
    pub fn ensure(&self) -> Result<&Path, GeneratedDirectoryError> {
        fs::create_dir_all(&self.path).map_err(|source| GeneratedDirectoryError {
            path: self.path.clone(),
            source,
        })?;
        let marker = self.path.join(SELF_EXCLUDE_FILE);
        match fs::read(&marker) {
            Ok(current) if current == SELF_EXCLUDE_CONTENT.as_bytes() => return Ok(&self.path),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GeneratedDirectoryError {
                    path: marker,
                    source,
                });
            }
        }
        crate::exclude::replace_atomically(&marker, SELF_EXCLUDE_CONTENT.as_bytes())
            .map_err(|(path, source)| GeneratedDirectoryError { path, source })?;
        Ok(&self.path)
    }
}

/// A generated directory could not be created, or could not be made to exclude itself.
#[derive(Debug)]
pub struct GeneratedDirectoryError {
    /// The directory or the exclusion file the failure is about.
    pub path: PathBuf,
    /// What the filesystem reported.
    pub source: io::Error,
}

impl fmt::Display for GeneratedDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "could not prepare Zuno's generated directory {}: {}",
            crate::display_path(&self.path),
            self.source
        )
    }
}

impl std::error::Error for GeneratedDirectoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::{GOAL_PROJECTION, TOOL_OUTPUT, is_generated};
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> Option<String> {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let output = match Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", null_device)
            .env("GIT_CONFIG_SYSTEM", null_device)
            .env("GIT_AUTHOR_NAME", "zuno-paths")
            .env("GIT_AUTHOR_EMAIL", "zuno-paths@example.test")
            .env("GIT_COMMITTER_NAME", "zuno-paths")
            .env("GIT_COMMITTER_EMAIL", "zuno-paths@example.test")
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => panic!("spawn git {args:?}: {error}"),
        };
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(String::from_utf8(output.stdout).expect("git output is UTF-8"))
    }

    #[test]
    fn the_path_is_the_pattern_joined_onto_the_worktree() {
        let directory = GeneratedDirectory::in_worktree(Path::new("/repo"), &TOOL_OUTPUT);

        assert_eq!(
            directory.path(),
            Path::new("/repo").join(".zuno").join("tool-output")
        );
        assert!(
            is_generated(Path::new("/repo"), directory.path()),
            "the directory the writer creates must be the one the classifier hides"
        );
        assert!(!directory.path().exists(), "constructing must not create");
    }

    #[test]
    fn creating_the_directory_writes_the_exclusion_that_hides_it_from_git() {
        let root = tempfile::tempdir().expect("tempdir");
        let directory = GeneratedDirectory::in_worktree(root.path(), &GOAL_PROJECTION);

        let created = directory.ensure().expect("create the generated directory");

        assert!(created.is_dir());
        let marker = created.join(SELF_EXCLUDE_FILE);
        let content = std::fs::read(&marker).expect("read the exclusion");
        assert_eq!(content, SELF_EXCLUDE_CONTENT.as_bytes());
        assert!(
            !content.contains(&b'\r'),
            "the exclusion is LF on every platform so one checkout shared between \
             machines does not rewrite it"
        );
        assert!(
            SELF_EXCLUDE_CONTENT.lines().any(|line| line == "*"),
            "the pattern has to be a bare `*`, which also matches the file itself"
        );
    }

    #[test]
    fn ensuring_twice_leaves_the_exclusion_untouched() {
        let root = tempfile::tempdir().expect("tempdir");
        let directory = GeneratedDirectory::in_worktree(root.path(), &GOAL_PROJECTION);
        directory.ensure().expect("first");
        let marker = directory.path().join(SELF_EXCLUDE_FILE);
        let first = std::fs::metadata(&marker)
            .expect("metadata")
            .modified()
            .ok();

        directory.ensure().expect("second");

        assert_eq!(
            std::fs::metadata(&marker)
                .expect("metadata")
                .modified()
                .ok(),
            first,
            "an unchanged exclusion must not be rewritten on every write"
        );
    }

    /// The exclusion is Zuno's, and a session that cannot keep its own scratch state
    /// out of git is the bug this file exists to prevent.
    #[test]
    fn an_edited_exclusion_is_restored() {
        let root = tempfile::tempdir().expect("tempdir");
        let directory = GeneratedDirectory::in_worktree(root.path(), &GOAL_PROJECTION);
        directory.ensure().expect("first");
        let marker = directory.path().join(SELF_EXCLUDE_FILE);
        std::fs::write(&marker, "# mine\n").expect("edit the exclusion");

        directory.ensure().expect("second");

        assert_eq!(
            std::fs::read_to_string(&marker).expect("read"),
            SELF_EXCLUDE_CONTENT
        );
    }

    /// The property the whole module is for: with no exclude block anywhere — no
    /// `info/exclude`, no `.gitignore` at the root — git still reports nothing.
    #[test]
    fn git_reports_nothing_for_a_generated_directory_with_no_exclude_block_at_all() {
        let root = tempfile::tempdir().expect("tempdir");
        if git(root.path(), &["init", "--initial-branch=main", "."]).is_none() {
            eprintln!("skipping: git is not installed, so only git can say what git reports");
            return;
        }
        std::fs::write(root.path().join("file.txt"), "hello\n").expect("write");
        git(root.path(), &["add", "file.txt"]).expect("git add");
        git(root.path(), &["commit", "-m", "initial"]).expect("git commit");

        let directory = GeneratedDirectory::in_worktree(root.path(), &TOOL_OUTPUT);
        let created = directory.ensure().expect("create the generated directory");
        std::fs::write(created.join("tool_ses_1_01"), "spilled\n").expect("write output");

        let status = git(
            root.path(),
            &["status", "--porcelain", "--untracked-files=all"],
        )
        .expect("git status");
        assert_eq!(
            status, "",
            "the directory's own exclusion must hold without any block"
        );
        assert_eq!(
            git(
                root.path(),
                &["check-ignore", "-v", ".zuno/tool-output/tool_ses_1_01"]
            )
            .expect("git check-ignore")
            .lines()
            .next()
            .map(|line| line.contains(".zuno/tool-output/.gitignore")),
            Some(true),
            "and the rule that hides it must be the one this module wrote"
        );
    }

    /// A session started in a subdirectory must write into the worktree's project
    /// directory, because that is where every pattern and the delivery check look.
    #[test]
    fn a_session_in_a_subdirectory_roots_its_generated_state_at_the_worktree() {
        let root = tempfile::tempdir().expect("tempdir");
        if git(root.path(), &["init", "--initial-branch=main", "."]).is_none() {
            eprintln!("skipping: git is not installed, so the worktree cannot be resolved");
            return;
        }
        let nested = root.path().join("crates").join("deep");
        std::fs::create_dir_all(&nested).expect("create the session directory");

        let directory = GeneratedDirectory::resolve(&nested, &TOOL_OUTPUT);

        // The temporary directory may itself be reached through a symbolic link, which
        // git resolves; compare the tail rather than the whole path.
        assert!(
            directory
                .path()
                .ends_with(Path::new(".zuno").join("tool-output")),
            "{}",
            directory.path().display()
        );
        assert!(
            !directory.path().starts_with(&nested),
            "the session's own directory is not the root: {}",
            directory.path().display()
        );
        let worktree = directory
            .path()
            .parent()
            .and_then(Path::parent)
            .expect("the worktree root");
        assert!(
            is_generated(worktree, directory.path()),
            "the resolved directory must be generated state under the root it resolved to"
        );
    }

    /// Outside a repository there is no worktree to root against, and a session there
    /// still has to be able to write.
    #[test]
    fn a_directory_outside_a_repository_keeps_its_own_place() {
        let root = tempfile::tempdir().expect("tempdir");

        let directory = GeneratedDirectory::resolve(root.path(), &GOAL_PROJECTION);

        assert_eq!(
            directory.path(),
            root.path().join(".zuno").join("goal"),
            "no repository means no root above this directory"
        );
    }

    #[test]
    fn a_failure_names_the_path_and_keeps_the_cause() {
        let root = tempfile::tempdir().expect("tempdir");
        let blocked = root.path().join("blocked");
        std::fs::write(&blocked, "not a directory\n").expect("write");
        let directory = GeneratedDirectory::in_worktree(&blocked, &GOAL_PROJECTION);

        let error = directory
            .ensure()
            .expect_err("a file cannot hold a directory");

        assert!(error.path.starts_with(&blocked), "{}", error.path.display());
        assert!(error.to_string().contains("generated directory"), "{error}");
        assert!(std::error::Error::source(&error).is_some());
    }
}
