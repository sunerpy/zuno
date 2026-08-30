use super::Subcall;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use zuno_tool::ToolOutput;

#[derive(Debug, thiserror::Error)]
pub(crate) enum BindingError {
    #[error("binding `{name}` is declared more than once")]
    Duplicate { name: String },
    #[error("binding `{name}` does not exist. Available bindings: {available}")]
    Missing { name: String, available: String },
    #[error("binding `{name}` must come from an earlier sub-call")]
    Forward { name: String },
    #[error("sub-call {call} contains more than one `$each` expression")]
    MultipleFanout { call: usize },
    #[error("invalid binding path `{path}`: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("binding `{name}` is unavailable because its sub-call failed")]
    Unavailable { name: String },
    #[error("binding path `{path}` did not resolve")]
    Unresolved { path: String },
    #[error("`$each` path `{path}` must end in `[*]` and select an array")]
    FanoutNotArray { path: String },
}

#[derive(Debug)]
pub(crate) struct BindingPlan {
    levels: Vec<Vec<usize>>,
}

impl BindingPlan {
    pub(crate) fn new(calls: &[Subcall]) -> Result<Self, BindingError> {
        let mut declarations = BTreeMap::new();
        for (index, call) in calls.iter().enumerate() {
            if let Some(name) = &call.bind
                && declarations.insert(name.clone(), index).is_some()
            {
                return Err(BindingError::Duplicate { name: name.clone() });
            }
        }
        let available = declarations.keys().cloned().collect::<Vec<_>>().join(", ");
        let mut call_levels = vec![0; calls.len()];
        let mut levels: Vec<Vec<usize>> = Vec::new();

        for (index, call) in calls.iter().enumerate() {
            let markers = markers(&Value::Object(call.arguments.clone()));
            if markers.iter().filter(|marker| marker.each).count() > 1 {
                return Err(BindingError::MultipleFanout { call: index + 1 });
            }
            let mut dependencies = BTreeSet::new();
            for marker in markers {
                let path = ParsedPath::parse(&marker.path, marker.each)?;
                let Some(source) = declarations.get(&path.binding).copied() else {
                    return Err(BindingError::Missing {
                        name: path.binding,
                        available: available.clone(),
                    });
                };
                if source >= index {
                    return Err(BindingError::Forward { name: path.binding });
                }
                dependencies.insert(source);
            }
            let level = dependencies
                .iter()
                .map(|source| call_levels[*source] + 1)
                .max()
                .unwrap_or(0);
            call_levels[index] = level;
            if levels.len() <= level {
                levels.resize_with(level + 1, Vec::new);
            }
            levels[level].push(index);
        }
        Ok(Self { levels })
    }

    pub(crate) fn levels(&self) -> &[Vec<usize>] {
        &self.levels
    }
}

pub(crate) fn expand(
    arguments: &Map<String, Value>,
    bindings: &BTreeMap<String, ToolOutput>,
) -> Result<Vec<Value>, BindingError> {
    match resolve(Value::Object(arguments.clone()), bindings)? {
        Resolved::One(value) => Ok(vec![value]),
        Resolved::Many(values) => Ok(values),
    }
}

enum Resolved {
    One(Value),
    Many(Vec<Value>),
}

fn resolve(
    value: Value,
    bindings: &BTreeMap<String, ToolOutput>,
) -> Result<Resolved, BindingError> {
    match value {
        Value::Object(mut object) if object.len() == 1 => {
            if let Some(Value::String(path)) = object.remove("$ref") {
                return Ok(Resolved::One(select(&path, false, bindings)?));
            }
            if let Some(Value::String(path)) = object.remove("$each") {
                let selected = select(&path, true, bindings)?;
                return selected
                    .as_array()
                    .cloned()
                    .map(Resolved::Many)
                    .ok_or(BindingError::FanoutNotArray { path });
            }
            resolve_object(object, bindings)
        }
        Value::Object(object) => resolve_object(object, bindings),
        Value::Array(values) => resolve_array(values, bindings),
        scalar => Ok(Resolved::One(scalar)),
    }
}

fn resolve_object(
    object: Map<String, Value>,
    bindings: &BTreeMap<String, ToolOutput>,
) -> Result<Resolved, BindingError> {
    let mut variants = vec![Map::new()];
    for (key, value) in object {
        let resolved = resolve(value, bindings)?;
        variants = combine_object(variants, key, resolved);
    }
    if variants.len() == 1 {
        Ok(Resolved::One(Value::Object(variants.remove(0))))
    } else {
        Ok(Resolved::Many(
            variants.into_iter().map(Value::Object).collect(),
        ))
    }
}

