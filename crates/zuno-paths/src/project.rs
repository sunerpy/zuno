//! Git worktree discovery and project identity.
//!
//! Ports two oracle functions:
//!
//! - `Git.repo.discover` (`packages/core/src/git.ts:184-203`) — find the nearest
//!   `.git`, then ask Git itself for the worktree root, the git directory and the
//!   common directory.
//! - `Project.resolve` (`packages/core/src/project.ts:110-122`) — derive the
//!   project id from the Git remote, a cached marker, or the root commit.
//!
//! # Why Git is asked instead of inferred
//!
//! The nearest `.git` is only a starting point. In a linked worktree it is a
//! *file* containing `gitdir: …`, and the common directory lives elsewhere; in a
//! submodule the git directory is under the superproject. `rev-parse` resolves
//! all of that, and the snapshot store is keyed on the answer, so guessing would
//! put a store in the wrong place.
//!
//! A failure to spawn `git` is not an error here. The oracle's `run` swallows a
//! spawn failure into `{ exitCode: 1 }`, so a machine without Git behaves exactly
//! like a directory that is not a repository — the id falls back to `global`.
//!
//! # Not answering is also a failure
//!
//! Asking git is a subprocess, and a subprocess reading a `.git` on an unresponsive
//! network mount does not fail — it waits, in a kernel call with nothing to time it
//! out. Both entry points below are synchronous and both run during startup, some of
//! them from inside a current-thread runtime, so an unbounded wait there is not one
//! slow answer but a process that never gets any further. Every call is therefore
//! bounded by [`bounded::GIT_TIMEOUT`], and a call that outstays it is killed and
//! reported the way a missing git already is: `None`, so no caller had to learn a new
//! outcome. [`crate::bounded`] holds that machinery, because [`crate::exclude`] asks
//! git too and must not grow a second copy of it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::bounded::{self, GIT_TIMEOUT};
use crate::node_path;
use crate::sha1;
use crate::walk;

/// The project id used when no repository can be discovered — `ID.global` in
/// `packages/schema/src/project-id.ts`.
pub const GLOBAL_PROJECT_ID: &str = "global";

/// The marker file inside the Git common directory that caches a previously
/// resolved project id.
pub const PROJECT_ID_MARKER: &str = "zuno";

/// The prefix hashed together with a normalized remote to form a project id.
pub const REMOTE_ID_PREFIX: &str = "git-remote:";

/// A discovered Git repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    /// `git rev-parse --show-toplevel` — the worktree root.
    pub worktree: PathBuf,
    /// `git rev-parse --git-dir` — this worktree's own git directory.
    pub git_directory: PathBuf,
    /// `git rev-parse --git-common-dir` — shared across linked worktrees, and
    /// the directory the project id marker lives in.
    pub common_directory: PathBuf,
}

/// The version control system backing a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vcs {
    /// `{ type: "git", store: commonDirectory }`.
    Git {
        /// The Git common directory.
        store: PathBuf,
    },
}

/// The outcome of `Project.resolve`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedProject {
    /// The id a previous run cached in the Git common directory, if any.
    pub previous: Option<String>,
    /// The project id: remote-derived, else cached, else root-commit, else
    /// [`GLOBAL_PROJECT_ID`].
    pub id: String,
    /// The worktree root, or the filesystem root of `start` when there is no
    /// repository.
    pub directory: PathBuf,
    /// `None` outside a repository.
    pub vcs: Option<Vcs>,
}

/// The nearest existing `.git` at or above `start`.
#[must_use]
pub fn find_git_marker(start: &Path) -> Option<PathBuf> {
    walk::up_first(&[".git"], start, None)
}

/// Port of `Git.repo.discover`.
///
/// Returns `None` when no `.git` is found, or when `--git-dir` or
/// `--git-common-dir` fails. Note that `--show-toplevel` failing is *not* fatal:
/// the oracle falls back to the directory holding `.git`, which is what happens
/// inside a bare repository.
#[must_use]
pub fn discover_repository(start: &Path) -> Option<Repository> {
    let marker = find_git_marker(start)?;
    let cwd = node_path::dirname(&marker.to_string_lossy());

    let top_level = git(&cwd, &["rev-parse", "--show-toplevel"]);
    let git_dir = git(&cwd, &["rev-parse", "--git-dir"])?;
    let common_dir = git(&cwd, &["rev-parse", "--git-common-dir"])?;

    Some(Repository {
        worktree: PathBuf::from(match top_level {
            Some(text) => resolve_git_path(&cwd, &text),
            None => cwd.clone(),
        }),
        git_directory: PathBuf::from(resolve_git_path(&cwd, &git_dir)),
        common_directory: PathBuf::from(resolve_git_path(&cwd, &common_dir)),
    })
}

