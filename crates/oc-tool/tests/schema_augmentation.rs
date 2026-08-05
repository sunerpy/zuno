//! Central augmentation: what it injects, and what it costs on every request.

use async_trait::async_trait;
use oc_error::ToolError;
use oc_tool::schema::{
    ACCEPT_LARGE_OUTPUT_KEY, INTENT_KEY, augment, derive_params_schema, params_schema,
};
use oc_tool::{Tool, ToolContext, ToolOutput, TypedTool, erase};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, JsonSchema)]
struct ReadParams {
    /// The file to read.
    file_path: String,
    /// First line to return, 1-based.
    #[serde(default)]
    offset: Option<u32>,
    /// How many lines to return.
    #[serde(default)]
    limit: Option<u32>,
}

struct Read;

#[async_trait]
impl TypedTool for Read {
    type Params = ReadParams;

    fn id(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file."
    }

    async fn run(&self, params: ReadParams, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("read", params.file_path)
            .with_metadata("offset", params.offset.unwrap_or(1))
            .with_metadata("limit", params.limit.unwrap_or(u32::MAX)))
    }
}

#[test]
fn the_injected_intent_is_present_and_required() {
    let parameters = erase(Read).definition().parameters;

    assert_eq!(parameters["properties"][INTENT_KEY]["type"], "string");
    let required: Vec<&str> = parameters["required"]
        .as_array()
        .expect("required array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        required.contains(&INTENT_KEY),
        "an optional intent is an intent the model will skip"
    );
    assert!(
        required.contains(&"file_path"),
        "the tool's own requirement survives"
    );
}

#[test]
fn the_escape_hatch_is_offered_but_never_required() {
    let parameters = erase(Read).definition().parameters;

    assert_eq!(
        parameters["properties"][ACCEPT_LARGE_OUTPUT_KEY]["type"],
        "boolean"
    );
    let required: Vec<&str> = parameters["required"]
        .as_array()
        .expect("required array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !required.contains(&ACCEPT_LARGE_OUTPUT_KEY),
        "requiring it would make the model answer a token-budget question every call"
    );
}

#[test]
fn a_derived_schema_is_valid_json_schema_with_no_hand_written_json() {
    let parameters = erase(Read).definition().parameters;

    // Shape: an object schema, properties as an object, required as an array of
    // strings, every property a subschema with a type.
    assert_eq!(parameters["type"], "object");
    let properties = parameters["properties"]
        .as_object()
        .expect("properties object");
    for (name, subschema) in properties {
        assert!(
            subschema.get("type").is_some(),
            "`{name}` has no declared type"
        );
    }
    for entry in parameters["required"].as_array().expect("required array") {
        let name = entry.as_str().expect("required entries are strings");
        assert!(
            properties.contains_key(name),
            "`{name}` is required but not declared"
        );
    }

    // Optionality came from `Option<T>`, not from a hand-maintained list.
    assert!(properties.contains_key("offset"));
    assert_eq!(
        parameters["required"],
        json!(["file_path", INTENT_KEY]),
        "only the non-Option field and the injected intent are required"
    );
}

#[test]
fn augmentation_costs_a_bounded_number_of_bytes_per_request() {
    // This rides on every tool schema on every request for the life of a session, so
    // each word is paid forever. The ceiling is here so an edit that doubles the
    // descriptions has to be a deliberate one.
    let raw = derive_params_schema::<ReadParams>();
    let augmented = augment(raw.clone());

    let before = serde_json::to_string(&raw).expect("serializable").len();
    let after = serde_json::to_string(&augmented)
        .expect("serializable")
        .len();
    let cost = after - before;

    println!("augmentation cost: {cost} bytes ({before} -> {after})");
    assert!(
        cost <= 260,
        "augmentation grew to {cost} bytes per tool per request"
    );
    assert!(cost > 0, "the augmentation did nothing");
}

#[test]
fn augmentation_is_the_only_difference_from_the_derived_schema() {
    let raw = derive_params_schema::<ReadParams>();
    let mut augmented = augment(raw.clone());

    // Undo exactly what augmentation claims to do; the result must be byte-identical
    // to the derived schema, which proves nothing else was touched.
    let object = augmented.as_object_mut().expect("object schema");
    let properties = object["properties"].as_object_mut().expect("properties");
    properties.remove(INTENT_KEY);
    properties.remove(ACCEPT_LARGE_OUTPUT_KEY);
    let required = object["required"].as_array_mut().expect("required");
    required.retain(|entry| entry.as_str() != Some(INTENT_KEY));

    assert_eq!(augmented, raw);
}

#[test]
fn params_schema_derives_and_augments_in_one_step() {
    assert_eq!(
        params_schema::<ReadParams>(),
        erase(Read).definition().parameters,
        "the definition path and the helper must agree"
    );
}

/// A proxy relaying a remote MCP server's schema: no local type describes it.
struct McpProxy(Value);

#[async_trait]
impl Tool for McpProxy {
    fn id(&self) -> &str {
        "codegraph_explore"
    }

    fn description(&self) -> &str {
        "Explore an indexed codebase."
    }

    fn raw_parameters_schema(&self) -> Value {
        self.0.clone()
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("proxied", args.to_string()))
    }
}

#[test]
fn a_proxied_tool_is_augmented_without_editing_the_remote_schema() {
    let proxy = McpProxy(json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" },
            "projectPath": { "type": "string" },
        },
        "required": ["query", "projectPath"],
        "additionalProperties": false,
    }));

    let parameters = proxy.definition().parameters;

    assert_eq!(parameters["properties"][INTENT_KEY]["type"], "string");
    assert_eq!(
        parameters["properties"][ACCEPT_LARGE_OUTPUT_KEY]["type"],
        "boolean"
    );
    assert_eq!(
        parameters["required"],
        json!(["query", "projectPath", INTENT_KEY])
    );
    // The server's own keywords are untouched.
    assert_eq!(parameters["additionalProperties"], false);
}

#[test]
fn a_proxied_schema_with_no_declared_type_is_still_augmented() {
    // Legal in every draft and common from MCP servers.
    let proxy = McpProxy(json!({ "properties": { "path": { "type": "string" } } }));

    let parameters = proxy.definition().parameters;

    assert_eq!(parameters["properties"][INTENT_KEY]["type"], "string");
    assert_eq!(parameters["required"], json!([INTENT_KEY]));
}
