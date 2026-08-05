//! The configuration directory and file chain, ported from
//! `packages/opencode/src/config/paths.ts:10-45`.
//!
//! ```text
//! directories(directory, worktree) = unique([
//!   Global.Path.config,
//!   ...(!OPENCODE_DISABLE_PROJECT_CONFIG
//!         ? up({ targets: [".opencode"], start: directory, stop: worktree })
//!         : []),
//!   ...up({ targets: [".opencode"], start: home, stop: home }),
//!   ...(OPENCODE_CONFIG_DIR ? [OPENCODE_CONFIG_DIR] : []),
//! ])
//!
//! files(name, directory, worktree) =
//!   up({ targets: [`${name}.jsonc`, `${name}.json`], start: directory, stop: worktree })
//!     .toReversed()
//! ```
//!
//! Order is the whole contract: whatever consumes this list decides precedence
//! by position, so a reordering is a silent behaviour change in every layer of
//! config merging. Two properties in particular:
//!
//! - `directories` runs **global first, then project directories nearest-first,
//!   then `$HOME/.opencode`, then `OPENCODE_CONFIG_DIR`** — the walk is *not*
//!   reversed here.
//! - `files` **is** reversed, so the outermost file comes first and the deepest
//!   last. Because [`crate::walk::up`] probes `.jsonc` before `.json` per
//!   directory, reversing makes `.json` precede `.jsonc` within one directory.
//!
//! `unique` is first-occurrence-wins, which matters when the global config
//! directory is itself on the walk (`$HOME/.config/opencode` inside a repository
//! checked out under `$HOME/.config`, for instance): it keeps its leading
//! position rather than being pulled deeper.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::Layout;
use crate::node_path;
use crate::walk;

/// The per-project configuration directory name.
pub const PROJECT_CONFIG_DIRECTORY: &str = ".opencode";

impl Layout {
    /// Port of `ConfigPaths.directories`.
    ///
    /// `worktree` bounds the project walk; `None` lets it run to the filesystem
    /// root, which is what the oracle does when the caller has no worktree.
    #[must_use]
    pub fn config_directories(&self, directory: &Path, worktree: Option<&Path>) -> Vec<PathBuf> {
        let mut ordered = Vec::new();
        ordered.push(self.config().to_path_buf());

        if !self.project_config_disabled() {
            ordered.extend(walk::up(&[PROJECT_CONFIG_DIRECTORY], directory, worktree));
        }

        // `stop === start` makes this a single-directory probe of $HOME, not a
        // walk: the loop searches, compares stop to current, and breaks.
        ordered.extend(walk::up(
            &[PROJECT_CONFIG_DIRECTORY],
            self.home(),
            Some(self.home()),
        ));

        if let Some(extra) = self.config_dir_override().filter(|value| !value.is_empty()) {
            ordered.push(PathBuf::from(extra));
        }

        unique(ordered)
    }

    /// Port of `ConfigPaths.files`, outermost first.
    ///
    /// A free-standing helper on [`Layout`] purely for discoverability — it uses
    /// nothing from `self`, because the oracle's version does not either.
    #[must_use]
    pub fn config_files(name: &str, directory: &Path, worktree: Option<&Path>) -> Vec<PathBuf> {
        let targets = [format!("{name}.jsonc"), format!("{name}.json")];
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();
        let mut found = walk::up(&targets, directory, worktree);
        found.reverse();
        found
    }

    /// Port of `ConfigPaths.fileInDirectory`: `[dir/name.json, dir/name.jsonc]`.
    ///
    /// Note the order is `.json` then `.jsonc`, the opposite of the probe order
    /// used by [`Layout::config_files`]. Both are upstream's; they are not
    /// interchangeable and neither is a typo.
    #[must_use]
    pub fn file_in_directory(directory: &Path, name: &str) -> [PathBuf; 2] {
        let directory = directory.to_string_lossy();
        [
            PathBuf::from(node_path::join(&directory, &format!("{name}.json"))),
            PathBuf::from(node_path::join(&directory, &format!("{name}.jsonc"))),
        ]
    }
}