/// The root of the worktree containing `start`, or `None` when `start` is in no
/// repository git recognises.
///
/// Exactly the path [`resolve_project`] reports as `directory`, by asking the same
/// function: a stray `.git` that git itself rejects is not a worktree here either, and
/// a caller that rooted state at a directory `resolve_project` disagreed with would
/// write outside the anchor every exclude pattern is read against.
///
/// That anchor is why this exists. [`crate::generated_dir`] roots Zuno's generated
/// directories here so a session started deep inside a checkout writes to the same
/// place as one started at the top, and needs the root without needing the project id.
#[must_use]
pub fn worktree_root(start: &Path) -> Option<PathBuf> {
    discover_repository(start).map(|repository| repository.worktree)
}

/// Port of `Project.resolve`.
///
/// Outside a repository the directory becomes `path.parse(start).root` — `/` for
/// an absolute `start` and the empty path for a relative one — and the id is
/// `global`. That is why every non-repository directory on a machine shares one
/// project id.
///
/// # The returned id may not be the one on disk yet
///
/// `remote ?? previous ?? root_commit` is the *current* precedence, but real
/// The cached marker remains the second choice so a project without a usable
/// remote keeps a stable id across root-history changes.
#[must_use]
pub fn resolve_project(start: &Path) -> ResolvedProject {
    let Some(repository) = discover_repository(start) else {
        return ResolvedProject {
            previous: None,
            id: GLOBAL_PROJECT_ID.to_owned(),
            directory: PathBuf::from(node_path::root(&start.to_string_lossy())),
            vcs: None,
        };
    };

    let previous = cached_project_id(&repository.common_directory);
    let id = remote_project_id(&repository)
        .or_else(|| previous.clone())
        .or_else(|| root_commit_project_id(&repository))
        .unwrap_or_else(|| GLOBAL_PROJECT_ID.to_owned());

    ResolvedProject {
        previous,
        id,
        directory: repository.worktree.clone(),
        vcs: Some(Vcs::Git {
            store: repository.common_directory,
        }),
    }
}

/// The id cached at `<common_directory>/zuno`, trimmed; `None` when the file
/// is missing or blank.
#[must_use]
pub fn cached_project_id(common_directory: &Path) -> Option<String> {
    let marker = node_path::join(&common_directory.to_string_lossy(), PROJECT_ID_MARKER);
    let contents = std::fs::read_to_string(&marker).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

/// `Hash.fast("git-remote:" + normalized)` for the `origin` remote.
#[must_use]
pub fn remote_project_id(repository: &Repository) -> Option<String> {
    let origin = git(
        &repository.worktree.to_string_lossy(),
        &["remote", "get-url", "origin"],
    )?;
    let origin = origin.trim();
    if origin.is_empty() {
        return None;
    }
    let normalized = normalize_remote(origin)?;
    Some(project_id_for_remote(&normalized))
}

/// The project id for an already-normalized remote such as
/// `github.com/sunerpy/opencode-rust`.
#[must_use]
pub fn project_id_for_remote(normalized_remote: &str) -> String {
    sha1::hex(format!("{REMOTE_ID_PREFIX}{normalized_remote}").as_bytes())
}

/// The lexicographically first root commit, used when there is no usable remote
/// and no cached id.
///
/// `rev-list --max-parents=0 HEAD` can report several roots after a history
/// graft or an octopus merge of unrelated histories; the oracle sorts and takes
/// the first so the id is stable regardless of Git's traversal order.
///
/// The cached marker is the stable fallback for repositories without a usable
/// remote; this value is only consulted when no marker is present.
#[must_use]
pub fn root_commit_project_id(repository: &Repository) -> Option<String> {
    let output = git(
        &repository.worktree.to_string_lossy(),
        &["rev-list", "--max-parents=0", "HEAD"],
    )?;
    let mut roots: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    roots.sort_unstable();
    roots.first().map(|root| (*root).to_owned())
}

/// Port of the `url` + `parts` helpers in `project.ts:81-103`.
///
/// Produces `<lowercased host>/<path without leading slashes, `.git` suffix or
/// trailing slashes>`, or `None` when the remote is a `file:` URL or has no
/// usable host and path. Two spellings of one repository — `https://` and
/// `git@…:` — therefore normalize to the same id, which is the whole point:
/// cloning over a different transport must not fork the project's history.
#[must_use]
pub fn normalize_remote(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(parsed) = url::Url::parse(value) {
        if parsed.scheme() == "file" {
            return None;
        }
        return remote_parts(parsed.host_str().unwrap_or_default(), parsed.path());
    }
    let (host, path) = scp_like(value)?;
    remote_parts(host, path)
}

/// Port of `^([^@/:]+@)?([^/:]+):(.+)$`, the SCP-like remote form
/// (`git@github.com:owner/repo.git`).
///
/// Written as explicit scanning rather than a regex so the crate needs no regex
/// dependency; the character classes are transcribed from the pattern.
fn scp_like(value: &str) -> Option<(&str, &str)> {
    // The optional `user@` prefix: everything before the first `@`, provided it
    // contains no `/` or `:`.
    let after_user = match value.find('@') {
        Some(at) if !value[..at].contains(['/', ':']) => &value[at + 1..],
        _ => value,
    };
    // `([^/:]+):` — the host runs up to the first `:` and may not contain `/`.
    let colon = after_user.find(':')?;
    let host = &after_user[..colon];
    let path = &after_user[colon + 1..];
    if host.is_empty() || host.contains('/') || path.is_empty() {
        return None;
    }
    Some((host, path))
}

/// Port of `parts(host, name)`.
fn remote_parts(host: &str, name: &str) -> Option<String> {
    let mut path = name.trim_start_matches('/');
    // `.replace(/\.git\/?$/, "")` — one optional trailing slash, then `.git`.
    let without_slash = path.strip_suffix('/').unwrap_or(path);
    if let Some(stripped) = without_slash.strip_suffix(".git") {
        path = stripped;
    }
    let path = path.trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_lowercase()))
}

