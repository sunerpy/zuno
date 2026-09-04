//! Parameter schemas: one artifact derived from the params struct, then augmented centrally.
//!
//! # Why derivation, not authorship
//!
//! The reference Rust agent in `claw-code`
//! hand-writes its tool schemas with `serde_json::json!` — 250 `json!` literals in
//! one file, 61 of them `"type": "object"` — and deserializes the same arguments
//! into *separate* serde structs declared elsewhere in the file (see
//! `PowerShellInput` at `:2922`, whose schema at `:487` is 400 lines away). Nothing
//! ties the two together, so a renamed field, a changed `required`, or a camelCase
//! key drifts silently: the model is told about a parameter the deserializer will
//! never read.
//!
//! Here there is exactly one artifact. [`params_schema`] derives the wire schema
//! from the same Rust type [`crate::TypedTool::Params`] that
//! [`serde`] deserializes, via [`schemars`]. A renamed field cannot desynchronize
//! the two, because there is no second copy to desynchronize from.
//!
//! # Why augmentation is central
//!
//! Two properties are cross-cutting: every tool may report *why* it is being
//! called, and every tool can produce a result too large to hand back. Wiring
//! those into each tool by hand would mean editing every schema, and would miss
//! MCP proxied tools entirely — their schema arrives from a remote server as plain
//! JSON that no local struct describes. So injection happens once, at the single
//! point where a tool becomes a provider-facing definition
//! ([`crate::Tool::definition`]), and applies to derived and proxied schemas
//! alike. Pattern taken from
//! `jcode`.
//!
//! # The cost discipline
//!
//! Every byte here rides on every tool schema on every request for the life of a
//! session, so each word is paid forever. The injected descriptions are
//! deliberately terse; the long explanation belongs in the refusal message, which
//! is only ever rendered when it is actually relevant. `schema_augmentation.rs`
//! pins the byte cost so a future edit cannot quietly inflate it.

use schemars::JsonSchema;
use serde_json::{Map, Value};

/// The property name carrying the model's short reason for a call.
///
/// Read back by [`crate::guard::intent`]. `tests/guard_key.rs` proves the two
/// cannot diverge.
pub const INTENT_KEY: &str = "intent";

/// The property name a caller sets to accept the token cost of an oversized result.
///
/// Read back by [`crate::guard::accepts_large_output`]. A divergence between this
/// constant and the guard would advertise a flag that is never honoured, which is
/// worse than not offering one, so `tests/guard_key.rs` proves they agree.
pub const ACCEPT_LARGE_OUTPUT_KEY: &str = "accept_large_output";

/// The `intent` description. Terse on purpose: see the module docs.
pub const INTENT_DESCRIPTION: &str =
    "Optional short label shown in the UI: why this call is being made.";

/// The `accept_large_output` description. Terse on purpose: see the module docs.
pub const ACCEPT_LARGE_OUTPUT_DESCRIPTION: &str =
    "Re-run accepting the stated token cost of a withheld result.";

/// Every property name the augmentation injects, in injection order.
///
/// Exists so a caller can enumerate the cross-cutting keys without restating the
/// literals — notably [`crate::guard::strip_cross_cutting`], which removes them
/// before a typed params struct sees the arguments.
pub const INJECTED_KEYS: [&str; 2] = [INTENT_KEY, ACCEPT_LARGE_OUTPUT_KEY];

/// The `intent` subschema.
#[must_use]
pub fn intent_property() -> Value {
    serde_json::json!({ "type": "string", "description": INTENT_DESCRIPTION })
}

/// The `accept_large_output` subschema.
#[must_use]
pub fn accept_large_output_property() -> Value {
    serde_json::json!({ "type": "boolean", "description": ACCEPT_LARGE_OUTPUT_DESCRIPTION })
}

