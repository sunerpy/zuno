//! The declared-divergence allow-list, as data a gate can consult.
//!
//! # Why a file and not a doc comment
//!
//! Nine waves of this port produced deliberate differences from upstream
//! `opencode`. Each was, at the time, recorded in prose — a module header, a
//! notepad entry, a line in the plan. Prose has one fatal property for this job:
//! a tenth difference can be introduced without anything noticing, because there
//! is no reader whose failure to find an entry is an error.
//!
//! `docs/divergences.toml` inverts that. The compatibility suite loads it,
//! compares its entry count against [`DECLARED_COUNT`], and — where the entry
//! carries a [`ExecuteContract`] — compares the declaration against the *live*
//! artifact. A divergence is then something the build knows about, and an
//! undeclared one fails.
//!
//! # Why the count is a constant here rather than only in the test
//!
//! The count has to be edited in the same commit that edits the file, and it has
//! to be visible to more than one consumer: plan todo 92's documentation test
//! asserts every entry appears on the divergence page, and plan todo 103 is
//! required to add an eighth entry when the memory subsystem lands. A single
//! `const` that both read is the smallest thing that cannot drift.
//!
//! # What does *not* belong in the file
//!
//! A surface that is merely unimplemented is a **gap**, not a divergence. Writing
//! it here would convert an omission into a decision by fiat, which is exactly
//! the laundering the plan's "must not normalize away a real difference" forbids.
//! Gaps are reported by [`crate::compat_report`] instead, where they read as what
//! they are.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, TestkitError};

/// How many entries `docs/divergences.toml` is expected to declare.
///
/// Bumping this without adding an entry — or adding an entry without bumping this
/// — fails the compatibility suite. That is the point.
///
/// Went from eight to twelve in plan todo 119, which reconciled the four
/// behavioural differences that had been kept in a second reporting structure
/// (`compat_suite.rs::nominated_divergences`) that positively asserted they stayed
/// *out* of the allow-list. Two of the six nominations were merged into entries
/// that already covered them rather than declared twice.
pub const DECLARED_COUNT: usize = 12;

/// The allow-list's path, relative to the workspace root.
pub const RELATIVE_PATH: &str = "docs/divergences.toml";

/// The entry id whose declaration is checked against a live schema.
///
/// Named as a constant because two places need it — the loader's lookup and the
/// suite's assertion — and a typo in either would silently skip the only
/// divergence the plan requires to be *verified* rather than merely declared.
pub const EXECUTE_CONTRACT_ID: &str = "execute-parameter-contract";

/// One declared divergence.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredDivergence {
    /// Stable identifier. Referenced by the documentation test, so renaming one
    /// is a visible edit rather than a silent one.
    pub id: String,
    /// The surface a reader would have to look at to observe the difference.
    pub surface: String,
    /// One line saying why the difference exists. Required: an entry without a
    /// reason is how an allow-list becomes a place to hide things.
    pub reason: String,
    /// Machine-checkable shape, present only where the suite verifies the
    /// declaration against a live artifact.
    #[serde(default)]
    pub contract: Option<ExecuteContract>,
}

/// The `execute` tool's parameter contract, as declared.
///
/// Every field is a sorted list of property names, because the assertion this
/// feeds is about *what the model sees*: a renamed or dropped property changes
/// the contract even when the types are unchanged.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteContract {
    /// Upstream's top-level property names (`packages/opencode/src/tool/code-mode.ts:12-20`).
    pub upstream_properties: Vec<String>,
    /// Upstream's required property names.
    pub upstream_required: Vec<String>,
    /// This implementation's top-level property names, including the ones the
    /// central augmentation injects.
    pub properties: Vec<String>,
    /// This implementation's required property names.
    pub required: Vec<String>,
    /// The control property names on one sub-call. Tool-specific arguments are
    /// flattened in beside these, so this is a *subset* assertion, not equality.
    pub subcall_properties: Vec<String>,
}

/// The parsed allow-list.
#[derive(Debug, Clone)]
pub struct DivergenceList {
    entries: Vec<DeclaredDivergence>,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(default)]
    divergence: Vec<DeclaredDivergence>,
}

