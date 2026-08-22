//! Machine-readable JSON Schema generated from the types that parse `zuno.json`.

use serde_json::Value;

use crate::Config;

/// Generate the repository's canonical `zuno.json` schema.
#[must_use]
pub fn document() -> Value {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<Config>()
        .to_value();
    let root = schema
        .as_object_mut()
        .expect("a derived root schema is always an object");
    root.insert(
        "title".to_owned(),
        Value::String("Zuno configuration".to_owned()),
    );
    root.insert(
        "description".to_owned(),
        Value::String(
            "Configuration accepted from zuno.json and zuno.jsonc layers. TUI-only settings such as theme and mouse belong in tui.json.".to_owned(),
        ),
    );
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_schema_matches_the_rust_configuration_types() {
        let committed: Value = serde_json::from_str(include_str!("../../../schemas/zuno.json"))
            .expect("committed schema is valid JSON");
        assert_eq!(
            committed,
            document(),
            "regenerate with `cargo run -p zuno-config --example generate-schema > schemas/zuno.json`"
        );
    }

    #[test]
    fn schema_rejects_tui_only_top_level_keys() {
        let schema = document();
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        assert!(schema["properties"].get("theme").is_none());
        assert!(schema["properties"].get("mouse").is_none());
    }
}
