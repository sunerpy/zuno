//! Building blocks the rest of the schema needs but `serde` does not ship.
//!
//! [`OrderedMap`] exists because `Record(String, ...)` in the TypeScript config
//! is decoded with Effect's `propertyOrder: "original"` option, and at least one
//! consumer — permission precedence — depends on that order
//! (`packages/core/src/v1/config/permission.ts:14-16`). `BTreeMap` would sort the
//! keys and `serde_json::Map` sorts them too unless its `preserve_order` feature
//! is on, which it is not in this workspace.
//!
//! [`OrderedJson`] is the same guarantee for a whole document rather than one
//! map: a parser that builds config-shaped JSON for a typed `Deserialize` builds
//! this instead of a `serde_json::Value`, whose objects would sort.
//!
//! [`False`] exists because the oracle has two `Schema.Literal(false)` arms
//! (provider timeouts and MCP OAuth) where `bool` would wrongly accept `true`.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::marker::PhantomData;

/// A string-keyed map that keeps the key order it was parsed in.
///
/// Duplicate keys resolve last-wins in place, which is what `serde_json` and
/// `JSON.parse` both do; the winning value keeps the losing key's position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedMap<V> {
    entries: Vec<(String, V)>,
}

impl<V: JsonSchema> JsonSchema for OrderedMap<V> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("OrderedMap_of_{}", V::schema_name()).into()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        format!("{}::OrderedMap<{}>", module_path!(), V::schema_id()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <std::collections::BTreeMap<String, V>>::json_schema(generator)
    }
}

impl<V> OrderedMap<V> {
    /// An empty map.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// The value stored under `key`, if any.
    pub fn get(&self, key: &str) -> Option<&V> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Whether `key` is present.
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Insert `key`, preserving an existing key's position when it is replaced.
    /// Returns the value that was displaced.
    pub fn insert(&mut self, key: impl Into<String>, value: V) -> Option<V> {
        let key = key.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            return Some(std::mem::replace(&mut slot.1, value));
        }
        self.entries.push((key, value));
        None
    }

    /// Entries in parse order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Keys in parse order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> FromIterator<(String, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

impl<V> IntoIterator for OrderedMap<V> {
    type Item = (String, V);
    type IntoIter = std::vec::IntoIter<(String, V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a, V> IntoIterator for &'a OrderedMap<V> {
    type Item = (&'a str, &'a V);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a V)> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedMapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for OrderedMapVisitor<V> {
            type Value = OrderedMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut map = OrderedMap::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    map.insert(key, value);
                }
                Ok(map)
            }
        }

        deserializer.deserialize_map(OrderedMapVisitor(PhantomData))
    }
}

/// A JSON document whose objects keep the key order they were written in.
///
/// `serde_json::Map` is a `BTreeMap` in this workspace, so anything that hops
/// through [`serde_json::Value`] comes out alphabetized. `permission.rules` is
/// evaluated last-match-wins over the author's key order
/// ([`PermissionObject`](crate::schema::permission::PermissionObject)), which
/// makes an alphabetized rule set a *different* rule set: sorting
/// `{"*": "allow", "$HOME/.ssh/*": "deny"}` moves the deny before the catch-all
/// and deletes the protection. A parser that produces config-shaped JSON for a
/// typed `Deserialize` builds this instead of a `Value`, and hands it over as
/// JSON **text** (`serde_json::to_string`), which every `Deserialize` impl in the
/// schema reads in order.
///
/// Objects are [`OrderedMap`]s, so one object cannot hold one key twice: a repeat
/// resolves last-wins in the first key's position, as `serde_json` and
/// `JSON.parse` do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedJson {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any JSON number.
    Number(serde_json::Number),
    /// A string.
    String(String),
    /// An array, in order.
    Array(Vec<Self>),
    /// An object, in the author's key order.
    Object(OrderedMap<Self>),
}

impl OrderedJson {
    /// The value under `key`, or `None` for a missing key or a non-object.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries.get(key),
            _ => None,
        }
    }

    /// This value as a string, or `None` when it is any other shape.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

