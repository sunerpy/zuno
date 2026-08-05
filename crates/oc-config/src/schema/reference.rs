//! Named git or local directory references.
//!
//! Oracle: `packages/core/src/config/reference.ts:5-21` — a three-way union of a
//! bare string, a git reference, and a local-path reference.

use serde::{Deserialize, Serialize};

/// One entry of the `references` map.
///
/// The arms are disjoint: a string is a string, [`GitReference`] requires
/// `repository`, and [`LocalReference`] requires `path`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReferenceEntry {
    /// The shorthand form: a bare repository or path string.
    Shorthand(String),
    /// A git repository reference.
    Git(GitReference),
    /// A local directory reference.
    Local(LocalReference),
}

/// A reference to a git repository (`config/reference.ts:5-10`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitReference {
    /// The repository to clone.
    pub repository: String,
    /// The branch to check out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Human description of the reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hide the reference from pickers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// A reference to a local directory (`config/reference.ts:12-16`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalReference {
    /// The directory to reference.
    pub path: String,
    /// Human description of the reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Hide the reference from pickers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}