/// Port of remeda's `unique`: keep the first occurrence, drop later duplicates,
/// preserve order.
fn unique(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::with_capacity(paths.len());
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{
        Env, HOME, OPENCODE_CONFIG_DIR, OPENCODE_DISABLE_PROJECT_CONFIG, XDG_CONFIG_HOME,
    };
    use std::fs;

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        /// A worktree at `<root>/repo` with `.opencode` at the worktree root, at
        /// `repo/a`, and at `repo/a/b/c`, plus a `$HOME/.opencode` and config
        /// files at two depths.
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let path = root.path();
            for directory in [
                "repo/.opencode",
                "repo/a/.opencode",
                "repo/a/b/c/.opencode",
                "home/.opencode",
                "xdgconfig/opencode",
            ] {
                fs::create_dir_all(path.join(directory)).expect("create directory");
            }
            fs::write(path.join("repo/opencode.json"), "{}").expect("write root json");
            fs::write(path.join("repo/a/b/opencode.json"), "{}").expect("write mid json");
            fs::write(path.join("repo/a/b/opencode.jsonc"), "{}").expect("write mid jsonc");
            Self { root }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.root.path().join(relative)
        }

        fn layout(&self, extra: &[(&str, &str)]) -> Layout {
            let mut env = Env::empty()
                .with(HOME, self.path("home").to_string_lossy().into_owned())
                .with(
                    XDG_CONFIG_HOME,
                    self.path("xdgconfig").to_string_lossy().into_owned(),
                );
            for (key, value) in extra {
                env = env.with(*key, *value);
            }
            Layout::resolve_with(&env, None)
        }
    }

    #[test]
    fn directories_run_global_then_nearest_project_then_home_then_override() {
        let fixture = Fixture::new();
        let layout = fixture.layout(&[]);
        let found =
            layout.config_directories(&fixture.path("repo/a/b/c"), Some(&fixture.path("repo")));
        assert_eq!(
            found,
            vec![
                fixture.path("xdgconfig/opencode"),
                fixture.path("repo/a/b/c/.opencode"),
                fixture.path("repo/a/.opencode"),
                fixture.path("repo/.opencode"),
                fixture.path("home/.opencode"),
            ]
        );
    }

    #[test]
    fn the_worktree_stop_is_inclusive_and_bounds_the_project_walk() {
        let fixture = Fixture::new();
        let layout = fixture.layout(&[]);
        let bounded =
            layout.config_directories(&fixture.path("repo/a/b/c"), Some(&fixture.path("repo/a")));
        assert_eq!(
            bounded,
            vec![
                fixture.path("xdgconfig/opencode"),
                fixture.path("repo/a/b/c/.opencode"),
                fixture.path("repo/a/.opencode"),
                fixture.path("home/.opencode"),
            ]
        );
        assert!(!bounded.contains(&fixture.path("repo/.opencode")));
    }

    #[test]
    fn disabling_project_config_drops_only_the_project_walk() {
        let fixture = Fixture::new();
        let layout = fixture.layout(&[(OPENCODE_DISABLE_PROJECT_CONFIG, "1")]);
        let found =
            layout.config_directories(&fixture.path("repo/a/b/c"), Some(&fixture.path("repo")));
        assert_eq!(
            found,
            vec![
                fixture.path("xdgconfig/opencode"),
                fixture.path("home/.opencode")
            ]
        );
    }

    #[test]
    fn a_config_dir_override_is_appended_last_and_an_empty_one_is_dropped() {
        let fixture = Fixture::new();
        let extra = fixture.path("extra-config");
        let layout = fixture.layout(&[(OPENCODE_CONFIG_DIR, extra.to_str().expect("utf8"))]);
        let found = layout.config_directories(&fixture.path("repo"), Some(&fixture.path("repo")));
        assert_eq!(found.last(), Some(&extra));

        let empty = fixture.layout(&[(OPENCODE_CONFIG_DIR, "")]);
        let without = empty.config_directories(&fixture.path("repo"), Some(&fixture.path("repo")));
        assert!(!without.iter().any(|path| path.as_os_str().is_empty()));
    }

    /// The override is not deduplicated away when it repeats an earlier entry —
    /// `unique` keeps the first occurrence, so the entry stays at its original
    /// (earlier) position and the list does not grow.
    #[test]
    fn a_duplicate_override_keeps_its_first_position() {
        let fixture = Fixture::new();
        let global = fixture.path("xdgconfig/opencode");
        let layout = fixture.layout(&[(OPENCODE_CONFIG_DIR, global.to_str().expect("utf8"))]);
        let found = layout.config_directories(&fixture.path("repo"), Some(&fixture.path("repo")));
        assert_eq!(found.first(), Some(&global));
        assert_eq!(found.iter().filter(|path| **path == global).count(), 1);
    }

    /// The home probe is a single directory, never a walk to the root — so a
    /// `.opencode` in a parent of `$HOME` is not picked up.
    #[test]
    fn the_home_probe_does_not_walk_above_home() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path(".opencode")).expect("create sibling-of-home marker");
        let layout = fixture.layout(&[]);
        let found = layout.config_directories(&fixture.path("repo"), Some(&fixture.path("repo")));
        assert!(found.contains(&fixture.path("home/.opencode")));
        assert!(!found.contains(&fixture.path(".opencode")));
    }

    #[test]
    fn files_are_outermost_first_with_json_before_jsonc() {
        let fixture = Fixture::new();
        let found = Layout::config_files(
            "opencode",
            &fixture.path("repo/a/b/c"),
            Some(&fixture.path("repo")),
        );
        assert_eq!(
            found,
            vec![
                fixture.path("repo/opencode.json"),
                fixture.path("repo/a/b/opencode.json"),
                fixture.path("repo/a/b/opencode.jsonc"),
            ]
        );
    }

    #[test]
    fn files_returns_empty_when_nothing_exists() {
        let fixture = Fixture::new();
        let found = Layout::config_files(
            "absent",
            &fixture.path("repo/a/b/c"),
            Some(&fixture.path("repo")),
        );
        assert!(found.is_empty());
    }

    #[test]
    fn file_in_directory_is_json_then_jsonc() {
        assert_eq!(
            Layout::file_in_directory(Path::new("/cfg"), "opencode"),
            [
                PathBuf::from("/cfg/opencode.json"),
                PathBuf::from("/cfg/opencode.jsonc")
            ]
        );
        // Node's join normalizes here too.
        assert_eq!(
            Layout::file_in_directory(Path::new("/cfg/"), "opencode"),
            [
                PathBuf::from("/cfg/opencode.json"),
                PathBuf::from("/cfg/opencode.jsonc")
            ]
        );
    }

    #[test]
    fn unique_preserves_first_occurrence_order() {
        let input = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/a"),
            PathBuf::from("/c"),
            PathBuf::from("/b"),
        ];
        assert_eq!(
            unique(input),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
    }
}