impl DivergenceList {
    /// Loads the allow-list from the workspace's `docs/divergences.toml`.
    ///
    /// # Errors
    ///
    /// Fails when the workspace root cannot be located, the file is unreadable,
    /// the TOML does not parse, or any entry has an empty required field. It does
    /// **not** check the count — that is the suite's assertion, so the count and
    /// the message naming it live next to each other in the test.
    pub fn load() -> Result<Self> {
        let root = crate::subject::workspace_root().ok_or_else(|| TestkitError::Io {
            action: "locate the workspace root for the divergence allow-list".to_owned(),
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no ancestor Cargo.toml declares [workspace]",
            ),
        })?;
        Self::load_from(&root.join(RELATIVE_PATH))
    }

    /// Loads the allow-list from an explicit path.
    ///
    /// Exists so the mutation proof can point the loader at a perturbed copy
    /// without editing the committed file.
    ///
    /// # Errors
    ///
    /// As [`Self::load`].
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| TestkitError::io("read the divergence allow-list", path, source))?;
        let document: Document =
            toml::from_str(&text).map_err(|source| TestkitError::DivergenceDecode {
                path: path.to_path_buf(),
                detail: source.to_string(),
            })?;
        let list = Self {
            entries: document.divergence,
            path: path.to_path_buf(),
        };
        list.validate()?;
        Ok(list)
    }

    /// Rejects an entry that would weaken the allow-list's guarantee.
    ///
    /// An empty `reason` is the failure mode worth catching: it turns the file
    /// from a record of decisions into a list of exceptions.
    fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            for (field, value) in [
                ("id", &entry.id),
                ("surface", &entry.surface),
                ("reason", &entry.reason),
            ] {
                if value.trim().is_empty() {
                    return Err(TestkitError::DivergenceShape {
                        path: self.path.clone(),
                        detail: format!(
                            "entry {} (id {:?}) has an empty `{field}`; every entry must carry an id, a surface, and a reason",
                            index + 1,
                            entry.id
                        ),
                    });
                }
            }
            if !seen.insert(entry.id.as_str()) {
                return Err(TestkitError::DivergenceShape {
                    path: self.path.clone(),
                    detail: format!(
                        "duplicate divergence id {:?}; ids are referenced by the documentation test and must be unique",
                        entry.id
                    ),
                });
            }
        }
        Ok(())
    }

    /// Every entry, in file order.
    #[must_use]
    pub fn entries(&self) -> &[DeclaredDivergence] {
        &self.entries
    }

    /// How many entries the file declares.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the file declares no entries at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The file the entries were read from, for failure messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every declared id, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.entries.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        ids
    }

    /// The entry with this id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&DeclaredDivergence> {
        self.entries.iter().find(|entry| entry.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_allow_list_parses_and_every_entry_carries_all_three_fields() {
        let list = DivergenceList::load().expect("the committed allow-list must parse");
        assert!(
            !list.is_empty(),
            "the allow-list parsed to zero entries, which would pass every downstream \
             assertion vacuously; the loader is reading the wrong file"
        );
        for entry in list.entries() {
            assert!(!entry.id.trim().is_empty());
            assert!(!entry.surface.trim().is_empty());
            assert!(!entry.reason.trim().is_empty());
        }
    }

    #[test]
    fn an_entry_missing_a_reason_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("divergences.toml");
        std::fs::write(
            &path,
            "[[divergence]]\nid = \"x\"\nsurface = \"y\"\nreason = \"   \"\n",
        )
        .expect("write");
        let error = DivergenceList::load_from(&path).expect_err("an empty reason must be rejected");
        assert!(
            error.to_string().contains("empty `reason`"),
            "message must name the offending field: {error}"
        );
    }

    #[test]
    fn a_duplicate_id_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("divergences.toml");
        std::fs::write(
            &path,
            "[[divergence]]\nid = \"x\"\nsurface = \"a\"\nreason = \"b\"\n\
             [[divergence]]\nid = \"x\"\nsurface = \"c\"\nreason = \"d\"\n",
        )
        .expect("write");
        let error = DivergenceList::load_from(&path).expect_err("a duplicate id must be rejected");
        assert!(
            error.to_string().contains("duplicate divergence id"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_silently_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("divergences.toml");
        std::fs::write(
            &path,
            "[[divergence]]\nid = \"x\"\nsurface = \"a\"\nreason = \"b\"\nnote = \"c\"\n",
        )
        .expect("write");
        let error = DivergenceList::load_from(&path).expect_err("unknown keys must be rejected");
        assert!(
            matches!(error, TestkitError::DivergenceDecode { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_execute_entry_declares_a_machine_checkable_contract() {
        let list = DivergenceList::load().expect("load");
        let entry = list
            .find(EXECUTE_CONTRACT_ID)
            .expect("the execute divergence must exist, since the suite verifies it");
        let contract = entry
            .contract
            .as_ref()
            .expect("the execute entry must carry a contract the suite can check");
        assert_eq!(contract.upstream_properties, ["code"]);
    }
}
