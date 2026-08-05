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

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::node_path;
use crate::sha1;
use crate::walk;

/// The project id used when no repository can be discovered — `ID.global` in
/// `packages/schema/src/project-id.ts`.
pub const GLOBAL_PROJECT_ID: &str = "global";

/// The marker file inside the Git common directory that caches a previously
/// resolved project id — `Project.cached` reads `<commonDirectory>/opencode`.
pub const PROJECT_ID_MARKER: &str = "opencode";

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
/// disks predate it. Measured on this machine: the oracle's own checkout has
/// `origin = github.com/anomalyco/opencode`, so this function returns
/// `012780c4…`, while its existing snapshot store sits under `4b0ea68d…` — the
/// root commit — and `.git/opencode` still caches that older value. Every
/// `.git/opencode` marker found here holds a root commit, remote or not.
///
/// So a consumer that keys storage on `id` alone will silently abandon the user's
/// existing data. Consult [`ResolvedProject::previous`] and migrate, the way
/// `packages/opencode/src/project/project.ts:221`'s `migrateProjectId` does for
/// database rows. Recorded in full in
/// `.omo/notepads/opencode-rust/issues.md` for todos 20 and 23, which own the
/// migration.
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

/// The id cached at `<common_directory>/opencode`, trimmed; `None` when the file
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
/// Historically this was the *only* id, which is why real disks are full of
/// snapshot stores and `.git/opencode` markers holding a root commit even for
/// repositories that have a remote. [`ResolvedProject::previous`] is how a
/// consumer reaches that older id; see the migration warning on
/// [`resolve_project`].
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
fn resolve_git_path(cwd: &str, value: &str) -> String {
    let trimmed = value.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return cwd.to_owned();
    }
    let normalized = node_path::windows_path(trimmed);
    if node_path::is_absolute(normalized) {
        return node_path::normalize(normalized);
    }
    node_path::resolve(cwd, &[normalized])
}

/// Run `git` in `cwd`, returning stdout on success and `None` on any failure —
/// non-zero exit, non-UTF-8 output, or the binary being absent.
fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn run(cwd: &Path, args: &[&str]) {
        let status = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "oc-paths")
            .env("GIT_AUTHOR_EMAIL", "oc-paths@example.test")
            .env("GIT_COMMITTER_NAME", "oc-paths")
            .env("GIT_COMMITTER_EMAIL", "oc-paths@example.test")
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

    #[test]
    fn discovers_the_worktree_root_from_a_nested_directory() {
        let root = repository();
        let nested = root.path().join("a/b");
        fs::create_dir_all(&nested).expect("create nested");

        let discovered = discover_repository(&nested).expect("repository");
        let expected = root.path().canonicalize().expect("canonicalize");
        assert_eq!(discovered.worktree, expected);
        assert_eq!(discovered.git_directory, expected.join(".git"));
        assert_eq!(discovered.common_directory, expected.join(".git"));
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
        let expected_common = root
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join(".git");
        assert_eq!(
            discovered.worktree,
            linked.canonicalize().expect("canonicalize")
        );
        assert_eq!(discovered.common_directory, expected_common);
        assert_eq!(
            discovered.git_directory,
            expected_common.join("worktrees/linked")
        );
        assert_ne!(discovered.git_directory, discovered.common_directory);
    }

    #[test]
    fn outside_a_repository_the_project_is_global_at_the_filesystem_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_project(root.path());
        assert_eq!(resolved.id, GLOBAL_PROJECT_ID);
        assert_eq!(resolved.directory, Path::new("/"));
        assert_eq!(resolved.vcs, None);
        assert_eq!(resolved.previous, None);
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
        assert_eq!(
            resolved.vcs,
            Some(Vcs::Git {
                store: root
                    .path()
                    .canonicalize()
                    .expect("canonicalize")
                    .join(".git")
            })
        );
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
        let nested = root.path().join("x/y/z");
        fs::create_dir_all(&nested).expect("create nested");
        assert_eq!(find_git_marker(&nested), Some(root.path().join(".git")));
        assert_eq!(
            find_git_marker(&root.path().join("x")),
            Some(root.path().join(".git"))
        );
    }
}
