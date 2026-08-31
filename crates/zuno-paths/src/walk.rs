//! The ancestor walk every discovery step is built on.
//!
//! Port of `FSUtil.up` (`packages/core/src/fs-util.ts:168-182`):
//!
//! ```text
//! up({ targets, start, stop }) {
//!   const result = []
//!   let current = start
//!   while (true) {
//!     for (const target of targets) {
//!       const search = join(current, target)
//!       if (await fs.exists(search)) result.push(search)
//!     }
//!     if (stop === current) break
//!     const parent = dirname(current)
//!     if (parent === current) break
//!     current = parent
//!   }
//!   return result
//! }
//! ```
//!
//! Three details that a hand-rolled walk usually gets wrong, and that consumers
//! depend on:
//!
//! 1. **`stop` is checked *after* the directory is searched**, so the stop
//!    directory is inclusive. A config walk bounded by the worktree still
//!    reads the worktree's own `.opencode`.
//! 2. **`stop` is compared for string equality, not ancestry.** A `stop` that is
//!    not on the chain from `start` upwards never matches, and the walk runs all
//!    the way to the filesystem root. That is a real behaviour, not a bug to fix
//!    here.
//! 3. **Targets are tested in the order given, per directory.** So for
//!    `["x.jsonc", "x.json"]` a directory holding both contributes `.jsonc`
//!    first — which is what makes `ConfigPaths.files`' final `toReversed()`
//!    put `.json` ahead of `.jsonc` at the deepest level.
//!
//! Existence is `fs.exists`, which is true for a file *or* a directory. A file
//! named `.opencode` is therefore collected by the config chain, exactly as
//! upstream does.

use std::path::{Path, PathBuf};

use crate::node_path;

/// Walk from `start` towards the filesystem root, collecting existing
/// `<directory>/<target>` paths.
///
/// See the module documentation for the `stop` semantics.
#[must_use]
pub fn up(targets: &[&str], start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut current = start.to_string_lossy().into_owned();
    let stop = stop.map(|path| path.to_string_lossy().into_owned());
    loop {
        for target in targets {
            let candidate = node_path::join(&current, target);
            if Path::new(&candidate).exists() {
                found.push(PathBuf::from(candidate));
            }
        }
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

/// The first hit of [`up`], which is how `Git.repo.discover` picks the nearest
/// `.git`.
#[must_use]
pub fn up_first(targets: &[&str], start: &Path, stop: Option<&Path>) -> Option<PathBuf> {
    up(targets, start, stop).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_path(base: &Path, relative: &str) -> PathBuf {
        relative
            .split('/')
            .filter(|component| !component.is_empty())
            .fold(base.to_path_buf(), |path, component| path.join(component))
    }

    fn tree() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().expect("tempdir");
        let marker = format!(
            ".zuno-walk-test-{}",
            root.path()
                .file_name()
                .expect("temporary directory has a basename")
                .to_string_lossy()
        );
        let deep = fixture_path(root.path(), "a/b/c");
        fs::create_dir_all(&deep).expect("create tree");
        fs::create_dir_all(root.path().join("a").join(&marker)).expect("create a marker");
        fs::create_dir_all(root.path().join("a/b/c").join(&marker)).expect("create c marker");
        fs::write(fixture_path(root.path(), "a/b/zuno.json"), "{}").expect("write json");
        fs::write(fixture_path(root.path(), "a/b/zuno.jsonc"), "{}").expect("write jsonc");
        (root, marker)
    }

    #[test]
    fn collects_nearest_first_up_to_the_root() {
        let (root, marker) = tree();
        let found = up(&[&marker], &fixture_path(root.path(), "a/b/c"), None);
        assert_eq!(
            found,
            vec![
                root.path().join("a/b/c").join(&marker),
                root.path().join("a").join(&marker)
            ]
        );
    }

    #[test]
    fn stop_is_inclusive_and_bounds_the_walk() {
        let (root, marker) = tree();
        let found = up(
            &[&marker],
            &fixture_path(root.path(), "a/b/c"),
            Some(&fixture_path(root.path(), "a/b")),
        );
        assert_eq!(found, vec![root.path().join("a/b/c").join(&marker)]);

        let including_stop = up(
            &[&marker],
            &fixture_path(root.path(), "a/b/c"),
            Some(&fixture_path(root.path(), "a")),
        );
        assert_eq!(
            including_stop,
            vec![
                root.path().join("a/b/c").join(&marker),
                root.path().join("a").join(&marker)
            ]
        );
    }

    /// A `stop` that is not an ancestor of `start` never matches, so the walk
    /// reaches the filesystem root instead of stopping early.
    #[test]
    fn unrelated_stop_does_not_bound_the_walk() {
        let (root, marker) = tree();
        let found = up(
            &[&marker],
            &fixture_path(root.path(), "a/b/c"),
            Some(Path::new("/definitely/not/an/ancestor")),
        );
        assert_eq!(
            found,
            vec![
                root.path().join("a/b/c").join(&marker),
                root.path().join("a").join(&marker)
            ]
        );
    }

    #[test]
    fn targets_are_probed_in_the_given_order() {
        let (root, _marker) = tree();
        let found = up(
            &["zuno.jsonc", "zuno.json"],
            &fixture_path(root.path(), "a/b"),
            Some(&fixture_path(root.path(), "a/b")),
        );
        assert_eq!(
            found,
            vec![
                fixture_path(root.path(), "a/b/zuno.jsonc"),
                fixture_path(root.path(), "a/b/zuno.json")
            ]
        );
    }

    #[test]
    fn returns_empty_when_nothing_matches() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(up(&[".nothing-here"], root.path(), None).is_empty());
        assert_eq!(up_first(&[".nothing-here"], root.path(), None), None);
    }

    #[test]
    fn up_first_takes_the_nearest_hit() {
        let (root, marker) = tree();
        assert_eq!(
            up_first(&[&marker], &fixture_path(root.path(), "a/b/c"), None),
            Some(root.path().join("a/b/c").join(&marker))
        );
    }

    /// Directories are collected as readily as files, because `fs.exists` does
    /// not discriminate.
    #[test]
    fn a_file_target_and_a_directory_target_are_both_collected() {
        let root = tempfile::tempdir().expect("tempdir");
        let nested = root.path().join("x");
        fs::create_dir_all(&nested).expect("create x");
        fs::write(nested.join(".opencode"), "").expect("write file named .opencode");
        assert_eq!(
            up(&[".opencode"], &nested, Some(&nested)),
            vec![nested.join(".opencode")]
        );
    }
}
