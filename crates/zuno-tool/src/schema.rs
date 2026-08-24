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
/// A params type whose schema is not object-shaped is normalized to an empty
/// object schema. `#[derive(JsonSchema)]` on a unit struct yields `{"type":
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
    } else {
        serde_json::json!({ "type": "object", "properties": {} })
    }
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
}
