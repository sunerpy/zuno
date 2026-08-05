//! The `session.path` column: Node's `path.relative` semantics, reproduced.
//!
//! `session.path` is written once, at creation, by
//! `packages/opencode/src/session/session.ts:171-173`:
//!
//! ```text
//! function sessionPath(worktree: string, cwd: string) {
//!   return path.relative(path.resolve(worktree), cwd).replaceAll("\\", "/")
//! }
//! ```
//!
//! It is not a display string. It is the value the project-scope list filter
//! matches on — `path = ?` OR `path LIKE ? || '/%'`
//! (`session.ts:969-984`) — so a `path` computed even slightly differently from
//! the oracle's silently drops sessions out of their own project listing. Two
//! properties carry that:
//!
//! * **Node's `path.relative` is lexical.** It never touches the filesystem, so
//!   it does not resolve symlinks and does not require either path to exist.
//!   [`std::fs::canonicalize`] does both and would diverge on any worktree
//!   reached through a symlink — a `/tmp` -> `/private/tmp` macOS default, or a
//!   symlinked home. So this module normalizes `.`, `..` and repeated separators
//!   textually, exactly as Node's `normalizeString` does.
//! * **A session at the worktree root gets `""`, not `NULL`.** `path.relative`
//!   of a directory against itself is the empty string, and `toRow` stores it
//!   verbatim. Upstream then treats that empty string as *absent*: `fromRow`
//!   maps it back through `row.path ?? undefined` (empty string survives) while
//!   `info.ts:42` maps it through `row.path ? ... : undefined` (empty string
//!   becomes `undefined`), and `listByProject`'s `if (input.path)` skips the
//!   filter entirely for it. Writing `NULL` instead would land in a different
//!   branch of the oracle's `OR (path IS NULL AND directory = ?)` arm.
//!
//! Only POSIX separators are produced. The oracle's trailing
//! `.replaceAll("\\", "/")` is applied too, so a Windows-shaped input collapses
//! the same way it does upstream.

use std::path::Path;

/// The `session.path` value for a session opened in `directory` under
/// `worktree`.
///
/// Mirrors `session.ts:171-173`: `path.relative(path.resolve(worktree), cwd)`
/// with backslashes folded to forward slashes. Returns `""` when `directory`
/// *is* the worktree.
///
/// Both arguments are normalized lexically, and a relative argument is resolved
/// against the process working directory, matching Node's `path.resolve`.
#[must_use]
pub fn session_path(worktree: &Path, directory: &Path) -> String {
    let from = resolve(&to_posix(&worktree.to_string_lossy()));
    let to = resolve(&to_posix(&directory.to_string_lossy()));
    relative(&from, &to)
}

/// Fold Windows separators into POSIX ones.
///
/// The oracle applies this only to `path.relative`'s *result*. Applying it to
/// the inputs as well is what makes a Windows-shaped pair normalize instead of
/// being treated as one long segment; on POSIX input it is the identity.
fn to_posix(value: &str) -> String {
    value.replace('\\', "/")
}

/// Node's `path.posix.resolve` for a single argument.
///
/// A relative `value` is prefixed with the process working directory, then the
/// whole thing is normalized lexically. An absolute result keeps its leading
/// `/`; a relative one that normalizes away becomes `"."`, as Node's does.
fn resolve(value: &str) -> String {
    let mut resolved = value.to_owned();
    let mut absolute = value.starts_with('/');
    if !absolute {
        let cwd = std::env::current_dir()
            .map(|path| to_posix(&path.to_string_lossy()))
            .unwrap_or_else(|_| String::from("/"));
        absolute = cwd.starts_with('/');
        resolved = format!("{cwd}/{resolved}");
    }
    let normalized = normalize_string(&resolved, !absolute);
    if absolute {
        return format!("/{normalized}");
    }
    if normalized.is_empty() {
        return String::from(".");
    }
    normalized
}