/// Derives a parameter schema from the params type, with no hand-written JSON.
///
/// Three deliberate departures from schemars' defaults, each paid for on every
/// request:
///
/// - **draft-07.** What provider tool-calling APIs consume. schemars defaults to
///   2020-12, whose `$defs`/`$dynamicRef` vocabulary providers do not implement.
/// - **Subschemas inlined.** A `$ref` into `$defs` is a hop providers handle
///   inconsistently; inlining trades a few bytes for a schema every provider reads
///   the same way.
/// - **`$schema` and `title` stripped.** The `$schema` URL is 46 bytes of pure
///   token cost that no provider reads, and `title` is the Rust type name — an
///   internal detail that says nothing the tool's own id does not.
///
/// A root tagged union — schemars' rendering of `#[serde(tag = "...")]` — is
/// folded into one object schema by [`fold_tagged_union`], so an operation-based
/// tool such as `plan_update` reaches the provider with every operation visible.
///
/// Any other params type whose schema is not object-shaped is normalized to an
/// empty object schema. `#[derive(JsonSchema)]` on a unit struct yields `{"type":
/// "null"}`, but a no-argument tool call arrives on the wire as `{}`, and an
/// un-augmented non-object schema would silently lose the injected `intent`.
#[must_use]
pub fn derive_params_schema<T: JsonSchema>() -> Value {
    let mut settings = schemars::generate::SchemaSettings::draft07();
    settings.inline_subschemas = true;
    let mut schema = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();

    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }

    if is_object_schema(&schema) {
        schema
    } else if let Some(folded) = fold_tagged_union(&schema) {
        folded
    } else {
        serde_json::json!({ "type": "object", "properties": {} })
    }
}