impl Serialize for OrderedJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Object(entries) => entries.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedJsonVisitor;

        impl<'de> Visitor<'de> for OrderedJsonVisitor {
            type Value = OrderedJson;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedJson::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedJson::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedJson::Number(value.into()))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(OrderedJson::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OrderedJson::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(OrderedJson::String(value))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJson::Null)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedJson::Null)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = access.next_element()? {
                    values.push(value);
                }
                Ok(OrderedJson::Array(values))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut entries = OrderedMap::new();
                while let Some((key, value)) = access.next_entry::<String, OrderedJson>()? {
                    entries.insert(key, value);
                }
                Ok(OrderedJson::Object(entries))
            }
        }

        deserializer.deserialize_any(OrderedJsonVisitor)
    }
}

/// The literal `false`, for union arms where `true` must be rejected.
///
/// Mirrors `Schema.Literal(false)` in `config/provider.ts:102,109` and
/// `config/mcp.ts:53`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct False;

impl JsonSchema for False {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "false".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        serde_json::json!({ "const": false })
            .try_into()
            .expect("the literal false is a valid JSON Schema")
    }
}

impl Serialize for False {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(false)
    }
}

impl<'de> Deserialize<'de> for False {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match bool::deserialize(deserializer)? {
            false => Ok(Self),
            true => Err(de::Error::invalid_value(
                de::Unexpected::Bool(true),
                &"the literal false",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `$HOME/.ssh/*` sorts before `*`, so a carrier that sorted would reverse the
    /// precedence of exactly the rule pair the permissions guide tells users to write.
    const GUIDE_SHAPE: &str =
        r#"{"permission":{"rules":{"read":{"*":"allow","$HOME/.ssh/*":"deny"}}}}"#;

    fn keys(value: &OrderedJson) -> Vec<&str> {
        match value {
            OrderedJson::Object(entries) => entries.keys().collect(),
            other => panic!("expected an object, got {other:?}"),
        }
    }

    #[test]
    fn a_document_round_trips_through_text_in_the_authors_key_order() {
        let parsed: OrderedJson = serde_json::from_str(GUIDE_SHAPE).expect("parses");
        let read = parsed
            .get("permission")
            .and_then(|permission| permission.get("rules"))
            .and_then(|rules| rules.get("read"))
            .expect("nested object");
        assert_eq!(keys(read), ["*", "$HOME/.ssh/*"]);
        assert_eq!(
            serde_json::to_string(&parsed).expect("serializes"),
            GUIDE_SHAPE,
            "the text a typed Deserialize reads is the text the author wrote"
        );
        let sorted: serde_json::Value = serde_json::from_str(GUIDE_SHAPE).expect("parses");
        assert_ne!(
            serde_json::to_string(&sorted).expect("serializes"),
            GUIDE_SHAPE,
            "the control: serde_json::Value really does reorder this document"
        );
    }

    #[test]
    fn a_repeated_key_resolves_last_wins_in_the_first_keys_position() {
        let parsed: OrderedJson =
            serde_json::from_str(r#"{"a":1,"b":2,"a":3}"#).expect("serde_json accepts repeats");
        assert_eq!(keys(&parsed), ["a", "b"]);
        assert_eq!(
            parsed.get("a"),
            Some(&OrderedJson::Number(3.into())),
            "the later value wins, as serde_json::Map and JSON.parse resolve it"
        );
    }

    #[test]
    fn every_scalar_shape_survives_a_round_trip() {
        let text = r#"[null,true,false,1,-2,3.5,"s",[],{}]"#;
        let parsed: OrderedJson = serde_json::from_str(text).expect("parses");
        assert_eq!(serde_json::to_string(&parsed).expect("serializes"), text);
        assert_eq!(parsed.get("anything"), None, "an array has no keys");
        assert_eq!(OrderedJson::String("s".to_owned()).as_str(), Some("s"));
        assert_eq!(OrderedJson::Bool(true).as_str(), None);
    }
}
