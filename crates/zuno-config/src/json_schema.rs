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
    use std::collections::BTreeSet;

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

    /// The published `toolOutput` text must describe what the limits do.
    ///
    /// These descriptions ship in editor completion, and they said output was truncated
    /// at the threshold. Nothing is truncated: the whole output is saved and withheld
    /// behind a notice. A user who believed the old text raised the limit to get their
    /// output back, which is the opposite of what the limit is for.
    ///
    /// The claim is what is pinned, not the wording. Two packages in the same batch wrote
    /// this description and this guard independently, and the guard first demanded the word
    /// `inlined` — so the clearer rewrite, which says the same thing in plainer words, broke
    /// a test about truncation by improving a sentence. What a reader must be able to learn
    /// is that output above the threshold is kept and fetched, so that is what is asserted.
    #[test]
    fn the_published_tool_output_limits_describe_withholding_and_not_truncation() {
        let schema = document();
        let published = schema["$defs"]["ToolOutputConfig"].clone();
        let text = serde_json::to_string(&published).expect("a published definition is JSON");

        assert!(
            !text.contains("truncation"),
            "the limits withhold output, they do not truncate it: {text}"
        );
        for field in ["max_lines", "max_bytes"] {
            let description = published["properties"][field]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(
                description.contains("never truncated") && description.contains("read back"),
                "{field} must say that output above the threshold is kept and read back \
                 rather than cut: {description}"
            );
        }
    }

    /// The published schema and the parser's whitelist are two hand-maintained
    /// lists of the same thing. [`crate::schema::KNOWN_TOP_LEVEL_KEYS`] is what
    /// actually rejects a key at parse time, so a property that the schema
    /// publishes but the whitelist omits is a documented key that no user can
    /// set.
    #[test]
    fn the_known_key_whitelist_matches_the_committed_schema_root_properties() {
        let committed: Value = serde_json::from_str(include_str!("../../../schemas/zuno.json"))
            .expect("committed schema is valid JSON");
        let published: BTreeSet<&str> = committed["properties"]
            .as_object()
            .expect("the schema root declares its properties")
            .keys()
            .map(String::as_str)
            .collect();
        let whitelisted: BTreeSet<&str> = crate::schema::KNOWN_TOP_LEVEL_KEYS
            .iter()
            .copied()
            .collect();

        let rejected_but_published: Vec<&str> =
            published.difference(&whitelisted).copied().collect();
        let whitelisted_but_unpublished: Vec<&str> =
            whitelisted.difference(&published).copied().collect();
        assert!(
            rejected_but_published.is_empty() && whitelisted_but_unpublished.is_empty(),
            "schemas/zuno.json and KNOWN_TOP_LEVEL_KEYS disagree; \
             published in the schema but rejected by the parser: {rejected_but_published:?}; \
             whitelisted by the parser but absent from the schema: {whitelisted_but_unpublished:?}"
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