/// Folds a root tagged union into one object schema.
///
/// schemars renders an internally tagged enum — `plan_update`'s `PlanMutationParams`,
/// `notes`' `NotesParams`, `history`'s `HistoryParams` — as a root `oneOf` of object
/// branches, each carrying the tag as a `const` string property, with neither `type`
/// nor `properties` at the root. That is not object-shaped, so until this fold
/// [`derive_params_schema`] normalized all three to `{"type":"object","properties":{}}`:
/// the model saw only the injected `intent` and `accept_large_output`, called
/// `plan_update` with `{"intent": …}`, the schema validator accepted it, and only the
/// typed parse blocked the call with `missing field \`action\``.
///
/// The fold emits what most tool schemas already look like: `type: object`, the root
/// `description`, the union of every branch's properties, the tag as
/// `{"type":"string","enum":[…]}` whose description names each operation and the
/// fields it requires, and `required: [tag]`. It emits no `additionalProperties:
/// false`: the union admits keys no single branch does, and the typed deserializer's
/// `deny_unknown_fields` still rejects strays per call. Only the tag is
/// schema-required; each branch's other required fields are enforced by the typed
/// parse, as before.
///
/// A property that several branches declare is merged deterministically, in
/// declaration order, by [`merge_uses`]:
///
/// - subschemas that differ only in `description` collapse to one, and when the
///   descriptions differ each is attributed to the operations that carry it
///   (`list_files_by_prefix: Page size, 1 through 50. search_contents: Page size, 1
///   through 20.`);
/// - when one is the other widened to admit `null` (`Option<T>` against `T`), the
///   nullable form wins, because it accepts everything the strict form does;
/// - otherwise the distinct shapes become an `anyOf`, so a `patch` whose `steps`
///   carry ids still validates alongside a `create` whose `steps` carry titles.
///
/// Returns `None` for a union that is not a tagged enum — branches that are not all
/// objects, or that do not share exactly one `const` string property with distinct
/// values — so the caller keeps its empty-object normalization for those.
fn fold_tagged_union(schema: &Value) -> Option<Value> {
    let root = schema.as_object()?;
    let branches = root
        .get("oneOf")
        .or_else(|| root.get("anyOf"))?
        .as_array()?;
    if branches.is_empty() || !branches.iter().all(is_object_schema) {
        return None;
    }
    let tag = shared_tag(branches)?;

    let mut values: Vec<String> = Vec::with_capacity(branches.len());
    let mut summaries: Vec<String> = Vec::with_capacity(branches.len());
    let mut uses: std::collections::BTreeMap<String, Vec<PropertyUse>> =
        std::collections::BTreeMap::new();
    for branch in branches {
        let branch_properties = branch.get("properties")?.as_object()?;
        let value = const_string(branch_properties.get(&tag)?)?;
        if values.contains(&value) {
            return None;
        }
        let required: Vec<&str> = branch
            .get("required")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|name| *name != tag)
                    .collect()
            })
            .unwrap_or_default();
        summaries.push(operation_summary(
            &value,
            branch.get("description").and_then(Value::as_str),
            &required,
        ));
        for (name, subschema) in branch_properties {
            if *name == tag {
                continue;
            }
            uses.entry(name.clone()).or_default().push(PropertyUse {
                operation: value.clone(),
                subschema: subschema.clone(),
            });
        }
        values.push(value);
    }
    let mut properties: Map<String, Value> = uses
        .into_iter()
        .map(|(name, uses)| (name, merge_uses(&uses)))
        .collect();

    let mut tag_property = Map::new();
    tag_property.insert("type".to_owned(), Value::String("string".to_owned()));
    tag_property.insert(
        "enum".to_owned(),
        Value::Array(values.into_iter().map(Value::String).collect()),
    );
    tag_property.insert(
        "description".to_owned(),
        Value::String(format!("Selects the operation. {}", summaries.join(" "))),
    );
    properties.insert(tag.clone(), Value::Object(tag_property));

    let mut folded = Map::new();
    folded.insert("type".to_owned(), Value::String("object".to_owned()));
    if let Some(description) = root.get("description") {
        folded.insert("description".to_owned(), description.clone());
    }
    // A recursive variant field keeps its `$ref` target: schemars still emits a root
    // `definitions` (draft-07) or `$defs` map for recursion even with inlining on, and
    // a fold that dropped it would leave every `$ref` inside the properties dangling.
    for key in ["definitions", "$defs"] {
        if let Some(definitions) = root.get(key) {
            folded.insert(key.to_owned(), definitions.clone());
        }
    }
    folded.insert("properties".to_owned(), Value::Object(properties));
    folded.insert("required".to_owned(), serde_json::json!([tag]));
    Some(Value::Object(folded))
}

/// The one property every branch declares as a `const` string: the serde tag.
///
/// Zero candidates means the union is not internally tagged; more than one means the
/// branches share a literal field this fold cannot tell apart from the tag. Both return
/// `None` so the caller falls back rather than guessing.
fn shared_tag(branches: &[Value]) -> Option<String> {
    let first = branches.first()?.get("properties")?.as_object()?;
    let mut candidates = first
        .iter()
        .filter(|(_, subschema)| const_string(subschema).is_some())
        .map(|(name, _)| name)
        .filter(|name| {
            branches.iter().all(|branch| {
                branch
                    .get("properties")
                    .and_then(|properties| properties.get(name.as_str()))
                    .and_then(const_string)
                    .is_some()
            })
        });
    let tag = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(tag.clone())
}

/// The literal a subschema pins a string to, whether as `const` or a one-value `enum`.
fn const_string(schema: &Value) -> Option<String> {
    let object = schema.as_object()?;
    if let Some(Value::String(value)) = object.get("const") {
        return Some(value.clone());
    }
    match object.get("enum")?.as_array()?.as_slice() {
        [Value::String(value)] => Some(value.clone()),
        _ => None,
    }
}