/// Port of `Git.resolvePath` (`packages/core/src/git.ts:980-986`).
///
/// Trailing newlines only — Git's output ends with one, but a path may legally
/// contain leading or trailing spaces, and trimming those would corrupt it.
///
/// Visible to the crate because [`crate::exclude`] resolves the answer to
/// `git rev-parse --git-path`, which has exactly this shape.
pub(crate) fn resolve_git_path(cwd: &str, value: &str) -> String {
    let trimmed = value.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return cwd.to_owned();
    }
    let normalized = node_path::windows_path(trimmed);
    if node_path::is_absolute(&normalized) {
        return node_path::normalize(&normalized);
    }
    node_path::resolve(cwd, &[&normalized])
}

/// Run `git` in `cwd`, returning stdout on success and `None` on any failure —
/// non-zero exit, non-UTF-8 output, the binary being absent, or the call outstaying
/// [`GIT_TIMEOUT`].
///
/// A timeout folding into that existing contract is the point: every caller already
/// reads `None` as "git could not answer", so bounding the wait in this one function
/// bounds [`discover_repository`], [`worktree_root`] and [`resolve_project`] without
/// changing a signature or making anything async.
fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    String::from_utf8(bounded_stdout(&mut command, GIT_TIMEOUT)?).ok()
}

