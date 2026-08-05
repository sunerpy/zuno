//! Which sessions reference which store.
//!
//! A snapshot store is **shared**. Every session working in the same
//! `(project id, worktree)` pair writes into one store and addresses its snapshots
//! by tree hash, so deleting a store because one session ended would destroy
//! another session's ability to revert.
//!
//! This module answers the question Todo 83's reference-counted artifact GC has to
//! ask before it deletes anything: *given the sessions that still exist, which
//! stores on disk are still referenced, and by whom?* It reads the filesystem and
//! counts. **It deletes nothing** — the decision and the deletion belong to the
//! caller, together with the disk-space accounting and the dry-run surface.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, SnapshotError};

/// The identity of one store: the two path components under the snapshot root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StoreKey {
    /// The project id — the first component under the snapshot root.
    pub project_id: String,
    /// `sha1` of the worktree path string — the second component.
    pub worktree_hash: String,
}

impl StoreKey {
    /// The key for a worktree, hashing it the way the oracle does.
    #[must_use]
    pub fn new(project_id: impl Into<String>, worktree: &Path) -> Self {
        Self {
            project_id: project_id.into(),
            worktree_hash: oc_paths::Layout::worktree_hash(worktree),
        }
    }

    /// The key for an already-hashed worktree, as read back off disk.
    #[must_use]
    pub fn from_components(
        project_id: impl Into<String>,
        worktree_hash: impl Into<String>,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            worktree_hash: worktree_hash.into(),
        }
    }

    /// Where this store lives under `root`.
    #[must_use]
    pub fn path_in(&self, root: &Path) -> PathBuf {
        root.join(&self.project_id).join(&self.worktree_hash)
    }
}

/// One session's claim on a store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRef {
    /// The session id.
    pub session_id: String,
    /// The session's project id.
    pub project_id: String,
    /// The session's worktree root.
    pub worktree: PathBuf,
}

impl SessionRef {
    /// A claim by `session_id` on the store for `(project_id, worktree)`.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        project_id: impl Into<String>,
        worktree: impl Into<PathBuf>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            project_id: project_id.into(),
            worktree: worktree.into(),
        }
    }

    /// The store this session references.
    #[must_use]
    pub fn key(&self) -> StoreKey {
        StoreKey::new(self.project_id.clone(), &self.worktree)
    }
}

/// A store together with the sessions that reference it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreReferences {
    /// The store's identity.
    pub key: StoreKey,
    /// Where it lives.
    pub path: PathBuf,
    /// Whether the directory actually exists. A referenced store that has not been
    /// created yet is reported with `on_disk: false`.
    pub on_disk: bool,
    /// The ids of the sessions that reference it, deduplicated and sorted.
    pub sessions: BTreeSet<String>,
}

impl StoreReferences {
    /// How many distinct sessions reference this store.
    #[must_use]
    pub fn count(&self) -> usize {
        self.sessions.len()
    }

    /// Whether any session still references this store.
    #[must_use]
    pub fn is_referenced(&self) -> bool {
        !self.sessions.is_empty()
    }
}

/// Whether `name` looks like a worktree hash: 40 lowercase hex digits.
///
/// Store discovery is deliberately strict about this. A GC consumer acts on what
/// this returns, so a stray directory that is not shaped like a store is reported
/// as no store at all rather than as a deletion candidate.
#[must_use]
pub fn is_worktree_hash(name: &str) -> bool {
    name.len() == 40
        && name
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Every store present under `root`.
///
/// A missing root is not an error — it means no snapshot has ever been taken.
pub fn discover_stores(root: &Path) -> Result<Vec<StoreKey>> {
    let projects = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(SnapshotError::Scan {
                root: root.to_path_buf(),
                source,
            });
        }
    };

    let mut found = Vec::new();
    for project in projects {
        let project = project.map_err(|source| SnapshotError::Scan {
            root: root.to_path_buf(),
            source,
        })?;
        if !project.path().is_dir() {
            continue;
        }
        let Some(project_id) = project.file_name().to_str().map(str::to_owned) else {
            continue;
        };

        let stores = fs::read_dir(project.path()).map_err(|source| SnapshotError::Scan {
            root: project.path(),
            source,
        })?;
        for store in stores {
            let store = store.map_err(|source| SnapshotError::Scan {
                root: project.path(),
                source,
            })?;
            if !store.path().is_dir() {
                continue;
            }
            let Some(hash) = store.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_worktree_hash(&hash) {
                continue;
            }
            found.push(StoreKey::from_components(project_id.clone(), hash));
        }
    }

    found.sort();
    Ok(found)
}

/// Reference-count every store: the ones on disk under `root` plus the ones
/// `sessions` reference, each with the set of sessions that reference it.
///
/// Sorted by key, so the output is stable across runs. Deletes nothing.
pub fn reference_counts<I>(root: &Path, sessions: I) -> Result<Vec<StoreReferences>>
where
    I: IntoIterator<Item = SessionRef>,
{
    let mut counts: BTreeMap<StoreKey, BTreeSet<String>> = BTreeMap::new();
    for key in discover_stores(root)? {
        counts.entry(key).or_default();
    }
    for session in sessions {
        counts
            .entry(session.key())
            .or_default()
            .insert(session.session_id);
    }

    Ok(counts
        .into_iter()
        .map(|(key, sessions)| {
            let path = key.path_in(root);
            StoreReferences {
                on_disk: path.is_dir(),
                key,
                path,
                sessions,
            }
        })
        .collect())
}

