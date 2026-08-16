//! The configuration directory and file chain, ported from
//! `packages/opencode/src/config/paths.ts:10-45`.
//!
//! The transcription below is the **oracle's**, so it keeps upstream's own
//! `.opencode` and `OPENCODE_DISABLE_PROJECT_CONFIG` spellings verbatim. Zuno
//! walks [`PROJECT_CONFIG_DIRECTORY`] (`.zuno`) and reads
//! `ZUNO_DISABLE_PROJECT_CONFIG`; the shape of the list is what is ported, not
//! the names.
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
//!   then `$HOME/.zuno`, then `OPENCODE_CONFIG_DIR`** — the walk is *not*
//!   reversed here.
//! - `files` **is** reversed, so the outermost file comes first and the deepest
//!   last. Because [`crate::walk::up`] probes `.jsonc` before `.json` per
//!   directory, reversing makes `.json` precede `.jsonc` within one directory.
//!
//! `unique` is first-occurrence-wins, which matters when the global config
//! directory is itself on the walk (`$HOME/.config/zuno` inside a repository
//! checked out under `$HOME/.config`, for instance): it keeps its leading
//! position rather than being pulled deeper.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::Layout;
use crate::node_path;
use crate::walk;

/// The per-project directory Zuno keeps its own project-local state in.
///
/// This is the single definition. Configuration reads it, and so do plan
/// documents (`oc-agent`), the goal projection (`oc-goal`), and the tool-output
/// and background stores (`oc-tools`). Three independent copies existed before,
/// which is how the rename to `.zuno` landed in some of them and not others.
///
/// The plugin ABI is deliberately out of scope: `engines.opencode` and the six
/// `OPENCODE_*` handshake variables are a contract with someone else's code and
/// keep the upstream name.
pub const PROJECT_DIRECTORY: &str = ".zuno";

/// The per-project configuration directory name, which is [`PROJECT_DIRECTORY`].
pub const PROJECT_CONFIG_DIRECTORY: &str = PROJECT_DIRECTORY;

/// The pre-rename project directory name, used only to detect unmigrated state.
pub const LEGACY_PROJECT_DIRECTORY: &str = ".opencode";