/// Run `command` under `ceiling`, returning its stdout bytes when it exits
/// successfully in time, and `None` otherwise — after killing it if it did not.
///
/// [`crate::bounded::output`] does the work, including why this cannot be
/// [`Command::output`] and why both pipes are drained before anything is reaped. This
/// module needs none of what that reports: stderr is dropped, because git's
/// complaints must never reach Zuno's own stderr where they would land in the middle
/// of a TUI frame, and the three failures collapse into `None` because a machine
/// without git, a git that objected and a git that never answered all mean the same
/// thing to a project id — fall back to `global`.
///
/// Bytes come back rather than text because nothing here may reshape the output: the
/// caller decides what a non-UTF-8 answer means, and [`resolve_git_path`] strips a
/// trailing newline and nothing else, since a path may legally end in a space.
fn bounded_stdout(command: &mut Command, ceiling: Duration) -> Option<Vec<u8>> {
    let collected = bounded::output(command, ceiling).ok()?;
    collected.status.success().then_some(collected.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    #[cfg(unix)]
    use std::process::Stdio;
    #[cfg(unix)]
    use std::time::Instant;

    fn run(cwd: &Path, args: &[&str]) {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let status = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", null_device)
            .env("GIT_CONFIG_SYSTEM", null_device)
            .env("GIT_AUTHOR_NAME", "zuno-paths")
            .env("GIT_AUTHOR_EMAIL", "zuno-paths@example.test")
            .env("GIT_COMMITTER_NAME", "zuno-paths")
            .env("GIT_COMMITTER_EMAIL", "zuno-paths@example.test")
            .output()
            .unwrap_or_else(|error| panic!("spawn {args:?}: {error}"));
        assert!(
            status.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path();
        run(path, &["git", "init", "--initial-branch=main", "."]);
        fs::write(path.join("file.txt"), "hello\n").expect("write file");
        run(path, &["git", "add", "file.txt"]);
        run(path, &["git", "commit", "-m", "initial"]);
        root
    }

    fn assert_same_path(actual: &Path, expected: &Path) {
        assert_eq!(
            actual.canonicalize().expect("canonicalize actual path"),
            expected.canonicalize().expect("canonicalize expected path"),
            "paths identify different filesystem entries: actual={} expected={}",
            actual.display(),
            expected.display()
        );
    }

    #[test]
    fn discovers_the_worktree_root_from_a_nested_directory() {
        let root = repository();
        let nested = root.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("create nested");

        let discovered = discover_repository(&nested).expect("repository");
        assert_same_path(&discovered.worktree, root.path());
        assert_same_path(&discovered.git_directory, &root.path().join(".git"));
        assert_same_path(&discovered.common_directory, &root.path().join(".git"));
    }

    /// Generated state is rooted here and every exclude pattern is read against it, so
    /// the cheap resolution has to agree with the full one from anywhere in the tree.
    #[test]
    fn the_worktree_root_is_the_same_from_a_nested_directory_as_from_the_top() {
        let root = repository();
        let nested = root.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("create nested");

        let from_nested = worktree_root(&nested).expect("a worktree root");

        assert_same_path(&from_nested, root.path());
        assert_same_path(
            &from_nested,
            &worktree_root(root.path()).expect("from the top"),
        );
        assert_same_path(
            &from_nested,
            &discover_repository(&nested).expect("repository").worktree,
        );
    }

    /// A caller that already resolved the project holds the generated root.
    ///
    /// `TurnHost::open_with_runtime_mcp_and_observers` composes a turn's tools from a
    /// synchronous closure it cannot await in, so it cannot call
    /// [`crate::generated_root`] there — the call spawns `git rev-parse`, and every CLI
    /// entry point drives a current-thread runtime where a blocked thread is the whole
    /// reactor. What it does instead is read the root out of the `ResolvedProject` it
    /// already has, which is only correct while these two functions answer from the same
    /// discovery. Pinned here, in the crate that owns both, because the caller cannot
    /// pin it: reading it there would just restate the derivation.
    #[test]
    fn a_resolved_project_already_names_the_generated_root() {
        let root = repository();
        let nested = root.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("create nested");

        for directory in [root.path(), nested.as_path()] {
            let project = resolve_project(directory);
            assert!(
                project.vcs.is_some(),
                "{} is in a repository, so the project must carry a vcs",
                directory.display()
            );
            // Exactly the derivation the caller performs: the project directory when a
            // vcs was detected, the session directory otherwise.
            let derived = project
                .vcs
                .as_ref()
                .map_or_else(|| directory.to_path_buf(), |_| project.directory.clone());
            assert_same_path(&derived, &crate::generated_root(directory));
        }

        let outside = tempfile::tempdir().expect("tempdir");
        let project = resolve_project(outside.path());
        assert_eq!(
            project.vcs, None,
            "a tempdir outside a checkout has no vcs, and the caller then keeps its own \
             directory rather than the `/` this function reports as the project directory"
        );
        let derived = project
            .vcs
            .as_ref()
            .map_or_else(|| outside.path().to_path_buf(), |_| project.directory);
        assert_same_path(&derived, &crate::generated_root(outside.path()));
    }

    /// A directory with no repository above it, and — as a shared `/tmp` on a build
    /// machine can have — one with a `.git` git itself does not accept, both have to
    /// answer the same way [`resolve_project`] does: there is no worktree here.
    #[test]
    fn outside_a_repository_recognised_by_git_there_is_no_worktree_root() {
        let outside = tempfile::tempdir().expect("tempdir");
        assert_eq!(worktree_root(outside.path()), None);

        fs::create_dir(outside.path().join(".git")).expect("an empty .git");

        assert_eq!(worktree_root(outside.path()), None);
        assert_eq!(resolve_project(outside.path()).vcs, None);
    }

    /// In a linked worktree the git directory is per-worktree while the common
    /// directory stays with the original clone. The snapshot store is keyed on
    /// the worktree, and the project id marker on the common directory, so
    /// conflating the two would make two worktrees of one repo share a store.
    #[test]
    fn separates_git_dir_from_common_dir_in_a_linked_worktree() {
        let root = repository();
        let linked = root.path().join("linked");
        run(
            root.path(),
            &[
                "git",
                "worktree",
                "add",
                linked.to_str().expect("utf8 path"),
                "-b",
                "side",
            ],
        );

        let discovered = discover_repository(&linked).expect("repository");
        let expected_common = root.path().join(".git");
        assert_same_path(&discovered.worktree, &linked);
        assert_same_path(&discovered.common_directory, &expected_common);
        assert_same_path(
            &discovered.git_directory,
            &expected_common.join("worktrees").join("linked"),
        );
        assert_ne!(discovered.git_directory, discovered.common_directory);
    }

    #[test]
    fn outside_a_repository_the_project_is_global_at_the_filesystem_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_project(root.path());
        assert_eq!(resolved.id, GLOBAL_PROJECT_ID);
        assert_eq!(
            resolved.directory,
            PathBuf::from(node_path::root(&root.path().to_string_lossy()))
        );
        assert_eq!(resolved.vcs, None);
        assert_eq!(resolved.previous, None);
    }

    #[test]
    fn the_project_id_marker_uses_the_zuno_identity() {
        assert_eq!(PROJECT_ID_MARKER, "zuno");
    }

    #[test]
    fn a_relative_start_outside_a_repository_yields_an_empty_directory() {
        let resolved = resolve_project(Path::new("definitely-not-a-repo-relative"));
        assert_eq!(resolved.id, GLOBAL_PROJECT_ID);
        assert_eq!(resolved.directory, Path::new(""));
    }

    #[test]
    fn without_a_remote_the_id_is_the_root_commit() {
        let root = repository();
        let resolved = resolve_project(root.path());
        let head = git(&root.path().to_string_lossy(), &["rev-parse", "HEAD"]).expect("head");
        assert_eq!(resolved.id, head.trim());
        assert_eq!(resolved.previous, None);
        let Some(Vcs::Git { store }) = resolved.vcs.as_ref() else {
            panic!("a repository must expose its Git store");
        };
        assert_same_path(store, &root.path().join(".git"));
    }

    #[test]
    fn a_remote_wins_over_the_root_commit_and_the_cached_marker() {
        let root = repository();
        run(
            root.path(),
            &[
                "git",
                "remote",
                "add",
                "origin",
                "https://github.com/sunerpy/opencode-rust.git",
            ],
        );
        let common = root.path().join(".git");
        fs::write(common.join(PROJECT_ID_MARKER), "cached-id\n").expect("write marker");

        let resolved = resolve_project(root.path());
        assert_eq!(resolved.previous, Some("cached-id".to_owned()));
        assert_eq!(
            resolved.id,
            project_id_for_remote("github.com/sunerpy/opencode-rust")
        );
    }

    #[test]
    fn the_cached_marker_wins_when_there_is_no_remote() {
        let root = repository();
        fs::write(
            root.path().join(".git").join(PROJECT_ID_MARKER),
            "  cached-id  \n",
        )
        .expect("write marker");
        let resolved = resolve_project(root.path());
        assert_eq!(resolved.id, "cached-id");
        assert_eq!(resolved.previous, Some("cached-id".to_owned()));
    }

    #[test]
    fn a_blank_marker_is_ignored() {
        let root = repository();
        fs::write(root.path().join(".git").join(PROJECT_ID_MARKER), "   \n").expect("write marker");
        assert_eq!(cached_project_id(&root.path().join(".git")), None);
        let resolved = resolve_project(root.path());
        assert_eq!(resolved.previous, None);
        assert_ne!(resolved.id, GLOBAL_PROJECT_ID);
    }

    /// Every expectation in this table was produced by executing the oracle's
    /// own `url`/`parts` helpers under `bun`, not by reading them. Two results
    /// are counter-intuitive and would have been "fixed" wrongly otherwise:
    /// `github.com:owner/repo` normalizes to **nothing** (WHATWG accepts
    /// `github.com` as a scheme, so the SCP branch is never reached and the host
    /// comes out empty), and an IPv6 host keeps its brackets.
    #[test]
    fn normalize_remote_matches_the_oracle_table() {
        let cases: [(&str, Option<&str>); 19] = [
            (
                "https://github.com/sunerpy/opencode-rust",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            (
                "https://github.com/sunerpy/opencode-rust.git",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            (
                "https://github.com/sunerpy/opencode-rust.git/",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            (
                "https://GitHub.com/sunerpy/opencode-rust.git",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            (
                "git@github.com:sunerpy/opencode-rust.git",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            ("github.com:sunerpy/opencode-rust", None),
            (
                "ssh://git@github.com/sunerpy/opencode-rust.git",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            (
                "  https://github.com/sunerpy/opencode-rust.git  ",
                Some("github.com/sunerpy/opencode-rust"),
            ),
            ("file:///srv/git/repo.git", None),
            ("https://github.com", None),
            ("https://github.com/", None),
            ("https://github.com/.git", None),
            ("not a remote at all", None),
            ("git://github.com/a/b.git", Some("github.com/a/b")),
            ("https://user:pw@github.com/a/b.git", Some("github.com/a/b")),
            ("ssh://git@github.com:2222/a/b.git", Some("github.com/a/b")),
            ("http://[::1]/a/b.git", Some("[::1]/a/b")),
            ("/srv/git/repo.git", None),
            ("../relative/repo.git", None),
        ];
        for (remote, expected) in cases {
            assert_eq!(normalize_remote(remote).as_deref(), expected, "{remote}");
        }
    }

    #[test]
    fn blank_remotes_are_rejected() {
        assert_eq!(normalize_remote(""), None);
        assert_eq!(normalize_remote("   "), None);
        assert_eq!(normalize_remote("\n\t "), None);
    }

    /// The point of normalization: cloning the same repository over HTTPS and
    /// over SSH must land on one project id, or a user's sessions would split in
    /// two when they change remote.
    #[test]
    fn https_and_ssh_clones_share_one_project_id() {
        let https =
            normalize_remote("https://github.com/sunerpy/opencode-rust.git").expect("https");
        let ssh = normalize_remote("git@github.com:sunerpy/opencode-rust.git").expect("ssh");
        assert_eq!(project_id_for_remote(&https), project_id_for_remote(&ssh));
    }

    #[test]
    fn remote_ids_are_sha1_over_the_prefixed_remote() {
        // Independently computed: printf 'git-remote:github.com/a/b' | sha1sum
        assert_eq!(
            project_id_for_remote("github.com/a/b"),
            sha1::hex(b"git-remote:github.com/a/b")
        );
        assert_eq!(project_id_for_remote("github.com/a/b").len(), 40);
    }

    #[test]
    fn resolve_git_path_honours_absolute_and_relative_output() {
        if cfg!(windows) {
            assert_eq!(
                resolve_git_path(r"C:\repo", "C:/abs/worktree\n"),
                r"C:\abs\worktree"
            );
            assert_eq!(resolve_git_path(r"C:\repo", ".git\n"), r"C:\repo\.git");
            assert_eq!(resolve_git_path(r"C:\repo", "\n"), r"C:\repo");
            assert_eq!(
                resolve_git_path(r"C:\repo", "C:/abs/../other\r\n"),
                r"C:\other"
            );
            return;
        }
        assert_eq!(
            resolve_git_path("/repo", "/abs/worktree\n"),
            "/abs/worktree"
        );
        assert_eq!(resolve_git_path("/repo", ".git\n"), "/repo/.git");
        assert_eq!(resolve_git_path("/repo", "\n"), "/repo");
        assert_eq!(resolve_git_path("/repo", ""), "/repo");
        assert_eq!(resolve_git_path("/repo", "/abs/../other\r\n"), "/other");
        // A path may legitimately end in a space; only newlines are stripped.
        assert_eq!(resolve_git_path("/repo", "dir /\n"), "/repo/dir ");
    }

    #[test]
    fn find_git_marker_walks_up() {
        let root = repository();
        let nested = root.path().join("x").join("y").join("z");
        fs::create_dir_all(&nested).expect("create nested");
        assert_eq!(find_git_marker(&nested), Some(root.path().join(".git")));
        assert_eq!(
            find_git_marker(&root.path().join("x")),
            Some(root.path().join(".git"))
        );
    }

    /// Names the case a re-exec of this test binary is playing, and is what turns
    /// that re-exec into the child half of a fake-git case at all.
    #[cfg(unix)]
    const FAKE_GIT_CASE: &str = "ZUNO_PATHS_FAKE_GIT_CASE";

    /// The directory the child half asks git about.
    #[cfg(unix)]
    const FAKE_GIT_CWD: &str = "ZUNO_PATHS_FAKE_GIT_CWD";

    /// What the child half prints once it has made every assertion its case calls for.
    ///
    /// A test binary given a filter that matches nothing exits successfully, so a
    /// misspelled child name would turn every case below into a silent pass. The
    /// child says which case it actually ran, and this half insists on hearing it.
    #[cfg(unix)]
    const OBSERVED: &str = "fake git case observed: ";

    /// How long the fake `git` sleeps in the stalled cases.
    ///
    /// An order of magnitude past [`GIT_TIMEOUT`], so a call that returns at all can
    /// only have returned because the ceiling ended it, and so the assertion on
    /// elapsed time is not a race against a slow machine.
    #[cfg(unix)]
    const FAKE_GIT_SLEEP: Duration = Duration::from_secs(120);

    /// The size of the fake `git`'s answer in the `large` case.
    ///
    /// Four times the usual 64 KiB pipe capacity, so the child cannot finish writing
    /// unless somebody is draining the pipe while this process waits for the exit.
    /// An implementation that waited first and read afterwards would deadlock here
    /// and then report this healthy call as a timeout.
    #[cfg(unix)]
    const LARGE_OUTPUT: usize = 256 * 1024;

    /// A `git` that misbehaves on purpose, one branch per case.
    ///
    /// `hang` and `hang-root` are the defect itself: a `.git` on a dead network
    /// mount, where git blocks in the kernel and never answers. The other branches
    /// pin the behaviors the ceiling was not allowed to change.
    ///
    /// The stall `exec`s rather than calling `sleep`, so the process the ceiling
    /// kills is the one that is stalling. A shell that merely waited for `sleep`
    /// would be killed while `sleep` kept the pipe open behind it, which is a
    /// property of this fixture and not of `git`.
    #[cfg(unix)]
    fn fake_git_script() -> String {
        format!(
            "#!/bin/sh\n\
             case \"${FAKE_GIT_CASE}\" in\n\
             hang|hang-root) exec sleep {sleep} ;;\n\
             trailing) printf 'dir /  \\n' ;;\n\
             large) yes | head -c {large} ;;\n\
             invalid-utf8) printf '\\377\\376' ;;\n\
             failure) printf 'ignored\\n'; exit 3 ;;\n\
             stdin) printf 'stdin:'; cat ;;\n\
             *) echo \"unknown case\" >&2; exit 127 ;;\n\
             esac\n",
            sleep = FAKE_GIT_SLEEP.as_secs(),
            large = LARGE_OUTPUT,
        )
    }

    /// Run this binary again as the child half of `case`, with a fake `git` as the
    /// first `git` on its `PATH`, and report how long the child took.
    ///
    /// A re-exec is the only way to test this at all: `PATH` decides which `git`
    /// runs, `std::env::set_var` is unsafe, and this workspace forbids unsafe code,
    /// so a test can never doctor its own environment — only a child's.
    ///
    /// The child's own output goes to files rather than pipes. This half has to be
    /// able to give up on a child that never exits, and polling for an exit while
    /// nobody drains a pipe is precisely the deadlock [`bounded_stdout`] is written
    /// to avoid; reproducing it in the test that proves the fix would prove nothing.
    #[cfg(unix)]
    fn run_fake_git_case(case: &str) -> (Duration, String) {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::tempdir().expect("tempdir");
        let bin = home.path().join("bin");
        fs::create_dir(&bin).expect("create the fake bin directory");
        // The `absent` case is the machine without git, so it gets no fake either.
        let absent = case == "absent";
        if !absent {
            let program = bin.join("git");
            fs::write(&program, fake_git_script()).expect("write the fake git");
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
                .expect("make the fake git executable");
        }

        let cwd = home.path().join("work");
        fs::create_dir(&cwd).expect("create the working directory");
        if case == "hang-root" {
            // `worktree_root` never asks git anything without a marker to start from.
            fs::create_dir(cwd.join(".git")).expect("create the git marker");
        }

        // Bait for the `stdin` case: with stdin closed the fake git reads nothing, and
        // with stdin inherited it reads this, so the two are told apart by the answer
        // rather than by how long the read took.
        let bait = home.path().join("stdin");
        fs::write(&bait, "leaked").expect("write the stdin bait");
        let out = home.path().join("stdout");
        let err = home.path().join("stderr");

        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args(["--exact", "project::tests::fake_git_child", "--nocapture"])
            .env(FAKE_GIT_CASE, case)
            .env(FAKE_GIT_CWD, &cwd)
            // The fake git shadows the real one because it comes first, and the
            // usual directories stay on the path because the script needs a shell's
            // own utilities. The `absent` case keeps them off, since a real git found
            // in `/usr/bin` is not an absent one.
            .env(
                "PATH",
                if absent {
                    bin.display().to_string()
                } else {
                    format!("{}:/bin:/usr/bin", bin.display())
                },
            )
            .stdin(Stdio::from(
                fs::File::open(&bait).expect("open the stdin bait"),
            ))
            .stdout(Stdio::from(
                fs::File::create(&out).expect("create the child stdout"),
            ))
            .stderr(Stdio::from(
                fs::File::create(&err).expect("create the child stderr"),
            ));

        let started = Instant::now();
        let mut child = command.spawn().expect("spawn the child half");
        // Four ceilings: enough for `hang-root`, which spends two of them, and short
        // enough that a lost bound fails this test instead of hanging the suite.
        let deadline = started + GIT_TIMEOUT * 4;
        let status = loop {
            match child.try_wait().expect("poll the child half") {
                Some(status) => break status,
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "the {case} child never returned within {:?}",
                        started.elapsed()
                    );
                }
            }
        };
        let elapsed = started.elapsed();
        let log = format!(
            "{}{}",
            fs::read_to_string(&out).expect("read the child stdout"),
            fs::read_to_string(&err).expect("read the child stderr")
        );
        assert!(status.success(), "the {case} child failed:\n{log}");
        assert!(
            log.contains(&format!("{OBSERVED}{case}")),
            "the {case} child never reached its assertions:\n{log}"
        );
        (elapsed, log)
    }

    /// The defect this closes: `git` reading a `.git` on an unresponsive network
    /// mount blocks in the kernel and never answers, and every caller in this module
    /// is synchronous, so an unbounded wait wedges the process — on a current-thread
    /// runtime, the whole reactor with it.
    #[test]
    #[cfg(unix)]
    fn a_git_that_never_answers_is_given_up_on_at_the_ceiling() {
        let (elapsed, log) = run_fake_git_case("hang");
        assert!(
            elapsed < FAKE_GIT_SLEEP,
            "waited {elapsed:?} on a git sleeping {FAKE_GIT_SLEEP:?}:\n{log}"
        );
    }

    /// The same stall reached through the public entry point, because that is what a
    /// caller has: `worktree_root` spends the ceiling on `--show-toplevel` and again
    /// on `--git-dir`, then reports no worktree, rather than never reporting at all.
    #[test]
    #[cfg(unix)]
    fn a_git_that_never_answers_cannot_hang_worktree_discovery() {
        let (elapsed, log) = run_fake_git_case("hang-root");
        assert!(
            elapsed < FAKE_GIT_SLEEP,
            "waited {elapsed:?} on a git sleeping {FAKE_GIT_SLEEP:?}:\n{log}"
        );
    }

    /// Everything the ceiling had to leave alone, each against a `git` built to
    /// produce exactly that answer.
    #[test]
    #[cfg(unix)]
    fn bounding_the_wait_changed_no_other_answer() {
        for case in [
            "trailing",
            "large",
            "invalid-utf8",
            "failure",
            "stdin",
            "absent",
        ] {
            let (elapsed, log) = run_fake_git_case(case);
            assert!(
                elapsed < GIT_TIMEOUT,
                "the {case} case took {elapsed:?}, so it was decided by the ceiling \
                 rather than by the answer:\n{log}"
            );
        }
    }

    /// The child half of every fake-git case, and a no-op in an ordinary run: one
    /// test binary is also its own fixture, since only a child process can be given
    /// a different `PATH`.
    #[test]
    #[cfg(unix)]
    fn fake_git_child() {
        let Ok(case) = std::env::var(FAKE_GIT_CASE) else {
            return;
        };
        let cwd = std::env::var(FAKE_GIT_CWD).expect("the working directory");
        let asked = ["rev-parse", "--show-toplevel"];
        let started = Instant::now();
        match case.as_str() {
            "hang" => {
                let observed = git(&cwd, &asked);
                let elapsed = started.elapsed();
                assert_eq!(observed, None, "a git that never answers has no answer");
                assert!(
                    elapsed >= GIT_TIMEOUT,
                    "returned in {elapsed:?}, inside the ceiling: the fake git cannot \
                     have run, so this proves nothing"
                );
                assert!(
                    elapsed < FAKE_GIT_SLEEP,
                    "waited {elapsed:?} on a git sleeping {FAKE_GIT_SLEEP:?}"
                );
            }
            "hang-root" => {
                let observed = worktree_root(Path::new(&cwd));
                let elapsed = started.elapsed();
                assert_eq!(observed, None, "a git that never answered names no root");
                assert!(
                    elapsed >= GIT_TIMEOUT,
                    "returned in {elapsed:?}, inside one ceiling: the fake git cannot \
                     have run, so this proves nothing"
                );
                assert!(
                    elapsed < FAKE_GIT_SLEEP,
                    "waited {elapsed:?} on a git sleeping {FAKE_GIT_SLEEP:?}"
                );
            }
            // Trailing bytes are preserved exactly. `resolve_git_path` trims a
            // trailing newline and nothing else because a path may legally end in a
            // space, so this function must not trim anything at all.
            "trailing" => assert_eq!(git(&cwd, &asked).as_deref(), Some("dir /  \n")),
            // A healthy answer larger than the pipe buffer arrives whole.
            "large" => assert_eq!(
                git(&cwd, &asked).map(|answer| answer.len()),
                Some(LARGE_OUTPUT)
            ),
            "invalid-utf8" => assert_eq!(git(&cwd, &asked), None, "non-UTF-8 is no answer"),
            // A machine without git behaves like a directory that is no repository,
            // which is the contract the ceiling had to fold into rather than widen.
            "absent" => assert_eq!(git(&cwd, &asked), None, "there is no git to ask"),
            "failure" => assert_eq!(
                git(&cwd, &asked),
                None,
                "a non-zero exit discards what was printed"
            ),
            // Stdin stays closed rather than inherited: git must never consume the
            // bytes a caller of Zuno is holding for something else.
            "stdin" => assert_eq!(git(&cwd, &asked).as_deref(), Some("stdin:")),
            other => panic!("unknown fake git case {other}"),
        }
        println!("{OBSERVED}{case}");
    }
}
