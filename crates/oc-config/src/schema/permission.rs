//! Permission configuration.
//!
//! Oracle: `packages/core/src/v1/config/permission.ts:5-50`.
//!
//! Two details of that file drive the shape here. First, the comment at `:14-16`:
//! runtime parsing uses `propertyOrder: "original"` **because permission
//! precedence depends on the author's key order**, so the object cannot be a
//! sorted map. Second, `Info` decodes a bare action string into `{ "*": action }`
//! (`:40-48`); that normalization is offered as [`PermissionConfig::normalized`]
//! rather than applied during deserialization, so the parsed value still says
//! which form the author wrote.

use crate::schema::ordered::OrderedMap;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// What to do when a tool asks (`config/permission.ts:5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    /// Prompt the user.
    Ask,
    /// Run without prompting.
    Allow,
    /// Refuse.
    Deny,
}

/// A permission rule: one action for the whole tool, or per-pattern actions
/// (`config/permission.ts:8-12`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    "todowrite",
    "question",
    "webfetch",
    "websearch",
    "lsp",
    "doom_loop",
    "skill",
];

/// The subset of [`KNOWN_KEYS`] the oracle types as a bare action, with no
/// per-pattern form (`config/permission.ts:27-30,32`).
pub const ACTION_ONLY_KEYS: &[&str] = &[
    "todowrite",
    "question",
    "webfetch",
    "websearch",
    "doom_loop",
];

/// The `permission` key: one action for everything, or per-tool rules.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PermissionConfig {
    /// A bare action applied to every tool.
    Action(PermissionAction),
    /// Per-tool rules, in the author's key order.
    Object(PermissionObject),
}

impl PermissionConfig {
    /// The object form, expanding a bare action to `{ "*": action }` exactly as
    /// `normalizeInput` does at `config/permission.ts:40-41`.
    #[must_use]
    pub fn normalized(&self) -> PermissionObject {
        match self {
            Self::Action(action) => {
                let mut rules = OrderedMap::new();
                rules.insert("*", PermissionRule::Action(*action));
                PermissionObject(rules)
            }
            Self::Object(object) => object.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for PermissionConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PermissionConfigVisitor;

        impl<'de> Visitor<'de> for PermissionConfigVisitor {
            type Value = PermissionConfig;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"one of "ask", "allow", "deny", or an object of permission rules"#)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value {
                    "ask" => Ok(PermissionConfig::Action(PermissionAction::Ask)),
                    "allow" => Ok(PermissionConfig::Action(PermissionAction::Allow)),
                    "deny" => Ok(PermissionConfig::Action(PermissionAction::Deny)),
                    other => Err(de::Error::unknown_variant(other, &["ask", "allow", "deny"])),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
                PermissionObject::from_map_access(access).map(PermissionConfig::Object)
            }
        }

        deserializer.deserialize_any(PermissionConfigVisitor)
    }
}

/// Per-tool permission rules, in the author's key order.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
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
            if ACTION_ONLY_KEYS.contains(&key.as_str())
                && matches!(rule, PermissionRule::Patterns(_))
            {
                return Err(de::Error::custom(format!(
                    "permission {key:?} takes a bare action, not per-pattern rules"
                )));
            }
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
