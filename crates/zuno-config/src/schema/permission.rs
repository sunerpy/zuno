//! Canonical permission configuration.
//!
//! Zuno is unreleased, so the public shape has one representation only:
//! `permission.mode` selects cross-cutting HITL behavior and
//! `permission.rules` carries ordered per-tool rules.

use crate::schema::ordered::OrderedMap;
use schemars::JsonSchema;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// What to do when a tool asks (`config/permission.ts:5`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    /// Prompt the user.
    Ask,
    /// Run without prompting.
    Allow,
    /// Refuse.
    Deny,
}

/// Cross-cutting human-in-the-loop behavior for tool calls.
#[derive(JsonSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Apply configured rules and the normal risk gates.
    #[default]
    Standard,
    /// Require a fresh decision for every side-effecting call.
    Strict,
    /// Skip HITL prompts while preserving explicit denies and sandbox validation.
    AllowAll,
}

/// A permission rule: one action for the whole tool, or per-pattern actions
/// (`config/permission.ts:8-12`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionRule {
    /// One action covering every invocation of the tool.
    Action(PermissionAction),
    /// Per-pattern actions, evaluated in the author's key order.
    Patterns(OrderedMap<PermissionAction>),
}

/// The keys the oracle names explicitly (`config/permission.ts:18-34`). Any other
/// key is still valid — the oracle's rest record accepts it — and is kept.
pub const KNOWN_KEYS: &[&str] = &[
    "read",
    "edit",
    "glob",
    "grep",
    "list",
    "bash",
    "task",
    "external_directory",
    "plan_get",
    "plan_update",
    "todo_get",
    "todo_update",
    "question",
    "webfetch",
    "web_search",
    "lsp",
    "doom_loop",
    "skill",
];

/// The subset of [`KNOWN_KEYS`] the oracle types as a bare action, with no
/// per-pattern form (`config/permission.ts:27-30,32`).
pub const ACTION_ONLY_KEYS: &[&str] = &[
    "plan_get",
    "plan_update",
    "todo_get",
    "todo_update",
    "question",
    "webfetch",
    "web_search",
    "doom_loop",
];

/// The canonical permission configuration used by Zuno.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PermissionConfig {
    /// How unresolved and side-effecting calls are admitted.
    #[serde(default)]
    pub mode: PermissionMode,
    /// Ordered per-tool rules. Explicit denies remain terminal in every mode.
    #[serde(default)]
    pub rules: PermissionObject,
}

/// Per-tool permission rules, in the author's key order.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize)]
#[serde(transparent)]
pub struct PermissionObject(pub OrderedMap<PermissionRule>);

impl PermissionObject {
    /// The rule for `key`, if the author set one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&PermissionRule> {
        self.0.get(key)
    }

    /// Rules in the author's key order, which is the precedence order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PermissionRule)> {
        self.0.iter()
    }

    /// Whether no rules were set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn from_map_access<'de, A: MapAccess<'de>>(mut access: A) -> Result<Self, A::Error> {
        let mut rules = OrderedMap::new();
        while let Some((key, rule)) = access.next_entry::<String, PermissionRule>()? {
            validate_rule::<A::Error>(&key, &rule)?;
            rules.insert(key, rule);
        }
        Ok(Self(rules))
    }
}

impl<'de> Deserialize<'de> for PermissionObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PermissionObjectVisitor;

        impl<'de> Visitor<'de> for PermissionObjectVisitor {
            type Value = PermissionObject;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of permission rules")
            }

            fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
                PermissionObject::from_map_access(access)
            }
        }

        deserializer.deserialize_map(PermissionObjectVisitor)
    }
}

fn validate_rule<E: de::Error>(key: &str, rule: &PermissionRule) -> Result<(), E> {
    if ACTION_ONLY_KEYS.contains(&key) && matches!(rule, PermissionRule::Patterns(_)) {
        return Err(de::Error::custom(format!(
            "permission {key:?} takes a bare action, not per-pattern rules"
        )));
    }
    Ok(())
}
