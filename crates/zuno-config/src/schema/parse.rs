//! Turning JSON into a [`Config`], with the failing key path named.
//!
//! Mirrors `packages/opencode/src/config/parse.ts`: unrecognized top-level keys
//! are rejected before validation (`:40-53`), and a validation failure carries the
//! key path of the offending value (`:59-71`).
//!
//! # How the key path is recovered
//!
//! `serde_json` reports a line and column, not a path, and the crate that adds
//! paths (`serde_path_to_error`) is not among this workspace's pinned
//! dependencies. [`locate_failure`] recovers the path from what is available: it
//! removes one candidate key at a time from a copy of the document and re-runs the
//! deserializer, and the key whose removal makes the error go away is the offending
//! one. A required key cannot be removed without breaking the document for a second
//! reason, so it is instead overwritten with each of [`PROBE_VALUES`]. Recursing into
//! the key that is found produces the full path. This runs only on the failure path,
//! where an extra pass over a config-sized document costs nothing.
//!
//! The one shape it cannot pinpoint is a *required* field whose valid values are a
//! closed set — an enum such as `experimental.policies[].effect`. Neither removal
//! nor any probe value can be shown to repair the document, so the path stops at the
//! enclosing object and the deserializer's own message ("unknown variant `maybe`,
//! expected `allow` or `deny`") supplies the rest.

use crate::schema::{Config, KNOWN_TOP_LEVEL_KEYS};
use serde_json::Value;
use std::path::Path;
use zuno_error::{ConfigError, ConfigIssue};

/// How deep [`locate_failure`] will descend before giving up.
const MAX_PROBE_DEPTH: usize = 64;

impl Config {
    /// Parse one config layer from JSON text.
    ///
    /// This is strict JSON. Comments and trailing commas belong to the JSONC
    /// reader in the discovery pass, not here.
    ///
    /// Deserialization runs against the **text**, not against an intermediate
    /// [`Value`], and that is load-bearing: `serde_json::Map` is a `BTreeMap` in
    /// this workspace, so a document that has passed through [`Value`] has already
    /// had its keys sorted — which would destroy the author's permission order that
    /// `packages/core/src/v1/config/permission.ts:14-16` says precedence depends on.
    pub fn from_json_str(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let value = serde_json::from_str::<Value>(text).map_err(|source| ConfigError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        reject_unknown_top_level_keys(path, &value)?;
        serde_json::from_str::<Self>(text).map_err(|error| invalid(path, &value, &error))
    }

    /// Parse one config layer from an already-decoded JSON document.
    ///
    /// Convenient, but lossy in one respect: a [`Value`] has already sorted its
    /// object keys, so the author's key order is gone. Prefer
    /// [`from_json_str`](Self::from_json_str) whenever the text is still at hand.
    pub fn from_json_value(path: &Path, value: Value) -> Result<Self, ConfigError> {
        reject_unknown_top_level_keys(path, &value)?;
        serde_json::from_value::<Self>(value.clone()).map_err(|error| invalid(path, &value, &error))
    }
}

/// Report one issue per unknown top-level key so a fixer can act on each.
fn reject_unknown_top_level_keys(path: &Path, value: &Value) -> Result<(), ConfigError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let issues: Vec<ConfigIssue> = object
        .keys()
        .filter(|key| !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()))
        .map(|key| ConfigIssue::new([key.as_str()], "unrecognized key"))
        .collect();
    if issues.is_empty() {
        return Ok(());
    }
    Err(ConfigError::Invalid {
        path: path.to_path_buf(),
        issues,
    })
}

fn invalid(path: &Path, value: &Value, error: &serde_json::Error) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_path_buf(),
        issues: vec![ConfigIssue::new(locate_failure(value), error.to_string())],
    }
}

/// One hop of a key path: an object key, or an array index.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

impl Step {
    fn render(&self) -> String {
        match self {
            Self::Key(key) => key.clone(),
            Self::Index(index) => index.to_string(),
        }
    }
}

/// The key path of the value that makes `root` fail to deserialize.
///
/// Returns the deepest path it can prove. An empty path means the failure is the
/// document itself — it is not an object, or no single child accounts for it.
fn locate_failure(root: &Value) -> Vec<String> {
    let mut path: Vec<Step> = Vec::new();
    while path.len() < MAX_PROBE_DEPTH {
        let Some(node) = value_at(root, &path) else {
            break;
        };
        let Some(culprit) = children(node)
            .into_iter()
            .find(|step| is_culprit(root, &path, step))
        else {
            break;
        };
        path.push(culprit);
    }
    path.iter().map(Step::render).collect()
}

/// Values substituted for a child to test whether that child alone is at fault.
///
/// Removal alone cannot implicate a *required* field, because removing it leaves
/// the document failing for a new reason. Substituting a value of every JSON shape
/// covers that case: if any of these makes the whole document valid, the child was
/// the only problem. A false positive is impossible — the document has to pass.
const PROBE_VALUES: &[fn() -> Value] = &[
    || Value::from(0),
    || Value::from(""),
    || Value::from(false),
    || Value::Object(serde_json::Map::new()),
    || Value::Array(Vec::new()),
];

/// Whether `step`, under the node at `path`, is what makes `root` fail.
fn is_culprit(root: &Value, path: &[Step], step: &Step) -> bool {
    let mut probe = root.clone();
    if remove_at(&mut probe, path, step) && parses(&probe) {
        return true;
    }
    PROBE_VALUES.iter().any(|make| {
        let mut probe = root.clone();
        replace_at(&mut probe, path, step, make()) && parses(&probe)
    })
}

fn parses(value: &Value) -> bool {
    serde_json::from_value::<Config>(value.clone()).is_ok()
}

/// The children of `node` that are worth probing.
fn children(node: &Value) -> Vec<Step> {
    match node {
        Value::Object(object) => object.keys().cloned().map(Step::Key).collect(),
        Value::Array(items) => (0..items.len()).map(Step::Index).collect(),
        _ => Vec::new(),
    }
}

fn value_at<'a>(root: &'a Value, path: &[Step]) -> Option<&'a Value> {
    let mut node = root;
    for step in path {
        node = match (step, node) {
            (Step::Key(key), Value::Object(object)) => object.get(key)?,
            (Step::Index(index), Value::Array(items)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(node)
}

/// Remove `step` from the node at `path`. Returns whether anything was removed.
fn remove_at(root: &mut Value, path: &[Step], step: &Step) -> bool {
    match (step, node_at_mut(root, path)) {
        (Step::Key(key), Some(Value::Object(object))) => object.remove(key).is_some(),
        (Step::Index(index), Some(Value::Array(items))) if *index < items.len() => {
            items.remove(*index);
            true
        }
        _ => false,
    }
}

/// Overwrite `step` under the node at `path`. Returns whether anything was written.
fn replace_at(root: &mut Value, path: &[Step], step: &Step, value: Value) -> bool {
    match (step, node_at_mut(root, path)) {
        (Step::Key(key), Some(Value::Object(object))) => {
            object.insert(key.clone(), value).is_some()
        }
        (Step::Index(index), Some(Value::Array(items))) => match items.get_mut(*index) {
            Some(slot) => {
                *slot = value;
                true
            }
            None => false,
        },
        _ => false,
    }
}

fn node_at_mut<'a>(root: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    let mut node = root;
    for hop in path {
        node = match (hop, node) {
            (Step::Key(key), Value::Object(object)) => object.get_mut(key)?,
            (Step::Index(index), Value::Array(items)) => items.get_mut(*index)?,
            _ => return None,
        };
    }
    Some(node)
}