/// Node's `normalizeString`: collapse `.`, `..` and repeated separators.
///
/// `allow_above_root` keeps leading `..` segments, which Node does only for a
/// path that stayed relative. Under an absolute root they are dropped, because
/// `/..` is `/`.
fn normalize_string(value: &str, allow_above_root: bool) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let mut above_root = 0usize;
    for segment in value.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.len() > above_root {
                    segments.pop();
                } else if allow_above_root {
                    segments.push("..");
                    above_root += 1;
                }
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Node's `path.posix.relative` over two already-resolved absolute paths.
///
/// Expressed segment-wise rather than by character index. For normalized
/// absolute paths the two agree: Node's `lastCommonSep` scan stops at the last
/// separator both strings share, which is the boundary of their common segment
/// prefix. `/abc` against `/abcd` therefore yields `../abcd` here exactly as it
/// does there — a shared *prefix* that is not a shared *segment* contributes
/// nothing.
fn relative(from: &str, to: &str) -> String {
    if from == to {
        return String::new();
    }
    let from_segments: Vec<&str> = from.split('/').filter(|part| !part.is_empty()).collect();
    let to_segments: Vec<&str> = to.split('/').filter(|part| !part.is_empty()).collect();

    let shared = from_segments
        .iter()
        .zip(to_segments.iter())
        .take_while(|(left, right)| left == right)
        .count();

    let mut parts: Vec<&str> = Vec::new();
    parts.extend(std::iter::repeat_n("..", from_segments.len() - shared));
    parts.extend_from_slice(&to_segments[shared..]);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path_of(value: &str) -> PathBuf {
        PathBuf::from(value)
    }

    #[test]
    fn a_session_at_the_worktree_root_gets_the_empty_string() {
        assert_eq!(
            session_path(&path_of("/home/u/project"), &path_of("/home/u/project")),
            ""
        );
    }

    #[test]
    fn a_session_below_the_worktree_gets_the_relative_subpath() {
        assert_eq!(
            session_path(
                &path_of("/home/u/project"),
                &path_of("/home/u/project/packages/core")
            ),
            "packages/core"
        );
    }

    #[test]
    fn a_session_outside_the_worktree_climbs_with_dot_dot() {
        assert_eq!(
            session_path(&path_of("/home/u/project"), &path_of("/home/u/other")),
            "../other"
        );
    }

    #[test]
    fn a_worktree_at_the_filesystem_root_needs_no_dot_dot() {
        assert_eq!(session_path(&path_of("/"), &path_of("/srv")), "srv");
    }

    #[test]
    fn a_directory_at_the_filesystem_root_climbs_out_of_every_segment() {
        assert_eq!(session_path(&path_of("/home/u"), &path_of("/")), "../..");
    }

    #[test]
    fn a_shared_prefix_that_is_not_a_shared_segment_is_not_shared() {
        assert_eq!(session_path(&path_of("/abc"), &path_of("/abcd")), "../abcd");
    }

    #[test]
    fn trailing_and_repeated_separators_normalize_away() {
        assert_eq!(
            session_path(&path_of("/home/u/project/"), &path_of("/home//u/project")),
            ""
        );
        assert_eq!(
            session_path(
                &path_of("/home/u/project"),
                &path_of("/home/u/project/sub/")
            ),
            "sub"
        );
    }

    #[test]
    fn dot_and_dot_dot_segments_normalize_lexically() {
        assert_eq!(
            session_path(
                &path_of("/home/u/./project"),
                &path_of("/home/u/project/pkg/../lib")
            ),
            "lib"
        );
    }

    #[test]
    fn dot_dot_above_an_absolute_root_is_dropped_rather_than_kept() {
        assert_eq!(session_path(&path_of("/../.."), &path_of("/srv")), "srv");
    }

    #[test]
    fn windows_separators_fold_into_posix_ones() {
        assert_eq!(
            session_path(
                &path_of("/home/u/project"),
                &path_of("/home/u/project\\packages\\core")
            ),
            "packages/core"
        );
    }

    #[test]
    fn a_relative_argument_resolves_against_the_working_directory() {
        let cwd = std::env::current_dir().expect("read working directory");
        assert_eq!(session_path(&cwd, &path_of(".")), "");
        assert_eq!(session_path(&cwd, &path_of("crates/oc-db")), "crates/oc-db");
    }

    #[test]
    fn normalize_string_keeps_leading_dot_dot_only_above_a_relative_root() {
        assert_eq!(normalize_string("a/../../b", true), "../b");
        assert_eq!(normalize_string("a/../../b", false), "b");
    }

    #[test]
    fn resolve_of_a_path_that_normalizes_away_is_the_current_directory_marker() {
        assert_eq!(resolve("/"), "/");
        assert_eq!(resolve("/a/.."), "/");
    }
}
