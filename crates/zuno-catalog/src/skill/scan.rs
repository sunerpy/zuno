//! Deterministic `SKILL.md` discovery for Zuno-owned, Agent-shared, and
//! explicitly configured roots.
//!
//! Three root classes have intentionally different patterns:
//!
//! ```text
//! AGENT_SKILL_PATTERN = "skills/**/SKILL.md"          // ~/.agents and project .agents
//! ZUNO_SKILL_PATTERN  = "skill/**/SKILL.md"           // every Zuno config directory
//! SKILL_PATTERN       = "**/SKILL.md"                  // skills.paths[], pulled URLs
//! ```
//!
//! Scans return absolute files, follow directory symlinks, and control hidden
//! directory traversal per root class:
//!
//! * `**` descends into symlinked directories. A
//!   `~/.agents/skills/link -> /elsewhere/skill` is found, while later canonical
//!   source de-duplication prevents the target from being advertised twice.
//! * A `SKILL.md` that is itself a symlink to a file still matches.
//! * Hidden traversal is enabled for Agent-shared roots and disabled for Zuno
//!   config directories and `skills.paths[]`. A
//!   `.hidden/x/SKILL.md` under a config directory is invisible; a
//!   `.dotdir/x/SKILL.md` under `~/.agents/skills` is not.
//!
//! `**` also matches zero segments, so `~/.agents/skills/SKILL.md` is a match.
//!
//! Matches are sorted before they are returned, so discovery and ambiguity
//! reporting are reproducible across filesystems.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// The filename every skill definition must have.
pub const SKILL_FILENAME: &str = "SKILL.md";

/// Subdirectory prefixes for `skills/**/SKILL.md`.
pub const EXTERNAL_PREFIXES: &[&str] = &["skills"];

/// The canonical Zuno-owned Skill subdirectory.
pub const ZUNO_PREFIXES: &[&str] = &["skill"];

/// No prefix at all: `**/SKILL.md` from the root itself.
pub const ROOT_PREFIXES: &[&str] = &[""];

/// One scan's outcome: the matches, plus whatever went wrong on the way.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Absolute paths of every `SKILL.md` found, sorted.
    pub matches: Vec<PathBuf>,
    /// Directories that could not be traversed, with the reason.
    ///
    /// A missing root is not in here: node-glob returns `[]` for a `cwd` that
    /// does not exist, and the oracle's un-scoped `scan` calls would `die` if it
    /// did otherwise.
    pub errors: Vec<(PathBuf, io::ErrorKind)>,
}

/// Find every `SKILL.md` under `root/<prefix>` for each prefix.
///
/// `dot` mirrors node-glob's option: when false, a path segment beginning with
/// `.` is not traversed.
#[must_use]
pub fn scan(root: &Path, prefixes: &[&str], dot: bool) -> ScanResult {
    let mut matches = BTreeSet::new();
    let mut errors = Vec::new();

    for prefix in prefixes {
        let base = if prefix.is_empty() {
            root.to_path_buf()
        } else {
            root.join(prefix)
        };

        let walk = WalkDir::new(&base)
            .follow_links(true)
            .into_iter()
            .filter_entry(|entry| dot || entry.depth() == 0 || !hidden(entry.file_name()));

        for entry in walk {
            match entry {
                Ok(entry) => {
                    if entry.file_name() == OsStr::new(SKILL_FILENAME)
                        && entry.file_type().is_file()
                    {
                        matches.insert(entry.into_path());
                    }
                }
                Err(error) => {
                    let at = error.path().unwrap_or(&base).to_path_buf();
                    let kind = error
                        .io_error()
                        .map_or(io::ErrorKind::Other, io::Error::kind);
                    // An absent root is the normal case for most config
                    // directories, and node-glob is silent about it.
                    if kind != io::ErrorKind::NotFound {
                        errors.push((at, kind));
                    }
                }
            }
        }
    }

    ScanResult {
        matches: matches.into_iter().collect(),
        errors,
    }
}

fn hidden(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(root: &Path, relative: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        fs::write(&path, "---\nname: x\n---\nB\n").expect("write");
        path
    }

    #[test]
    fn double_star_matches_zero_segments() {
        let dir = TempDir::new().expect("tempdir");
        let at_root = write(dir.path(), "skills/SKILL.md");
        let nested = write(dir.path(), "skills/a/b/SKILL.md");
        let found = scan(dir.path(), EXTERNAL_PREFIXES, true);
        assert_eq!(found.matches, vec![at_root, nested]);
    }

    #[test]
    fn dot_false_prunes_hidden_directories() {
        let dir = TempDir::new().expect("tempdir");
        let visible = write(dir.path(), "skill/a/SKILL.md");
        write(dir.path(), "skill/.hidden/b/SKILL.md");
        let found = scan(dir.path(), ZUNO_PREFIXES, false);
        assert_eq!(found.matches, vec![visible]);
    }

    #[test]
    fn dot_true_keeps_hidden_directories() {
        let dir = TempDir::new().expect("tempdir");
        write(dir.path(), "skills/a/SKILL.md");
        write(dir.path(), "skills/.hidden/b/SKILL.md");
        let found = scan(dir.path(), EXTERNAL_PREFIXES, true);
        assert_eq!(found.matches.len(), 2);
    }

    #[test]
    fn only_the_canonical_singular_zuno_prefix_is_scanned() {
        let dir = TempDir::new().expect("tempdir");
        let singular = write(dir.path(), "skill/a/SKILL.md");
        write(dir.path(), "skills/b/SKILL.md");
        let found = scan(dir.path(), ZUNO_PREFIXES, false);
        assert_eq!(found.matches, vec![singular]);
    }

    #[test]
    fn missing_root_is_silent() {
        let dir = TempDir::new().expect("tempdir");
        let found = scan(&dir.path().join("absent"), ROOT_PREFIXES, false);
        assert!(found.matches.is_empty());
        assert!(found.errors.is_empty(), "{:?}", found.errors);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_followed_and_reported_by_link_path() {
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("target/real");
        fs::create_dir_all(&target).expect("mkdir");
        fs::write(target.join(SKILL_FILENAME), "---\nname: x\n---\n").expect("write");
        fs::create_dir_all(dir.path().join("skills")).expect("mkdir");
        std::os::unix::fs::symlink(&target, dir.path().join("skills/linked")).expect("symlink");

        let found = scan(dir.path(), EXTERNAL_PREFIXES, true);
        assert_eq!(
            found.matches,
            vec![dir.path().join("skills/linked").join(SKILL_FILENAME)]
        );
    }

    #[test]
    fn other_filenames_are_ignored() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join("skills/a")).expect("mkdir");
        fs::write(dir.path().join("skills/a/README.md"), "no").expect("write");
        fs::write(dir.path().join("skills/a/skill.md"), "no").expect("write");
        assert!(scan(dir.path(), EXTERNAL_PREFIXES, true).matches.is_empty());
    }
}
