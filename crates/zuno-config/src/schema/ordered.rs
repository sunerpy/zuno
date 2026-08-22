//! Building blocks the rest of the schema needs but `serde` does not ship.
//!
//! [`OrderedMap`] exists because `Record(String, ...)` in the TypeScript config
//! is decoded with Effect's `propertyOrder: "original"` option, and at least one
//! consumer — permission precedence — depends on that order
//! (`packages/core/src/v1/config/permission.ts:14-16`). `BTreeMap` would sort the
//! keys and `serde_json::Map` sorts them too unless its `preserve_order` feature
//! is on, which it is not in this workspace.
//!
//! [`False`] exists because the oracle has two `Schema.Literal(false)` arms
//! (provider timeouts and MCP OAuth) where `bool` would wrongly accept `true`.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
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
