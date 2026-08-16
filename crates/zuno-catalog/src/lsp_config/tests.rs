//! Round-trip and resolution tests for the `lsp` unions, outer and inner.

use super::*;

fn round_trip(raw: &str) -> LspConfig {
    let config: LspConfig = serde_json::from_str(raw).expect("arm should parse");
    let back = serde_json::to_value(&config).expect("arm should serialize");
    let original: serde_json::Value = serde_json::from_str(raw).expect("input should be JSON");
    assert_eq!(back, original, "round trip changed the document");
    config
}

#[test]
fn outer_arm_one_boolean_round_trips_both_ways() {
    assert_eq!(round_trip("true"), LspConfig::Enabled(true));
    assert_eq!(round_trip("false"), LspConfig::Enabled(false));
}

#[test]
fn outer_arm_two_record_round_trips() {
    let raw = r#"{"gopls":{"command":["gopls"]}}"#;
    let config = round_trip(raw);
    let LspConfig::Servers(map) = &config else {
        panic!("expected the record arm, got {config:?}");
    };
    assert_eq!(map.len(), 1);
}

#[test]
fn inner_arm_one_disabled_only_round_trips() {
    let config = round_trip(r#"{"gopls":{"disabled":true}}"#);
    let LspConfig::Servers(map) = &config else {
        panic!("expected the record arm, got {config:?}");
    };
    let entry = map.get("gopls").expect("declared");
    assert!(entry.is_disabled());
    assert_eq!(entry.command, None, "the disable-only arm has no command");
}

#[test]
fn inner_arm_two_full_entry_round_trips_with_every_field() {
    let raw = r#"{"my-server":{"command":["my-lsp","--stdio"],"extensions":[".my"],"disabled":false,"env":{"MY_LOG":"debug"},"initialization":{"nested":{"flag":true}}}}"#;
    let config = round_trip(raw);
    let LspConfig::Servers(map) = &config else {
        panic!("expected the record arm, got {config:?}");
    };
    let entry = map.get("my-server").expect("declared");
    assert_eq!(entry.env.as_ref().map(BTreeMap::len), Some(1));
    assert!(entry.initialization.is_some());
    assert!(!entry.is_disabled());
}

#[test]
fn lsp_false_disables_all_while_one_disabled_entry_disables_exactly_one() {
    let all_off = LspConfig::Enabled(false);
    let all_off = ResolvedLsp::resolve(Some(&all_off));
    assert!(!all_off.is_enabled());
    for id in ["gopls", "typescript", "rust", "clangd"] {
        assert!(
            !all_off.is_server_enabled(id),
            "`lsp: false` must disable {id}"
        );
    }

    let one_off: LspConfig =
        serde_json::from_str(r#"{"gopls":{"disabled":true}}"#).expect("parses");
    let one_off = ResolvedLsp::resolve(Some(&one_off));
    assert!(one_off.is_enabled(), "a record arm still enables LSP");
    assert!(!one_off.is_server_enabled("gopls"), "gopls must be off");
    for id in ["typescript", "rust", "clangd"] {
        assert!(
            one_off.is_server_enabled(id),
            "{id} must stay on when only gopls is disabled"
        );
    }
    assert_eq!(one_off.disabled().collect::<Vec<_>>(), vec!["gopls"]);
}

#[test]
fn an_absent_key_disables_every_server() {
    let resolved = ResolvedLsp::resolve(None);
    assert!(!resolved.is_enabled());
    assert!(!resolved.is_server_enabled("gopls"));
    assert_eq!(resolved.command_for("gopls"), None);
    assert_eq!(resolved.extensions_for("gopls"), None);
    assert_eq!(resolved.initialization_for("gopls"), None);
}

#[test]
fn true_enables_the_builtins_with_no_overrides() {
    let config = LspConfig::Enabled(true);
    let resolved = ResolvedLsp::resolve(Some(&config));
    assert!(resolved.is_enabled());
    assert!(resolved.is_server_enabled("typescript"));
    assert_eq!(resolved.servers().count(), 0);
}

#[test]
fn the_resolved_view_answers_command_extensions_and_initialization() {
    let config: LspConfig = serde_json::from_str(
        r#"{"my-server":{"command":["my-lsp","--stdio"],"extensions":[".my"],"env":{"A":"b"},"initialization":{"k":1}}}"#,
    )
    .expect("parses");
    let resolved = ResolvedLsp::resolve(Some(&config));
    assert_eq!(
        resolved.command_for("my-server"),
        Some(["my-lsp".to_owned(), "--stdio".to_owned()].as_slice())
    );
    assert_eq!(
        resolved.extensions_for("my-server"),
        Some([".my".to_owned()].as_slice())
    );
    assert_eq!(
        resolved
            .initialization_for("my-server")
            .and_then(|map| map.get("k")),
        Some(&serde_json::json!(1))
    );
    let server = resolved.get("my-server").expect("declared");
    assert_eq!(server.env.get("A").map(String::as_str), Some("b"));
    assert!(!server.is_builtin(), "my-server is not a built-in id");
    assert_eq!(
        resolved
            .for_extension(".my")
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        vec!["my-server"]
    );
}

#[test]
fn a_builtin_override_without_extensions_reports_none_not_an_empty_slice() {
    let config: LspConfig =
        serde_json::from_str(r#"{"gopls":{"command":["gopls","serve"]}}"#).expect("parses");
    let resolved = ResolvedLsp::resolve(Some(&config));
    assert_eq!(
        resolved.extensions_for("gopls"),
        None,
        "no extensions means keep the built-in's"
    );
    assert!(resolved.get("gopls").expect("declared").is_builtin());
}

#[test]
fn a_disabled_server_reports_nothing_from_the_resolved_view() {
    let config: LspConfig =
        serde_json::from_str(r#"{"gopls":{"disabled":true},"rust":{"command":["rust-analyzer"]}}"#)
            .expect("parses");
    let resolved = ResolvedLsp::resolve(Some(&config));
    assert_eq!(resolved.command_for("gopls"), None);
    assert_eq!(resolved.get("gopls"), None);
    assert_eq!(
        resolved.command_for("rust"),
        Some(["rust-analyzer".to_owned()].as_slice())
    );
}

#[test]
fn overrides_keep_declaration_order() {
    let config: LspConfig =
        serde_json::from_str(r#"{"zls":{"command":["zls"]},"clangd":{"command":["clangd"]}}"#)
            .expect("parses");
    let resolved = ResolvedLsp::resolve(Some(&config));
    assert_eq!(
        resolved
            .servers()
            .map(|server| server.id.as_str())
            .collect::<Vec<_>>(),
        vec!["zls", "clangd"]
    );
}

#[test]
fn the_builtin_id_list_is_the_oracles_thirty_eight() {
    assert_eq!(BUILTIN_SERVER_IDS.len(), 38);
    assert!(BUILTIN_SERVER_IDS.contains(&"php intelephense"));
}
