//! Resolution of the `references` config surface.
//!
//! The schema types live in [`oc_config::schema::reference`] (todo 7); this module
//! adds the resolved view a picker or a reference loader consumes, plus a parse
//! entry point that names the offending entry when an arm does not match.
//!
//! Oracle: `packages/core/src/config/reference.ts:5-21` — `Record(String, Union([
//! String, Git, Local ]))`, three arms:
//!
//! 1. a bare string,
//! 2. `{ repository, branch?, description?, hidden? }`,
//! 3. `{ path, description?, hidden? }`.
//!
//! Only the plural `references` key feeds this module. The singular `reference`
//! is a deprecated spelling that `oc_config::legacy` rejects
//! (`packages/core/src/v1/config/config.ts:48-50`), so nothing here reads it.

use oc_config::schema::ordered::OrderedMap;
use oc_config::schema::reference::{GitReference, LocalReference, ReferenceEntry};
use oc_error::{ConfigError, ConfigIssue};
use std::path::Path;

/// Where a resolved reference points.
///
/// The bare-string arm is not a third kind: the oracle's loader treats a string
/// as a repository when it looks like one and as a path otherwise, so the
/// shorthand is retained verbatim in [`Self::Shorthand`] for the loader to
/// classify rather than guessed at here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceTarget {
    /// The bare-string arm, kept verbatim.
    Shorthand(String),
    /// A git repository, with the branch the author asked for.
    Git {
        /// The repository to clone.
        repository: String,
        /// The branch to check out, if pinned.
        branch: Option<String>,
    },
    /// A local directory.
    Local {
        /// The directory to reference.
        path: String,
    },
}

/// One reference with its union arm already interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReference {
    /// The key the reference was declared under.
    pub name: String,
    /// Where it points.
    pub target: ReferenceTarget,
    /// Human description, when the author gave one. Never set on the shorthand
    /// arm, which has no room for it.
    pub description: Option<String>,
    /// Whether the reference is hidden from pickers. Absent means visible.
    pub hidden: bool,
}

impl ResolvedReference {
    /// Interpret one entry declared under `name`.
    #[must_use]
    pub fn from_entry(name: &str, entry: &ReferenceEntry) -> Self {
        match entry {
            ReferenceEntry::Shorthand(text) => Self {
                name: name.to_owned(),
                target: ReferenceTarget::Shorthand(text.clone()),
                description: None,
                hidden: false,
            },
            ReferenceEntry::Git(GitReference {
                repository,
                branch,
                description,
                hidden,
            }) => Self {
                name: name.to_owned(),
                target: ReferenceTarget::Git {
                    repository: repository.clone(),
                    branch: branch.clone(),
                },
                description: description.clone(),
                hidden: hidden.unwrap_or(false),
            },
            ReferenceEntry::Local(LocalReference {
                path,
                description,
                hidden,
            }) => Self {
                name: name.to_owned(),
                target: ReferenceTarget::Local { path: path.clone() },
                description: description.clone(),
                hidden: hidden.unwrap_or(false),
            },
        }
    }
}

/// Every reference in declaration order, arms interpreted.
///
/// Built from the already-parsed `references` map so a caller that holds a
/// [`oc_config::schema::Config`] never re-reads the union.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedReferences {
    entries: Vec<ResolvedReference>,
}

impl ResolvedReferences {
    /// Resolve the `references` map. `None` — the key absent — yields no
    /// references, which is the oracle's behaviour for an omitted record.
    #[must_use]
    pub fn resolve(references: Option<&OrderedMap<ReferenceEntry>>) -> Self {
        let Some(map) = references else {
            return Self::default();
        };
        Self {
            entries: map
                .iter()
                .map(|(name, entry)| ResolvedReference::from_entry(name, entry))
                .collect(),
        }
    }

    /// References in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &ResolvedReference> {
        self.entries.iter()
    }

    /// References a picker should show, i.e. those not marked `hidden`.
    pub fn visible(&self) -> impl Iterator<Item = &ResolvedReference> {
        self.entries.iter().filter(|entry| !entry.hidden)
    }

    /// The reference declared under `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ResolvedReference> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// How many references were declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no references were declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<'a> IntoIterator for &'a ResolvedReferences {
    type Item = &'a ResolvedReference;
    type IntoIter = std::slice::Iter<'a, ResolvedReference>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

/// Parse a raw `references` map, naming the entry whose arm did not match.
///
/// `serde`'s untagged unions report "data did not match any variant" without
/// saying *which* key was at fault, which for a map of references is the only
/// detail that makes the message actionable. Deserializing entry by entry keeps
/// the key in hand, so `{"x": {}}` reports `references.x`.
pub fn parse(
    raw: &OrderedMap<serde_json::Value>,
    path: &Path,
) -> Result<ResolvedReferences, ConfigError> {
    let mut entries = Vec::with_capacity(raw.len());
    let mut issues = Vec::new();
    for (name, value) in raw.iter() {
        match serde_json::from_value::<ReferenceEntry>(value.clone()) {
            Ok(entry) => entries.push(ResolvedReference::from_entry(name, &entry)),
            Err(error) => issues.push(ConfigIssue::new(
                ["references", name],
                format!(
                    "reference {name:?} is neither a string, a git reference with 'repository', \
                     nor a local reference with 'path': {error}"
                ),
            )),
        }
    }
    if issues.is_empty() {
        Ok(ResolvedReferences { entries })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            issues,
        })
    }
}

/// Parse a `references` map straight from JSON text.
///
/// A convenience over [`parse`] for callers holding the raw object.
pub fn parse_json(raw: &str, path: &Path) -> Result<ResolvedReferences, ConfigError> {
    let map: OrderedMap<serde_json::Value> =
        serde_json::from_str(raw).map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    parse(&map, path)
}

#[cfg(test)]
mod tests;