fn combine_object(
    bases: Vec<Map<String, Value>>,
    key: String,
    resolved: Resolved,
) -> Vec<Map<String, Value>> {
    match resolved {
        Resolved::One(value) => bases
            .into_iter()
            .map(|mut base| {
                base.insert(key.clone(), value.clone());
                base
            })
            .collect(),
        Resolved::Many(values) => bases
            .into_iter()
            .flat_map(|base| {
                let key = key.clone();
                values.iter().cloned().map(move |value| {
                    let mut variant = base.clone();
                    variant.insert(key.clone(), value);
                    variant
                })
            })
            .collect(),
    }
}

fn resolve_array(
    values: Vec<Value>,
    bindings: &BTreeMap<String, ToolOutput>,
) -> Result<Resolved, BindingError> {
    let mut variants = vec![Vec::new()];
    for value in values {
        let resolved = resolve(value, bindings)?;
        variants = match resolved {
            Resolved::One(value) => variants
                .into_iter()
                .map(|mut base| {
                    base.push(value.clone());
                    base
                })
                .collect(),
            Resolved::Many(items) => variants
                .into_iter()
                .flat_map(|base| {
                    items.iter().cloned().map(move |item| {
                        let mut variant = base.clone();
                        variant.push(item);
                        variant
                    })
                })
                .collect(),
        };
    }
    if variants.len() == 1 {
        Ok(Resolved::One(Value::Array(variants.remove(0))))
    } else {
        Ok(Resolved::Many(
            variants.into_iter().map(Value::Array).collect(),
        ))
    }
}

#[derive(Debug)]
struct Marker {
    path: String,
    each: bool,
}

fn markers(value: &Value) -> Vec<Marker> {
    let mut found = Vec::new();
    collect_markers(value, &mut found);
    found
}

fn collect_markers(value: &Value, found: &mut Vec<Marker>) {
    match value {
        Value::Object(object) if object.len() == 1 => {
            for (key, each) in [("$ref", false), ("$each", true)] {
                if let Some(path) = object.get(key).and_then(Value::as_str) {
                    found.push(Marker {
                        path: path.to_owned(),
                        each,
                    });
                    return;
                }
            }
            for value in object.values() {
                collect_markers(value, found);
            }
        }
        Value::Object(object) => object
            .values()
            .for_each(|value| collect_markers(value, found)),
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_markers(value, found)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[derive(Debug)]
struct ParsedPath {
    binding: String,
    tokens: Vec<PathToken>,
}

#[derive(Debug)]
enum PathToken {
    Field(String),
    Index(usize),
    Wildcard,
}

impl ParsedPath {
    fn parse(path: &str, each: bool) -> Result<Self, BindingError> {
        let (binding, remainder) =
            path.split_once('.')
                .ok_or_else(|| BindingError::InvalidPath {
                    path: path.to_owned(),
                    reason: "expected `<binding>.<field>`",
                })?;
        if binding.is_empty() {
            return Err(BindingError::InvalidPath {
                path: path.to_owned(),
                reason: "binding name is empty",
            });
        }
        let tokens = parse_tokens(path, remainder)?;
        let wildcard_is_final = matches!(tokens.last(), Some(PathToken::Wildcard));
        if each != wildcard_is_final {
            return Err(BindingError::InvalidPath {
                path: path.to_owned(),
                reason: if each {
                    "`$each` paths must end in `[*]`"
                } else {
                    "`$ref` paths cannot contain `[*]`"
                },
            });
        }
        Ok(Self {
            binding: binding.to_owned(),
            tokens,
        })
    }
}

fn parse_tokens(path: &str, input: &str) -> Result<Vec<PathToken>, BindingError> {
    let normalized = input.replace('[', ".[");
    normalized
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part == "[*]" {
                Ok(PathToken::Wildcard)
            } else if let Some(index) = part.strip_prefix('[').and_then(|p| p.strip_suffix(']')) {
                index.parse::<usize>().map(PathToken::Index).map_err(|_| {
                    BindingError::InvalidPath {
                        path: path.to_owned(),
                        reason: "array index must be a non-negative integer",
                    }
                })
            } else {
                Ok(PathToken::Field(part.to_owned()))
            }
        })
        .collect()
}

fn select(
    path: &str,
    each: bool,
    bindings: &BTreeMap<String, ToolOutput>,
) -> Result<Value, BindingError> {
    let parsed = ParsedPath::parse(path, each)?;
    let output = bindings
        .get(&parsed.binding)
        .ok_or_else(|| BindingError::Unavailable {
            name: parsed.binding.clone(),
        })?;
    let mut value = serde_json::to_value(output).map_err(|_| BindingError::Unresolved {
        path: path.to_owned(),
    })?;
    for token in parsed.tokens {
        value = match token {
            PathToken::Field(field) => value.get(&field).cloned(),
            PathToken::Index(index) => value.get(index).cloned(),
            PathToken::Wildcard => Some(value),
        }
        .ok_or_else(|| BindingError::Unresolved {
            path: path.to_owned(),
        })?;
    }
    Ok(value)
}
