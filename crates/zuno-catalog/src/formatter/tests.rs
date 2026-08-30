//! Round-trip and resolution tests for the `formatter` union.

use super::*;

fn round_trip(raw: &str) -> FormatterConfig {
    let config: FormatterConfig = serde_json::from_str(raw).expect("arm should parse");
    let back = serde_json::to_value(&config).expect("arm should serialize");
    let original: serde_json::Value = serde_json::from_str(raw).expect("input should be JSON");
    assert_eq!(back, original, "round trip changed the document");
    config
}

#[test]
fn arm_one_boolean_round_trips_both_ways() {
    assert_eq!(round_trip("true"), FormatterConfig::Enabled(true));
    assert_eq!(round_trip("false"), FormatterConfig::Enabled(false));
}

#[test]
fn arm_two_record_round_trips_with_every_field() {
    let raw = r#"{"prettier":{"disabled":false,"command":["prettier","--write","$FILE"],"environment":{"NODE_ENV":"production"},"extensions":[".ts",".tsx"]}}"#;
    let config = round_trip(raw);
    let FormatterConfig::Formatters(map) = &config else {
        panic!("expected the record arm, got {config:?}");
    };
    let entry = map.get("prettier").expect("declared");
    assert_eq!(entry.command.as_deref().map(<[String]>::len), Some(3));
    assert_eq!(
        entry.extensions.as_deref(),
        Some([".ts".to_owned(), ".tsx".to_owned()].as_slice())
    );
}

#[test]
fn an_empty_entry_round_trips_to_an_empty_object() {
    let config = round_trip(r#"{"gofmt":{}}"#);
    let FormatterConfig::Formatters(map) = &config else {
        panic!("expected the record arm, got {config:?}");
    };
    assert_eq!(map.get("gofmt"), Some(&FormatterEntry::default()));
}

#[test]
fn an_absent_key_disables_every_formatter() {
    let resolved = ResolvedFormatters::resolve(None);
    assert!(!resolved.is_enabled());
    assert!(!resolved.is_formatter_enabled("prettier"));
    assert_eq!(resolved.command_for("prettier"), None);
}

#[test]
fn false_disables_every_formatter() {
    let config = FormatterConfig::Enabled(false);
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert!(!resolved.is_enabled());
    assert!(!resolved.is_formatter_enabled("gofmt"));
}

#[test]
fn true_enables_the_builtins_with_no_overrides() {
    let config = FormatterConfig::Enabled(true);
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert!(resolved.is_enabled());
    assert!(resolved.is_formatter_enabled("gofmt"));
    assert_eq!(resolved.overrides().count(), 0);
    assert_eq!(resolved.command_for("gofmt"), None, "no override to report");
}

#[test]
fn a_record_disables_exactly_the_listed_formatter() {
    let config: FormatterConfig =
        serde_json::from_str(r#"{"gofmt":{"disabled":true},"prettier":{"command":["p"]}}"#)
            .expect("parses");
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert!(resolved.is_enabled());
    assert!(!resolved.is_formatter_enabled("gofmt"));
    assert!(resolved.is_formatter_enabled("prettier"));
    assert!(
        resolved.is_formatter_enabled("rustfmt"),
        "an unmentioned built-in stays enabled"
    );
    assert_eq!(resolved.disabled().collect::<Vec<_>>(), vec!["gofmt"]);
    assert_eq!(
        resolved.command_for("prettier"),
        Some(["p".to_owned()].as_slice())
    );
}

#[test]
fn disabling_ruff_also_disables_uv_because_they_share_a_backend() {
    let config: FormatterConfig =
        serde_json::from_str(r#"{"ruff":{"disabled":true}}"#).expect("parses");
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert!(!resolved.is_formatter_enabled("ruff"));
    assert!(!resolved.is_formatter_enabled("uv"));
    let mut names = resolved.disabled().collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, vec!["ruff", "uv"]);
}

#[test]
fn disabling_uv_also_disables_ruff_and_drops_a_ruff_override() {
    let config: FormatterConfig =
        serde_json::from_str(r#"{"ruff":{"command":["ruff","format"]},"uv":{"disabled":true}}"#)
            .expect("parses");
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert!(!resolved.is_formatter_enabled("ruff"));
    assert!(!resolved.is_formatter_enabled("uv"));
    assert_eq!(resolved.get("ruff"), None, "the override must be dropped");
    assert_eq!(resolved.command_for("ruff"), None);
}

#[test]
fn overrides_keep_declaration_order_and_default_an_absent_environment() {
    let config: FormatterConfig =
        serde_json::from_str(r#"{"z":{"command":["z"]},"a":{"command":["a"]}}"#).expect("parses");
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert_eq!(
        resolved
            .overrides()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["z", "a"]
    );
    assert!(
        resolved.get("z").expect("declared").environment.is_empty(),
        "an absent environment resolves to empty, not to None"
    );
}

#[test]
fn for_extension_matches_the_leading_dot_form_the_runtime_uses() {
    let config: FormatterConfig = serde_json::from_str(
        r#"{"prettier":{"command":["p"],"extensions":[".ts"]},"gofmt":{"command":["g"],"extensions":[".go"]}}"#,
    )
    .expect("parses");
    let resolved = ResolvedFormatters::resolve(Some(&config));
    assert_eq!(
        resolved
            .for_extension(".ts")
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["prettier"]
    );
    assert_eq!(resolved.for_extension("ts").count(), 0, "the dot matters");
}
