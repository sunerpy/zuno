//! The two glob shapes `instructions[]` resolution needs.
//!
//! The oracle reaches for Bun's `Glob.scan`, which this module reproduces with
//! `globset` + `walkdir`:
//!
//! ```text
//! fs.glob(pattern, { cwd, absolute: true, include: "file", dot: ... })
//! ```
//!
//! Two properties are load-bearing and are easy to lose in a rewrite:
//!
//! 1. **`*` does not cross a separator.** `globset` only behaves that way with
//!    [`globset::GlobBuilder::literal_separator`] turned on; the default lets
//!    `*` swallow `/` and would make `*.md` match `docs/nested/x.md`.
//! 2. **A pattern with no metacharacters must not walk the tree.** [`up`] runs
//!    the pattern once per ancestor directory all the way to the filesystem
//!    root, so a recursive scan for the literal `AGENTS.md` would walk every
//!    sibling of every ancestor — near `/` that is the whole disk. Bun's scanner
//!    is effectively an `exists` check for a literal pattern, and so is this one.
//!
//! Depth is bounded for any pattern without `**`, for the same reason.

use globset::{GlobBuilder, GlobMatcher};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zuno_paths::node_path;

/// Characters whose presence makes a pattern a pattern.
const META: [char; 5] = ['*', '?', '[', '{', '!'];

/// `fs.glob(pattern, { cwd, absolute: true, include: "file", dot })`.
///
/// Returns absolute paths of matching **files** — directories never match,
/// which is `include: "file"`. Order is `walkdir`'s deterministic
/// depth-first order, then sorted, so a caller's de-duplication is stable
/// between runs.
///
/// `dot == false` hides path components that begin with `.` unless the pattern's
/// component at the same position also begins with `.`, which is the standard
/// glob rule Bun implements.
#[must_use]
pub(crate) fn files(pattern: &str, cwd: &Path, dot: bool) -> Vec<PathBuf> {
    if pattern.is_empty() {
        return Vec::new();
    }

    // Literal fast path: no scan, just an existence check. See the module note.
    if !pattern.contains(META) {
        let candidate = PathBuf::from(node_path::join(&cwd.to_string_lossy(), pattern));
        if candidate.is_file() {
            return vec![candidate];
        }
        return Vec::new();
    }

    let Some(matcher) = compile(pattern) else {
        return Vec::new();
    };

    let mut walker = WalkDir::new(cwd).follow_links(false);
    if let Some(depth) = bounded_depth(pattern) {
        walker = walker.max_depth(depth);
    }

    let mut found = Vec::new();
    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        let relative = relative.to_string_lossy();
        if !matcher.is_match(relative.as_ref()) {
            continue;
        }
        if !hidden_allowed(pattern, relative.as_ref(), dot) {
            continue;
        }
        found.push(entry.path().to_path_buf());
    }
    found.sort();
    found
}

/// `FSUtil.globUp(pattern, start, stop)`
/// (`packages/core/src/fs-util.ts:184-199`).
///
/// Runs [`files`] with `dot: true` in every directory from `start` towards the
/// filesystem root, appending matches nearest-first. `stop` is compared for
/// **string equality after** the directory is scanned, so the stop directory is
/// inclusive and a `stop` that is not on the chain never bounds the walk — the
/// same two properties [`zuno_paths::walk::up`] documents.
#[must_use]
pub(crate) fn up(pattern: &str, start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut current = start.to_string_lossy().into_owned();
    let stop = stop.map(|path| path.to_string_lossy().into_owned());
    loop {
        found.extend(files(pattern, Path::new(&current), true));
        if stop.as_deref() == Some(current.as_str()) {
            break;
        }
        let parent = node_path::dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    found
}

/// A pattern `globset` rejects yields no matches, mirroring the oracle's
/// `Effect.catch(() => [])` around every glob call.
fn compile(pattern: &str) -> Option<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .ok()
        .map(|glob| glob.compile_matcher())
}