/// The stores under `root` that exist but no surviving session references.
///
/// A **query**, not a sweep: it opens nothing, locks nothing and removes nothing.
/// Todo 83 owns the decision to delete, the dry-run surface and the byte
/// accounting; this exists so that decision is made against a count rather than a
/// guess.
pub fn unreferenced_stores<I>(root: &Path, sessions: I) -> Result<Vec<StoreReferences>>
where
    I: IntoIterator<Item = SessionRef>,
{
    Ok(reference_counts(root, sessions)?
        .into_iter()
        .filter(|store| store.on_disk && !store.is_referenced())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_dir(root: &Path, project: &str, worktree: &Path) -> StoreKey {
        let key = StoreKey::new(project, worktree);
        fs::create_dir_all(key.path_in(root)).expect("create store");
        key
    }

    #[test]
    fn a_worktree_hash_is_forty_lowercase_hex_digits() {
        assert!(is_worktree_hash("8aef7515c5d209dba568d82622ef121a8e84cd17"));
        assert!(!is_worktree_hash(
            "8AEF7515C5D209DBA568D82622EF121A8E84CD17"
        ));
        assert!(!is_worktree_hash("8aef7515"));
        assert!(!is_worktree_hash("tmp"));
        assert!(!is_worktree_hash(
            "8aef7515c5d209dba568d82622ef121a8e84cd17extra"
        ));
    }

    #[test]
    fn a_missing_root_has_no_stores() {
        let root = Path::new("/definitely/not/here/snapshot");
        assert_eq!(discover_stores(root).expect("scan"), Vec::new());
        assert_eq!(
            reference_counts(root, Vec::new()).expect("count"),
            Vec::new()
        );
    }

    #[test]
    fn discovery_skips_directories_that_are_not_store_shaped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let key = store_dir(root, "proj", Path::new("/w"));
        fs::create_dir_all(root.join("proj").join("not-a-hash")).expect("junk dir");
        fs::write(root.join("loose-file"), b"x").expect("junk file");

        assert_eq!(discover_stores(root).expect("scan"), vec![key]);
    }

    #[test]
    fn two_sessions_in_one_worktree_share_one_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let key = store_dir(root, "proj", Path::new("/w"));

        let counts = reference_counts(
            root,
            vec![
                SessionRef::new("ses_a", "proj", "/w"),
                SessionRef::new("ses_b", "proj", "/w"),
            ],
        )
        .expect("count");

        assert_eq!(counts.len(), 1, "one store, not one per session");
        assert_eq!(counts[0].key, key);
        assert_eq!(counts[0].count(), 2);
        assert!(counts[0].on_disk);
        assert!(counts[0].is_referenced());
    }

    #[test]
    fn a_different_worktree_in_the_same_project_is_a_different_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        store_dir(root, "proj", Path::new("/w"));
        store_dir(root, "proj", Path::new("/other"));

        let counts =
            reference_counts(root, vec![SessionRef::new("ses_a", "proj", "/w")]).expect("count");
        assert_eq!(counts.len(), 2);
        let referenced: Vec<usize> = counts.iter().map(StoreReferences::count).collect();
        assert_eq!(referenced.iter().sum::<usize>(), 1);
    }

    #[test]
    fn an_unreferenced_store_is_reported_but_not_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let live = store_dir(root, "proj", Path::new("/w"));
        let dead = store_dir(root, "proj", Path::new("/gone"));

        let dead_stores =
            unreferenced_stores(root, vec![SessionRef::new("ses_a", "proj", "/w")]).expect("query");

        assert_eq!(dead_stores.len(), 1);
        assert_eq!(dead_stores[0].key, dead);
        assert!(
            dead.path_in(root).is_dir(),
            "the query must not delete the store it reports"
        );
        assert!(live.path_in(root).is_dir());
    }

    #[test]
    fn a_referenced_store_that_does_not_exist_yet_is_reported_as_absent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let counts = reference_counts(
            temp.path(),
            vec![SessionRef::new("ses_a", "proj", "/never-tracked")],
        )
        .expect("count");

        assert_eq!(counts.len(), 1);
        assert!(!counts[0].on_disk);
        assert!(counts[0].is_referenced());
        assert!(
            unreferenced_stores(temp.path(), Vec::new())
                .expect("query")
                .is_empty()
        );
    }

    #[test]
    fn the_key_matches_the_observed_store_path() {
        // Observed from the real binary under a temporary XDG_DATA_HOME:
        // snapshot/441e…/8aef7515c5d209dba568d82622ef121a8e84cd17 for worktree
        // /tmp/opencode/obs23/wt.
        let key = StoreKey::new(
            "441e482eeb9a551532d4ef8320edce003aad21d3",
            Path::new("/tmp/opencode/obs23/wt"),
        );
        assert_eq!(
            key.worktree_hash,
            "8aef7515c5d209dba568d82622ef121a8e84cd17"
        );
        assert_eq!(
            key.path_in(Path::new("/data/snapshot")),
            Path::new(
                "/data/snapshot/441e482eeb9a551532d4ef8320edce003aad21d3/8aef7515c5d209dba568d82622ef121a8e84cd17"
            )
        );
    }
}