/// The pre-rename path of a project-local state file that was never migrated.
///
/// | `.zuno` file | `.opencode` file | result |
/// |---|---|---|
/// | absent | absent | `None` — an ordinary new project |
/// | absent | present | `Some(old)` — the state a diagnostic must name |
/// | present | absent | `None` — migrated |
/// | present | present | `None` — the new file wins, as everywhere else |
///
/// A path with no `.zuno` component, or more than one, returns `None` rather
/// than guessing which component was meant.
///
/// Detection only: it does not read, copy, merge, or fall back to the legacy
/// file — the same hard cut [`crate::legacy_db_path`] makes for the database
/// filename.
#[must_use]
pub fn unmigrated_project_path(new_path: &Path) -> Option<PathBuf> {
    if new_path.exists() {
        return None;
    }
    let mut rebuilt = PathBuf::new();
    let mut swapped = 0_usize;
    for component in new_path.components() {
        if component.as_os_str() == std::ffi::OsStr::new(PROJECT_DIRECTORY) {
            swapped += 1;
            rebuilt.push(LEGACY_PROJECT_DIRECTORY);
        } else {
            rebuilt.push(component);
        }
    }
    if swapped != 1 {
        return None;
    }
    rebuilt.exists().then_some(rebuilt)
}

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
        Env, HOME, OPENCODE_CONFIG_DIR, XDG_CONFIG_HOME, ZUNO_DISABLE_PROJECT_CONFIG,
    };
    use std::fs;

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        /// A worktree at `<root>/repo` with `.zuno` at the worktree root, at
        /// `repo/a`, and at `repo/a/b/c`, plus a `$HOME/.zuno` and config
        /// files at two depths.
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let path = root.path();
            for directory in [
                "repo/.zuno",
                "repo/a/.zuno",
                "repo/a/b/c/.zuno",
                "home/.zuno",
                "xdgconfig/zuno",
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

    fn project_marker_membership(old_present: bool, new_present: bool) -> (bool, bool) {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        let current = repo.join("nested");
        let home = root.path().join("home");
        let xdg_config = root.path().join("xdg-config");
        for directory in [&current, &home, &xdg_config] {
            fs::create_dir_all(directory).expect("create fixture directory");
        }
        if old_present {
            fs::create_dir_all(repo.join(".opencode")).expect("create old project config");
        }
        if new_present {
            fs::create_dir_all(repo.join(".zuno")).expect("create Zuno project config");
        }

        let env = Env::empty()
            .with(HOME, home.to_string_lossy().into_owned())
            .with(XDG_CONFIG_HOME, xdg_config.to_string_lossy().into_owned());
        let found = Layout::resolve_with(&env, None).config_directories(&current, Some(&repo));
        (
            found.contains(&repo.join(".opencode")),
            found.contains(&repo.join(".zuno")),
        )
    }

    #[test]
    fn zuno_path_matrix_ignores_an_old_only_project_directory() {
        assert_eq!(project_marker_membership(true, false), (false, false));
    }

    #[test]
    fn zuno_path_matrix_discovers_a_new_only_project_directory() {
        assert_eq!(project_marker_membership(false, true), (false, true));
    }

    #[test]
    fn zuno_path_matrix_uses_only_new_when_both_project_directories_exist() {
        assert_eq!(project_marker_membership(true, true), (false, true));
    }

    #[test]
    fn zuno_path_matrix_adds_no_project_directory_when_neither_exists() {
        assert_eq!(project_marker_membership(false, false), (false, false));
    }

    fn unmigrated_state(old_present: bool, new_present: bool) -> (Option<PathBuf>, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("repo");
        let new_path = repo.join(PROJECT_DIRECTORY).join("plans").join("plan.md");
        let old_path = repo
            .join(LEGACY_PROJECT_DIRECTORY)
            .join("plans")
            .join("plan.md");
        for (present, path) in [(old_present, &old_path), (new_present, &new_path)] {
            if present {
                fs::create_dir_all(path.parent().expect("parent")).expect("create fixture parent");
                fs::write(path, "body").expect("write fixture document");
            }
        }
        (unmigrated_project_path(&new_path), old_path)
    }

    #[test]
    fn unmigrated_project_path_reports_nothing_when_neither_document_exists() {
        assert_eq!(unmigrated_state(false, false).0, None);
    }

    #[test]
    fn unmigrated_project_path_reports_the_legacy_document_when_only_it_exists() {
        let (found, old_path) = unmigrated_state(true, false);
        assert_eq!(found, Some(old_path));
    }

    #[test]
    fn unmigrated_project_path_reports_nothing_when_only_the_new_document_exists() {
        assert_eq!(unmigrated_state(false, true).0, None);
    }

    #[test]
    fn unmigrated_project_path_reports_nothing_when_both_documents_exist() {
        assert_eq!(unmigrated_state(true, true).0, None);
    }

    #[test]
    fn unmigrated_project_path_refuses_a_path_with_no_project_directory_component() {
        let root = tempfile::tempdir().expect("tempdir");
        let legacy = root.path().join("plans");
        fs::create_dir_all(&legacy).expect("create fixture parent");
        fs::write(legacy.join("plan.md"), "body").expect("write fixture document");
        assert_eq!(
            unmigrated_project_path(&root.path().join("plans").join("absent.md")),
            None
        );
    }

    #[test]
    fn unmigrated_project_path_refuses_a_path_with_two_project_directory_components() {
        let root = tempfile::tempdir().expect("tempdir");
        let ambiguous = root
            .path()
            .join(PROJECT_DIRECTORY)
            .join(PROJECT_DIRECTORY)
            .join("plan.md");
        let one_swap = root
            .path()
            .join(LEGACY_PROJECT_DIRECTORY)
            .join(PROJECT_DIRECTORY);
        fs::create_dir_all(&one_swap).expect("create fixture parent");
        fs::write(one_swap.join("plan.md"), "body").expect("write fixture document");
        assert_eq!(unmigrated_project_path(&ambiguous), None);
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
                fixture.path("xdgconfig/zuno"),
                fixture.path("repo/a/b/c/.zuno"),
                fixture.path("repo/a/.zuno"),
                fixture.path("repo/.zuno"),
                fixture.path("home/.zuno"),
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
                fixture.path("xdgconfig/zuno"),
                fixture.path("repo/a/b/c/.zuno"),
                fixture.path("repo/a/.zuno"),
                fixture.path("home/.zuno"),
            ]
        );
        assert!(!bounded.contains(&fixture.path("repo/.zuno")));
    }

    #[test]
    fn disabling_project_config_drops_only_the_project_walk() {
        let fixture = Fixture::new();
        let layout = fixture.layout(&[(ZUNO_DISABLE_PROJECT_CONFIG, "1")]);
        let found =
            layout.config_directories(&fixture.path("repo/a/b/c"), Some(&fixture.path("repo")));
        assert_eq!(
            found,
            vec![fixture.path("xdgconfig/zuno"), fixture.path("home/.zuno")]
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
        let global = fixture.path("xdgconfig/zuno");
        let layout = fixture.layout(&[(OPENCODE_CONFIG_DIR, global.to_str().expect("utf8"))]);
        let found = layout.config_directories(&fixture.path("repo"), Some(&fixture.path("repo")));
        assert_eq!(found.first(), Some(&global));
        assert_eq!(found.iter().filter(|path| **path == global).count(), 1);
    }

    /// The home probe is a single directory, never a walk to the root — so a
    /// `.zuno` in a parent of `$HOME` is not picked up.
    #[test]
    fn the_home_probe_does_not_walk_above_home() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path(".zuno")).expect("create sibling-of-home marker");
        let layout = fixture.layout(&[]);
        let found = layout.config_directories(&fixture.path("repo"), Some(&fixture.path("repo")));
        assert!(found.contains(&fixture.path("home/.zuno")));
        assert!(!found.contains(&fixture.path(".zuno")));
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