/// One sentence per operation for the tag's description.
///
/// The variant's own doc comment, when it has one, comes first so the model reads what
/// the operation does before what it needs; the required list is the branch's `required`
/// minus the tag, in declaration order.
fn operation_summary(value: &str, description: Option<&str>, required: &[&str]) -> String {
    let fields = if required.is_empty() {
        "no other property".to_owned()
    } else {
        required.join(", ")
    };
    let description = description
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|text| text.trim_end_matches('.').to_owned())
        .filter(|text| !text.is_empty());
    match description {
        Some(description) => format!("{value}: {description}; requires {fields}."),
        None => format!("{value} requires {fields}."),
    }
}

/// One branch's declaration of a property: which operation, and what it accepts there.
struct PropertyUse {
    operation: String,
    subschema: Value,
}

/// One shape a property takes across the operations that declare it.
struct Shape {
    /// The subschema with `description` removed and `null` stripped: what makes two
    /// declarations the same shape.
    key: Value,
    /// The subschema to emit for this shape, without its description.
    schema: Value,
    /// Whether `schema` already admits `null`.
    nullable: bool,
    /// Each distinct description and, in declaration order, the operations that carry it.
    descriptions: Vec<(Vec<String>, String)>,
}

impl Shape {
    fn render(self) -> Value {
        let Self {
            mut schema,
            descriptions,
            ..
        } = self;
        let description = match descriptions.as_slice() {
            [] => None,
            [(_, only)] => Some(only.clone()),
            many => Some(
                many.iter()
                    .map(|(operations, text)| format!("{}: {text}", operations.join(", ")))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        };
        if let (Some(description), Some(object)) = (description, schema.as_object_mut()) {
            object.insert("description".to_owned(), Value::String(description));
        }
        schema
    }
}

/// Merges every branch's declaration of one property into the subschema the wire carries.
///
/// See [`fold_tagged_union`] for the policy. Declarations are grouped by shape in
/// declaration order; a group is emitted in its nullable form as soon as any member is
/// nullable; one group is emitted as-is and several as an `anyOf`.
fn merge_uses(uses: &[PropertyUse]) -> Value {
    let mut shapes: Vec<Shape> = Vec::new();
    for PropertyUse {
        operation,
        subschema,
    } in uses
    {
        let mut schema = subschema.clone();
        let description = schema
            .as_object_mut()
            .and_then(|object| object.remove("description"))
            .and_then(|description| description.as_str().map(str::to_owned));
        let (key, nullable) = without_null(&schema);
        let shape = match shapes.iter().position(|shape| shape.key == key) {
            Some(index) => {
                let shape = &mut shapes[index];
                if nullable && !shape.nullable {
                    shape.schema = schema;
                    shape.nullable = true;
                }
                shape
            }
            None => {
                shapes.push(Shape {
                    key,
                    schema,
                    nullable,
                    descriptions: Vec::new(),
                });
                shapes.last_mut().expect("a shape was just pushed")
            }
        };
        if let Some(description) = description {
            match shape
                .descriptions
                .iter_mut()
                .find(|(_, text)| *text == description)
            {
                Some((operations, _)) => operations.push(operation.clone()),
                None => shape
                    .descriptions
                    .push((vec![operation.clone()], description)),
            }
        }
    }
    let mut rendered: Vec<Value> = shapes.into_iter().map(Shape::render).collect();
    if rendered.len() == 1 {
        rendered.remove(0)
    } else {
        serde_json::json!({ "anyOf": rendered })
    }
}

/// The schema with `null` removed from `type` and `enum` and a `null` default dropped,
/// plus whether anything was removed: the shape `Option<T>` shares with `T`.
///
/// schemars renders the option as `"type": [T, "null"]` plus `"default": null`, and a
/// nullable string enum also lists `null` among its values. Stripping those is what lets
/// a field that is optional in one operation and required in another be one property.
fn without_null(schema: &Value) -> (Value, bool) {
    let Some(object) = schema.as_object() else {
        return (schema.clone(), false);
    };
    let mut stripped = object.clone();
    let mut changed = false;
    if let Some(Value::Array(names)) = stripped.get("type").cloned() {
        let remaining: Vec<Value> = names
            .iter()
            .filter(|name| name.as_str() != Some("null"))
            .cloned()
            .collect();
        if remaining.len() != names.len() {
            changed = true;
            stripped.insert(
                "type".to_owned(),
                match remaining.as_slice() {
                    [single] => single.clone(),
                    _ => Value::Array(remaining),
                },
            );
        }
    }
    if let Some(Value::Array(values)) = stripped.get("enum").cloned() {
        let remaining: Vec<Value> = values
            .iter()
            .filter(|value| !value.is_null())
            .cloned()
            .collect();
        if remaining.len() != values.len() {
            changed = true;
            stripped.insert("enum".to_owned(), Value::Array(remaining));
        }
    }
    if changed && stripped.get("default") == Some(&Value::Null) {
        stripped.remove("default");
    }
    (Value::Object(stripped), changed)
}

/// Derives and augments in one step: the schema a provider should be sent.
///
/// Prefer this over calling [`derive_params_schema`] directly; the raw form exists
/// so [`crate::Tool::raw_parameters_schema`] can stay honest about being
/// un-augmented.
#[must_use]
pub fn params_schema<T: JsonSchema>() -> Value {
    augment(derive_params_schema::<T>())
}

/// Injects the cross-cutting properties into an object schema.
///
/// Idempotent, and non-destructive: a schema that already declares `intent` or
/// `accept_large_output` keeps its own definition, so a tool with a stricter
/// `intent` (an enum of labels, say) is not overwritten. Both injected fields are
/// optional metadata; the tool params type remains the sole source of required
/// arguments.
///
/// Non-object schemas pass through untouched. That includes MCP proxied schemas
/// that are shaped unusually — the alternative, rewriting a remote server's
/// declared parameters, is worse than leaving one tool without an intent.
#[must_use]
pub fn augment(mut schema: Value) -> Value {
    if !is_object_schema(&schema) {
        return schema;
    }
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };

    let properties = object
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(properties) = properties.as_object_mut() else {
        return schema;
    };
    properties.entry(INTENT_KEY).or_insert_with(intent_property);
    properties
        .entry(ACCEPT_LARGE_OUTPUT_KEY)
        .or_insert_with(accept_large_output_property);

    schema
}

/// Whether a schema describes a JSON object, and so can carry properties.
///
/// An explicit `"type"` decides it. With no `"type"` — legal in every draft, and
/// common in schemas emitted by MCP servers — the presence of `properties` is
/// taken as the declaration.
#[must_use]
pub fn is_object_schema(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    match object.get("type") {
        Some(Value::String(name)) => name == "object",
        Some(Value::Array(names)) => names.iter().any(|n| n.as_str() == Some("object")),
        Some(_) | None => object.contains_key("properties"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(JsonSchema)]
    #[allow(
        dead_code,
        reason = "the fields are consumed only by the JsonSchema derive in this schema fixture"
    )]
    struct Params {
        command: String,
        timeout: Option<u32>,
    }

    #[derive(JsonSchema)]
    struct NoParams;

    #[test]
    fn schema_derives_required_and_optional_from_the_params_struct_alone() {
        let schema = derive_params_schema::<Params>();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["command"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["command"]));
    }

    #[test]
    fn schema_strips_the_bytes_no_provider_reads() {
        let schema = derive_params_schema::<Params>();

        assert!(schema.get("$schema").is_none(), "$schema is dead weight");
        assert!(
            schema.get("title").is_none(),
            "title is the Rust type name, not the tool's name"
        );
    }

    #[test]
    fn schema_normalizes_a_no_argument_params_type_to_an_object() {
        // schemars renders a unit struct as {"type":"null"}; the wire sends `{}`.
        let raw = schemars::generate::SchemaSettings::draft07()
            .into_generator()
            .into_root_schema_for::<NoParams>()
            .to_value();
        assert_eq!(raw["type"], "null", "guarding the premise of the fix");

        let schema = augment(derive_params_schema::<NoParams>());
        assert_eq!(schema["type"], "object");
        assert!(
            schema["properties"][INTENT_KEY].is_object(),
            "a no-argument tool can still report optional intent metadata"
        );
    }

    #[test]
    fn augment_is_idempotent() {
        let once = augment(derive_params_schema::<Params>());
        let twice = augment(once.clone());

        assert_eq!(once, twice);
        assert_eq!(
            twice["required"],
            serde_json::json!(["command"]),
            "augmentation must not add optional metadata to required"
        );
    }

    #[test]
    fn augment_preserves_a_schema_that_declares_the_keys_itself() {
        let custom = serde_json::json!({
            "type": "object",
            "properties": {
                INTENT_KEY: { "type": "string", "description": "custom intent" },
                ACCEPT_LARGE_OUTPUT_KEY: { "type": "boolean", "description": "custom hatch" },
            }
        });

        let out = augment(custom);

        assert_eq!(
            out["properties"][INTENT_KEY]["description"],
            "custom intent"
        );
        assert_eq!(
            out["properties"][ACCEPT_LARGE_OUTPUT_KEY]["description"],
            "custom hatch"
        );
    }

    #[test]
    fn augment_does_not_guess_at_a_malformed_required_keyword() {
        let broken = serde_json::json!({ "type": "object", "required": "command" });

        let out = augment(broken);

        assert_eq!(out["required"], serde_json::json!("command"));
    }

    #[test]
    fn augment_leaves_non_object_schemas_alone() {
        for schema in [
            serde_json::json!({ "type": "string" }),
            serde_json::json!({ "type": "array", "items": { "type": "string" } }),
            serde_json::json!("not-a-schema"),
        ] {
            assert_eq!(augment(schema.clone()), schema);
        }
    }

    #[test]
    fn object_shape_is_inferred_from_properties_when_type_is_absent() {
        let untyped = serde_json::json!({ "properties": { "path": { "type": "string" } } });

        assert!(is_object_schema(&untyped));
        assert!(augment(untyped)["properties"][INTENT_KEY].is_object());
    }

    /// The shape `plan_update`, `notes`, and `history` derive from: one operation tag.
    ///
    /// The branches disagree on purpose. `expected_revision` and `title` are nullable in
    /// one branch and required elsewhere; `steps` carries a different item type in
    /// `create` and `patch`, exactly as `PlanStepInput` and `PlanStepPatch` do.
    #[derive(JsonSchema, serde::Deserialize)]
    #[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
    #[allow(
        dead_code,
        reason = "the variants are consumed only by the JsonSchema derive in this schema fixture"
    )]
    enum Tagged {
        /// Open a plan.
        Create {
            #[serde(default)]
            expected_revision: Option<i64>,
            title: String,
            steps: Vec<StepInput>,
        },
        Patch {
            expected_revision: i64,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            steps: Vec<StepPatch>,
        },
        Pop {
            expected_revision: i64,
        },
    }

    #[derive(JsonSchema, serde::Deserialize)]
    #[allow(
        dead_code,
        reason = "the fields are consumed only by the JsonSchema derive in this schema fixture"
    )]
    struct StepInput {
        title: String,
    }

    #[derive(JsonSchema, serde::Deserialize)]
    #[allow(
        dead_code,
        reason = "the fields are consumed only by the JsonSchema derive in this schema fixture"
    )]
    struct StepPatch {
        id: String,
        #[serde(default)]
        title: Option<String>,
    }

    #[derive(JsonSchema, serde::Deserialize)]
    #[allow(
        dead_code,
        reason = "the variants are consumed only by the JsonSchema derive in this schema fixture"
    )]
    enum ExternallyTagged {
        Read { path: String },
        Write { path: String, text: String },
    }

    #[derive(JsonSchema, serde::Deserialize)]
    #[serde(untagged)]
    #[allow(
        dead_code,
        reason = "the variants are consumed only by the JsonSchema derive in this schema fixture"
    )]
    enum Untagged {
        Name(String),
        Count(u32),
    }

    #[test]
    fn schema_folds_a_root_tagged_enum_into_one_object() {
        // Guard the premise: schemars renders an internally tagged enum as a root `oneOf`
        // with neither `type` nor `properties`, so `is_object_schema` rejects it and the
        // old normalization shipped `{"type":"object","properties":{}}` for `plan_update`.
        let raw = schemars::generate::SchemaSettings::draft07()
            .into_generator()
            .into_root_schema_for::<Tagged>()
            .to_value();
        assert!(raw["oneOf"].is_array() && !is_object_schema(&raw));

        let schema = derive_params_schema::<Tagged>();

        assert_eq!(schema["type"], "object");
        assert!(schema.get("oneOf").is_none() && schema.get("anyOf").is_none());
        assert_eq!(schema["properties"]["action"]["type"], "string");
        assert_eq!(
            schema["properties"]["action"]["enum"],
            serde_json::json!(["create", "patch", "pop"]),
            "every operation is listed in declaration order"
        );
        assert_eq!(schema["required"], serde_json::json!(["action"]));
        for field in ["expected_revision", "title", "steps"] {
            assert!(
                schema["properties"][field].is_object(),
                "{field} from at least one branch must reach the wire"
            );
        }
        assert!(
            schema.get("additionalProperties").is_none(),
            "the union admits more keys than any one branch; the typed parse rejects strays"
        );
        assert!(
            schema["properties"]["action"].get("const").is_none(),
            "a single const would pin the tag to one branch"
        );

        let augmented = augment(schema);
        assert!(augmented["properties"][INTENT_KEY].is_object());
        assert_eq!(augmented["required"], serde_json::json!(["action"]));
    }

    #[test]
    fn folded_tag_description_names_each_operations_own_required_fields() {
        let schema = derive_params_schema::<Tagged>();

        let description = schema["properties"]["action"]["description"]
            .as_str()
            .expect("the tag carries a description");
        assert!(description.contains("create"), "{description}");
        assert!(description.contains("title, steps"), "{description}");
        assert!(
            description.contains("create: Open a plan; requires title, steps."),
            "{description}"
        );
        assert!(
            description.contains("pop requires expected_revision"),
            "{description}"
        );
    }

    #[test]
    fn folding_keeps_the_nullable_subschema_when_branches_disagree_on_nullability() {
        let schema = derive_params_schema::<Tagged>();

        assert_eq!(
            schema["properties"]["expected_revision"]["type"],
            serde_json::json!(["integer", "null"]),
            "the nullable branch admits everything the required branch does"
        );
        assert_eq!(
            schema["properties"]["title"]["type"],
            serde_json::json!(["string", "null"])
        );
    }

    #[test]
    fn folding_unions_genuinely_different_subschemas_instead_of_choosing_one() {
        let schema = derive_params_schema::<Tagged>();

        let alternatives = schema["properties"]["steps"]["anyOf"]
            .as_array()
            .expect("two item shapes become an anyOf");
        assert_eq!(alternatives.len(), 2);
        assert_eq!(
            alternatives[0]["items"]["required"],
            serde_json::json!(["title"])
        );
        assert_eq!(
            alternatives[1]["items"]["required"],
            serde_json::json!(["id"])
        );
    }

    #[derive(JsonSchema, serde::Deserialize)]
    #[serde(tag = "action", rename_all = "snake_case")]
    #[allow(
        dead_code,
        reason = "the variants are consumed only by the JsonSchema derive in this schema fixture"
    )]
    enum Paged {
        List {
            /// Page size, 1 through 50.
            limit: Option<u32>,
            /// Opaque cursor.
            cursor: Option<String>,
        },
        Search {
            query: String,
            /// Page size, 1 through 20.
            limit: Option<u32>,
            /// Opaque cursor.
            cursor: Option<String>,
        },
    }

    #[test]
    fn folding_attributes_differing_descriptions_instead_of_duplicating_the_shape() {
        // `notes` declares `limit` in three operations with two page ceilings; the
        // shape is the same integer every time, so the wire carries one property whose
        // description says which operation has which ceiling.
        let schema = derive_params_schema::<Paged>();

        let limit = &schema["properties"]["limit"];
        assert!(limit.get("anyOf").is_none(), "{limit}");
        assert_eq!(limit["type"], serde_json::json!(["integer", "null"]));
        assert_eq!(
            limit["description"],
            "list: Page size, 1 through 50. search: Page size, 1 through 20."
        );
        assert_eq!(
            schema["properties"]["cursor"]["description"], "Opaque cursor.",
            "an identical description is not attributed"
        );
    }

    #[test]
    fn folding_is_deterministic_across_derivations() {
        assert_eq!(
            derive_params_schema::<Tagged>(),
            derive_params_schema::<Tagged>()
        );
    }

    #[derive(JsonSchema)]
    #[serde(tag = "action")]
    #[allow(
        dead_code,
        reason = "the fields are consumed only by the JsonSchema derive in this schema fixture"
    )]
    enum Recursive {
        Leaf { value: i64 },
        Node { children: Vec<Recursive> },
    }

    #[test]
    fn a_folded_tagged_enum_keeps_the_definitions_its_refs_point_at() {
        // A recursive variant field makes schemars emit a root `definitions` (or `$defs`)
        // map even with inlining on; the fold rebuilds the root and must carry it over,
        // otherwise every `$ref` under `properties` dangles.
        let raw = schemars::generate::SchemaSettings::draft07()
            .into_generator()
            .into_root_schema_for::<Recursive>()
            .to_value();
        let schema = derive_params_schema::<Recursive>();
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["action"]["enum"],
            serde_json::json!(["Leaf", "Node"])
        );

        for key in ["definitions", "$defs"] {
            if raw.get(key).is_some() {
                assert_eq!(
                    schema.get(key),
                    raw.get(key),
                    "the fold must keep the root {key} map its $ref targets live in"
                );
            }
        }
        // Every `$ref` still resolves: `#` names the folded root itself (an object schema
        // whose `properties` now hold the recursion), and a pointer into a definitions
        // map needs that map to have survived the fold.
        fn refs(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(target)) = map.get("$ref") {
                        out.push(target.clone());
                    }
                    map.values().for_each(|child| refs(child, out));
                }
                Value::Array(items) => items.iter().for_each(|child| refs(child, out)),
                _ => {}
            }
        }
        let mut targets = Vec::new();
        refs(&schema, &mut targets);
        assert!(
            !targets.is_empty(),
            "a recursive variant must reference itself"
        );
        for target in targets {
            if target == "#" {
                continue;
            }
            let map = if target.starts_with("#/definitions/") {
                "definitions"
            } else if target.starts_with("#/$defs/") {
                "$defs"
            } else {
                panic!("unexpected $ref shape {target}");
            };
            let name = target
                .rsplit('/')
                .next()
                .expect("a $ref names a definition");
            assert!(
                schema[map].get(name).is_some(),
                "{target} dangles after the fold: {schema}"
            );
        }
    }

    #[test]
    fn unions_that_are_not_tagged_enums_still_normalize_to_an_empty_object() {
        for schema in [
            derive_params_schema::<ExternallyTagged>(),
            derive_params_schema::<Untagged>(),
        ] {
            assert_eq!(
                schema,
                serde_json::json!({ "type": "object", "properties": {} }),
                "no shared const tag means no fold"
            );
        }
    }
}