/// The deepest directory level a pattern can reach, or `None` when `**` makes it
/// unbounded.
fn bounded_depth(pattern: &str) -> Option<usize> {
    if pattern.contains("**") {
        return None;
    }
    Some(pattern.split('/').filter(|part| !part.is_empty()).count())
}

fn hidden_allowed(pattern: &str, relative: &str, dot: bool) -> bool {
    if dot {
        return true;
    }
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    relative
        .split('/')
        .enumerate()
        .all(|(index, part)| match part.starts_with('.') {
            false => true,
            true => pattern_parts
                .get(index)
                .is_some_and(|expected| expected.starts_with('.')),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("docs/deep")).expect("mkdir docs/deep");
        fs::create_dir_all(root.path().join("nested/inner")).expect("mkdir nested/inner");
        fs::write(root.path().join("AGENTS.md"), "root").expect("write");
        fs::write(root.path().join(".hidden.md"), "hidden").expect("write");
        fs::write(root.path().join("docs/one.md"), "one").expect("write");
        fs::write(root.path().join("docs/deep/two.md"), "two").expect("write");
        fs::write(root.path().join("nested/inner/AGENTS.md"), "inner").expect("write");
        root
    }

    #[test]
    fn a_literal_pattern_is_an_existence_check() {
        let root = tree();
        assert_eq!(
            files("AGENTS.md", root.path(), true),
            vec![root.path().join("AGENTS.md")]
        );
        assert!(files("MISSING.md", root.path(), true).is_empty());
    }

    /// A literal pattern must not find a same-named file in a subdirectory —
    /// that is the difference between an existence check and a recursive scan,
    /// and getting it wrong makes [`up`] quadratic in the tree size.
    #[test]
    fn a_literal_pattern_does_not_recurse() {
        let root = tree();
        assert_eq!(files("AGENTS.md", root.path(), true).len(), 1);
    }

    #[test]
    fn a_directory_never_matches() {
        let root = tree();
        assert!(files("docs", root.path(), true).is_empty());
        assert!(files("do*s", root.path(), true).is_empty());
    }

    #[test]
    fn star_does_not_cross_a_separator() {
        let root = tree();
        let shallow = files("*.md", root.path(), true);
        assert!(shallow.contains(&root.path().join("AGENTS.md")));
        assert!(!shallow.contains(&root.path().join("docs/one.md")));
    }

    #[test]
    fn double_star_crosses_separators() {
        let root = tree();
        let deep = files("**/*.md", root.path(), true);
        assert!(deep.contains(&root.path().join("docs/one.md")));
        assert!(deep.contains(&root.path().join("docs/deep/two.md")));
    }

    #[test]
    fn dot_false_hides_dot_prefixed_components() {
        let root = tree();
        assert!(!files("*.md", root.path(), false).contains(&root.path().join(".hidden.md")));
        assert!(files("*.md", root.path(), true).contains(&root.path().join(".hidden.md")));
        assert!(files(".*.md", root.path(), false).contains(&root.path().join(".hidden.md")));
    }

    #[test]
    fn up_collects_nearest_first_and_stop_is_inclusive() {
        let root = tree();
        let found = up(
            "AGENTS.md",
            &root.path().join("nested/inner"),
            Some(root.path()),
        );
        assert_eq!(
            found,
            vec![
                root.path().join("nested/inner/AGENTS.md"),
                root.path().join("AGENTS.md"),
            ]
        );
    }

    #[test]
    fn up_stops_before_a_nearer_bound() {
        let root = tree();
        let found = up(
            "AGENTS.md",
            &root.path().join("nested/inner"),
            Some(&root.path().join("nested")),
        );
        assert_eq!(found, vec![root.path().join("nested/inner/AGENTS.md")]);
    }

    #[test]
    fn an_invalid_pattern_yields_nothing_rather_than_failing() {
        let root = tree();
        assert!(files("[", root.path(), true).is_empty());
    }

    #[test]
    fn bounded_depth_tracks_the_pattern_shape() {
        assert_eq!(bounded_depth("AGENTS.md"), Some(1));
        assert_eq!(bounded_depth("docs/*.md"), Some(2));
        assert_eq!(bounded_depth("**/*.md"), None);
    }
}
