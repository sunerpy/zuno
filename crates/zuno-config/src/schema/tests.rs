//! Tests for the config schema.
//!
//! Named `schema::tests::*` so `cargo test -p zuno-config schema` selects them.

use super::*;
use crate::schema::agent::{
    AgentColor, AgentConfig, AgentMode, AgentReasoning, SWEEP_EXEMPT_KEYS, ThemeColor,
};
use crate::schema::formatter::FormatterConfig;
use crate::schema::lsp::{BUILTIN_SERVER_IDS, LspConfig};
use crate::schema::mcp::{McpOauth, McpServerConfig};
use crate::schema::ordered::False;
use crate::schema::permission::{PermissionAction, PermissionMode, PermissionRule};
use crate::schema::product_agent::{ProductAgentKind, ProductAgentPermissionMode};
use crate::schema::provider::{ResponsesTextBlocks, Timeout};
use crate::schema::reference::ReferenceEntry;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use zuno_error::ConfigError;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> String {
    let path = PathBuf::from(FIXTURES).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::from_json_str(Path::new("opencode.json"), text)
}

fn parse_value(value: Value) -> Result<Config, ConfigError> {
    Config::from_json_value(Path::new("opencode.json"), value)
}

/// The rendered key path of the single issue in an `Invalid` error.
fn issue_path(error: &ConfigError) -> String {
    let ConfigError::Invalid { issues, .. } = error else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(issues.len(), 1, "expected one issue, got {issues:?}");
    issues[0].key_path.join(".")
}

fn issue_detail(error: &ConfigError) -> String {
    let ConfigError::Invalid { issues, .. } = error else {
        panic!("expected Invalid, got {error:?}");
    };
    issues[0].detail.clone()
}

/// JSON with every number reduced to its `f64` value.
///
/// JSON has one number type; `serde_json` keeps the integer/float distinction of
/// the text it read, so `272000` and `272000.0` are unequal `Value`s despite being
/// the same JSON number. Comparing after this pass compares the documents, not
/// their spelling.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Number(number) => number
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| value.clone(), Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| (k.clone(), canonical(v)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Every key of `expected` appears in `actual` with an equal value, recursively.
fn assert_contains(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            for (key, want) in expected {
                let got = actual
                    .get(key)
                    .unwrap_or_else(|| panic!("{path}.{key} was dropped"));
                assert_contains(got, want, &format!("{path}.{key}"));
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            assert_eq!(actual.len(), expected.len(), "{path} changed length");
            for (index, want) in expected.iter().enumerate() {
                assert_contains(&actual[index], want, &format!("{path}.{index}"));
            }
        }
        _ => assert_eq!(
            canonical(actual),
            canonical(expected),
            "{path} changed value"
        ),
    }
}

// ---------------------------------------------------------------------------
// Acceptance: every top-level key round-trips with no field loss.
// ---------------------------------------------------------------------------

#[test]
fn the_all_keys_fixture_uses_every_top_level_key() {
    let text = fixture("all-keys.json");
    let value: Value = serde_json::from_str(&text).expect("fixture is valid JSON");
    let present: Vec<&str> = value
        .as_object()
        .expect("fixture is an object")
        .keys()
        .map(String::as_str)
        .collect();
    let mut missing: Vec<&str> = KNOWN_TOP_LEVEL_KEYS
        .iter()
        .copied()
        .filter(|key| !present.contains(key))
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "fixture does not exercise these keys: {missing:?}"
    );
    assert_eq!(present.len(), KNOWN_TOP_LEVEL_KEYS.len());
    assert!(
        KNOWN_TOP_LEVEL_KEYS.len() >= 30,
        "the plan promises 30+ keys"
    );
}

#[test]
fn every_top_level_key_round_trips_without_field_loss() {
    let text = fixture("all-keys.json");
    let before: Value = serde_json::from_str(&text).expect("fixture is valid JSON");
    let config = parse(&text).expect("fixture deserializes");
    let after = serde_json::to_value(&config).expect("config serializes");
    assert_eq!(
        canonical(&after),
        canonical(&before),
        "round trip changed the document"
    );
}

#[test]
fn round_trip_is_stable_on_a_second_pass() {
    let text = fixture("all-keys.json");
    let once = parse(&text).expect("fixture deserializes");
    let json = serde_json::to_string(&once).expect("serializes");
    let twice = parse(&json).expect("re-parses");
    assert_eq!(once, twice);
}

#[test]
fn presets_are_structured_ordered_and_use_canonical_reasoning() {
    let config = parse(
        r#"{
          "preset":"house",
          "presets":{
            "house":{
              "agents":{
                "orchestrator":{"model":"vendor/frontier","reasoning":"max"},
                "explorer":"vendor/fast"
              },
              "categories":{"cheap":"vendor/fast"}
            }
          }
        }"#,
    )
    .expect("typed preset parses");

    assert_eq!(config.preset.as_deref(), Some("house"));
    let preset = config
        .presets
        .as_ref()
        .and_then(|presets| presets.get("house"))
        .expect("named preset is retained");
    assert_eq!(
        preset.agents.keys().collect::<Vec<_>>(),
        vec!["orchestrator", "explorer"]
    );
    let orchestrator = preset
        .agents
        .get("orchestrator")
        .expect("orchestrator route exists");
    assert_eq!(orchestrator.model(), "vendor/frontier");
    assert_eq!(orchestrator.reasoning(), Some(AgentReasoning::Max));
    assert_eq!(
        preset
            .categories
            .get("cheap")
            .expect("category exists")
            .model(),
        "vendor/fast"
    );
}

#[test]
fn presets_reject_the_unreleased_flat_body_and_unknown_model_options() {
    let flat = parse(r#"{"presets":{"house":{"orchestrator":"vendor/frontier"}}}"#)
        .expect_err("a preset body must use explicit agents/categories sections");
    assert_eq!(issue_path(&flat), "presets.house.orchestrator");

    let unknown = parse(
        r#"{"presets":{"house":{"agents":{"orchestrator":{"model":"vendor/frontier","variant":"thinking"}}}}}"#,
    )
    .expect_err("preset choices accept canonical reasoning, not provider-specific variants");
    assert_eq!(
        issue_path(&unknown),
        "presets.house.agents.orchestrator.variant"
    );
    assert!(issue_detail(&unknown).contains("did not match any variant"));
}

#[test]
fn memory_false_dominates_every_enabled_default() {
    let config = parse(r#"{"memory":false}"#).expect("master switch parses");
    let memory = config.resolved_memory();

    assert!(!memory.enabled);
    assert!(!memory.resident);
    assert!(!memory.tool);
    assert!(!memory.reflection);
    assert_eq!(memory.global_char_limit, 2_200);
    assert_eq!(memory.project_char_limit, 3_000);
    assert_eq!(memory.nudge_interval, 10);
    assert_eq!(memory.promotion, MemoryPromotion::Review);
    assert_eq!(memory.auto_confidence, 0.9);
}

#[test]
fn permission_mode_defaults_to_standard_and_legacy_authorization_is_rejected() {
    assert_eq!(
        Config::default().permission_mode(),
        PermissionMode::Standard
    );
    assert!(!Config::default().strict_authorization());

    let error = parse(r#"{"authorization":{"strict":true}}"#)
        .expect_err("legacy authorization configuration must be rejected");
    assert_eq!(issue_path(&error), "authorization");
}

#[test]
fn canonical_permission_modes_keep_ordered_rules_and_allow_all() {
    let config =
        parse(r#"{"permission":{"mode":"strict","rules":{"shell":"ask","read":"allow"}}}"#)
            .expect("canonical permission policy parses");
    assert_eq!(config.permission_mode(), PermissionMode::Strict);
    let policy = config
        .permission
        .expect("canonical permission policy was not retained");
    assert_eq!(
        policy.rules.iter().map(|(key, _)| key).collect::<Vec<_>>(),
        vec!["shell", "read"]
    );

    let allow_all = parse(r#"{"permission":{"mode":"allow_all"}}"#)
        .expect("allow_all permission policy parses");
    assert_eq!(allow_all.permission_mode(), PermissionMode::AllowAll);
    assert!(!allow_all.strict_authorization());
}

#[test]
fn danger_full_access_effectively_disables_hitl_prompts() {
    let danger = parse(
        r#"{
            "permission": {"mode": "strict"},
            "sandbox": {"mode": "danger-full-access"}
        }"#,
    )
    .expect("trusted full access configuration parses");

    assert_eq!(
        danger.permission_mode(),
        PermissionMode::Strict,
        "the authored permission mode remains inspectable"
    );
    assert_eq!(
        danger.effective_permission_mode(),
        PermissionMode::AllowAll,
        "danger-full-access is the explicit no-approval execution mode"
    );
    assert!(!danger.strict_authorization());

    let confined = parse(
        r#"{
            "permission": {"mode": "strict"},
            "sandbox": {"mode": "workspace-write"}
        }"#,
    )
    .expect("confined strict configuration parses");
    assert_eq!(confined.effective_permission_mode(), PermissionMode::Strict);
}

#[test]
fn compaction_threshold_percent_is_typed_and_bounded() {
    let config = parse(r#"{"compaction":{"auto":true,"threshold_percent":80}}"#)
        .expect("bounded compaction threshold parses");
    assert_eq!(
        config
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.threshold_percent)
            .map(CompactionThresholdPercent::get),
        Some(80)
    );

    for value in [0, 101] {
        let error = parse(&format!(
            r#"{{"compaction":{{"threshold_percent":{value}}}}}"#
        ))
        .expect_err("compaction percentage outside 1..=100 must fail");
        assert_eq!(issue_path(&error), "compaction.threshold_percent");
    }
}

#[test]
fn web_search_accepts_an_explicit_profile_master_switch() {
    let disabled =
        parse(r#"{"web_search":{"enabled":false}}"#).expect("the web-search master switch parses");
    assert_eq!(
        serde_json::to_value(disabled).expect("web-search config serializes")["web_search"]["enabled"],
        false
    );
}

#[test]
fn sandbox_defaults_to_denied_network_and_preserves_explicit_paths() {
    let config = parse(
        r#"{
            "sandbox": {
                "writableRoots": ["../shared-cache"],
                "protectedPaths": [".zuno", ".agents"]
            }
        }"#,
    )
    .expect("sandbox config parses");
    let sandbox = config.sandbox.expect("sandbox config");

    assert_eq!(
        sandbox.resolved_network(),
        crate::schema::sandbox::SandboxNetworkMode::Deny
    );
    assert_eq!(
        sandbox.resolved_on_unavailable(),
        crate::schema::sandbox::SandboxUnavailableAction::Deny
    );
    assert_eq!(
        sandbox.writable_roots.as_deref(),
        Some(["../shared-cache".to_owned()].as_slice())
    );
    assert_eq!(
        sandbox.protected_paths.as_deref(),
        Some([".zuno".to_owned(), ".agents".to_owned()].as_slice())
    );
}

#[test]
fn sandbox_modes_use_the_exact_public_vocabulary_and_default_to_workspace_write() {
    use crate::schema::sandbox::{SandboxMode, SandboxNetworkMode, SandboxUnavailableAction};

    assert_eq!(
        Config::default().sandbox_mode(),
        SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        Config::default().sandbox_network(),
        SandboxNetworkMode::Deny
    );
    assert_eq!(
        Config::default().sandbox_on_unavailable(),
        SandboxUnavailableAction::Deny
    );

    for (spelling, expected) in [
        ("read-only", SandboxMode::ReadOnly),
        ("workspace-write", SandboxMode::WorkspaceWrite),
        ("danger-full-access", SandboxMode::DangerFullAccess),
    ] {
        let config = parse(&format!(r#"{{"sandbox":{{"mode":"{spelling}"}}}}"#))
            .unwrap_or_else(|error| panic!("{spelling} must parse: {error:?}"));
        assert_eq!(config.sandbox_mode(), expected);
    }

    let danger = parse(r#"{"sandbox":{"mode":"danger-full-access"}}"#)
        .expect("full access has host networking when no contradictory network policy is set");
    assert_eq!(danger.sandbox_network(), SandboxNetworkMode::Allow);

    for (spelling, expected) in [
        ("deny", SandboxUnavailableAction::Deny),
        ("run-unconfined", SandboxUnavailableAction::RunUnconfined),
    ] {
        let config = parse(&format!(
            r#"{{"sandbox":{{"onUnavailable":"{spelling}"}}}}"#
        ))
        .unwrap_or_else(|error| panic!("{spelling} must parse: {error:?}"));
        assert_eq!(config.sandbox_on_unavailable(), expected);
    }
}

#[test]
fn sandbox_rejects_options_that_cannot_be_enforced_by_the_selected_mode() {
    for (document, path, detail) in [
        (
            r#"{"sandbox":{"mode":"read-only","writableRoots":["../cache"]}}"#,
            "sandbox.writableRoots",
            "read-only",
        ),
        (
            r#"{"sandbox":{"mode":"danger-full-access","network":"deny"}}"#,
            "sandbox.network",
            "host network",
        ),
        (
            r#"{"sandbox":{"mode":"danger-full-access","writableRoots":["../cache"]}}"#,
            "sandbox.writableRoots",
            "danger-full-access",
        ),
        (
            r#"{"sandbox":{"mode":"danger-full-access","protectedPaths":[".zuno"]}}"#,
            "sandbox.protectedPaths",
            "danger-full-access",
        ),
    ] {
        let error = parse(document).expect_err("contradictory sandbox policy must fail");
        assert_eq!(issue_path(&error), path);
        assert!(
            issue_detail(&error).contains(detail),
            "{path}: {}",
            issue_detail(&error)
        );
    }
}

#[test]
fn permission_rejects_unknown_policy_keys() {
    let error = parse(r#"{"permission":{"mode":"strict","remember":true}}"#)
        .expect_err("permission must not silently ignore unknown policy fields");
    assert_eq!(issue_path(&error), "permission.remember");
}

#[test]
fn memory_options_resolve_caps_cadence_and_component_flags() {
    let config = parse(
        r#"{"memory":{"resident":false,"tool":false,"reflection":false,"global_char_limit":1200,"project_char_limit":2400,"nudge_interval":0}}"#,
    )
    .expect("memory options parse");
    let memory = config.resolved_memory();

    assert!(memory.enabled);
    assert!(!memory.resident);
    assert!(!memory.tool);
    assert!(!memory.reflection);
    assert_eq!(memory.global_char_limit, 1_200);
    assert_eq!(memory.project_char_limit, 2_400);
    assert_eq!(memory.nudge_interval, 0);
}

#[test]
fn memory_promotion_and_confidence_are_typed_and_resolved() {
    let config = parse(r#"{"memory":{"promotion":"high_confidence","auto_confidence":0.95}}"#)
        .expect("memory promotion parses");
    let memory = config.resolved_memory();

    assert_eq!(memory.promotion, MemoryPromotion::HighConfidence);
    assert_eq!(memory.auto_confidence, 0.95);
}

#[test]
fn memory_confidence_must_be_a_finite_probability() {
    for value in ["-0.01", "1.01"] {
        let error = parse(&format!(r#"{{"memory":{{"auto_confidence":{value}}}}}"#))
            .expect_err("confidence outside the probability range must fail");
        assert_eq!(issue_path(&error), "memory.auto_confidence");
    }
}

#[test]
fn memory_character_caps_must_be_positive() {
    let error = parse(r#"{"memory":{"global_char_limit":0}}"#)
        .expect_err("a zero character budget is not usable");
    assert_eq!(issue_path(&error), "memory.global_char_limit");
}

#[test]
fn concurrency_defaults_and_overrides_are_fully_resolved() {
    assert_eq!(
        Config::default().resolved_concurrency(),
        ResolvedConcurrencyConfig {
            tool_calls: 8,
            delegations: 8,
            mcp_connections: 8,
            lsp_requests: 4,
        }
    );
    let config = parse(
        r#"{"concurrency":{"tool_calls":1,"delegations":2,"mcp_connections":16,"lsp_requests":64}}"#,
    )
    .expect("bounded concurrency parses");
    assert_eq!(
        config.resolved_concurrency(),
        ResolvedConcurrencyConfig {
            tool_calls: 1,
            delegations: 2,
            mcp_connections: 16,
            lsp_requests: 64,
        }
    );
}

#[test]
fn every_concurrency_limit_is_between_one_and_sixty_four() {
    for field in [
        "tool_calls",
        "delegations",
        "mcp_connections",
        "lsp_requests",
    ] {
        for bad in [json!(0), json!(65), json!(-1)] {
            let mut value = json!({"concurrency": {}});
            value["concurrency"][field] = bad;
            let error = parse_value(value).expect_err("out-of-range concurrency must be rejected");
            assert_eq!(issue_path(&error), format!("concurrency.{field}"));
            assert!(
                issue_detail(&error).contains("between 1 and 64")
                    || issue_detail(&error).contains("invalid value"),
                "{}",
                issue_detail(&error)
            );
        }
    }
}

#[test]
fn product_agents_default_off_and_validate_native_permission_modes() {
    let config = parse(
        r#"{
          "productAgent": {
            "codex-work": {"kind":"codex"},
            "claude-review": {
              "kind":"claude-code",
              "enabled":true,
              "toolName":"subagent_review",
              "permissionMode":"dontAsk"
            }
          }
        }"#,
    )
    .expect("product agents parse");
    let agents = config.product_agent.expect("product agents");
    let codex = agents.get("codex-work").expect("codex");
    assert_eq!(codex.kind, ProductAgentKind::Codex);
    assert!(!codex.is_enabled());
    assert_eq!(
        codex.resolved_permission_mode(),
        ProductAgentPermissionMode::Never
    );
    codex.validate("codex-work").expect("codex config");

    let claude = agents.get("claude-review").expect("claude");
    assert!(claude.is_enabled());
    assert_eq!(claude.resolved_tool_name(), "subagent_review");
    claude.validate("claude-review").expect("claude config");
}

#[test]
fn product_agent_permission_modes_do_not_cross_products() {
    let config = parse(r#"{"productAgent":{"bad":{"kind":"codex","permissionMode":"dontAsk"}}}"#)
        .expect("the shared schema parses before product-specific validation");
    let bad = config
        .product_agent
        .expect("product agents")
        .get("bad")
        .expect("bad")
        .clone();
    let error = bad.validate("bad").expect_err("dontAsk is Claude-only");
    assert!(error.contains("not valid"), "{error}");
}

// ---------------------------------------------------------------------------
// Acceptance: the agent unknown-key sweep.
// ---------------------------------------------------------------------------

fn agent(value: Value) -> AgentConfig {
    serde_json::from_value(value).expect("agent deserializes")
}

#[test]
fn an_unknown_agent_key_lands_in_options() {
    let agent = agent(json!({ "reasoningEffort": "high" }));
    let options = agent.options.as_ref().expect("options materialized");
    assert_eq!(options["reasoningEffort"], json!("high"));
    // The source key remains available to config merging and diagnostics.
    assert_eq!(agent.extra["reasoningEffort"], json!("high"));
}

#[test]
fn a_nested_unknown_agent_key_lands_in_options_intact() {
    let thinking = json!({ "type": "enabled", "budgetTokens": 32000 });
    let agent = agent(json!({
        "model": "anthropic/claude-sonnet-4-5",
        "reasoningEffort": "high",
        "thinking": thinking,
    }));
    let options = agent.options.as_ref().expect("options materialized");
    assert_eq!(options["reasoningEffort"], json!("high"));
    assert_eq!(options["thinking"], thinking);
    assert_eq!(options["thinking"]["budgetTokens"], json!(32000));
    assert_eq!(agent.model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
}

#[test]
fn the_sweep_merges_into_declared_options_without_dropping_them() {
    let agent = agent(json!({
        "options": { "reasoningEffort": "low", "declared": true },
        "reasoningEffort": "high",
    }));
    let options = agent.options.as_ref().expect("options present");
    assert_eq!(options["declared"], json!(true));
    // A top-level key wins over the same key inside `options`, matching
    // `config/agent.ts:63-66`, which spreads `agent.options` first and then
    // overwrites from the unknown keys.
    assert_eq!(options["reasoningEffort"], json!("high"));
}

#[test]
fn an_agent_without_unknown_keys_keeps_options_absent() {
    let agent = agent(json!({ "model": "anthropic/claude-sonnet-4-5" }));
    assert!(agent.options.is_none());
    assert!(agent.extra.is_empty());
}

#[test]
fn an_explicit_empty_options_object_survives() {
    let agent = agent(json!({ "options": {} }));
    assert_eq!(agent.options, Some(JsonMap::new()));
}

#[test]
fn sweep_exempt_keys_never_become_provider_options() {
    let agent = agent(json!({
        "name": "build",
        "reasoningEffort": "high",
    }));
    let options = agent.options.as_ref().expect("options materialized");
    for exempt in SWEEP_EXEMPT_KEYS {
        assert!(
            !options.contains_key(*exempt),
            "{exempt} must not be swept into options"
        );
        assert!(
            agent.extra.contains_key(*exempt),
            "{exempt} must remain available to the catalog"
        );
    }
    assert_eq!(options["reasoningEffort"], json!("high"));
}

#[test]
fn an_agent_with_swept_keys_loses_nothing_on_round_trip() {
    let before = json!({
        "agents": {
            "build": {
                "model": "anthropic/claude-sonnet-4-5",
                "reasoningEffort": "high",
                "thinking": { "type": "enabled", "budgetTokens": 32000 }
            }
        }
    });
    let config = parse_value(before.clone()).expect("deserializes");
    let after = serde_json::to_value(&config).expect("serializes");
    assert_contains(&after, &before, "$");
    let build = &after["agents"]["build"];
    assert_eq!(build["options"]["reasoningEffort"], json!("high"));
    assert_eq!(build["options"]["thinking"]["budgetTokens"], json!(32000));
}

#[test]
fn agent_named_fields_are_not_swept() {
    let agent = agent(json!({
        "model": "m", "variant": "v", "temperature": 0.5, "top_p": 0.9,
        "prompt": "p", "disable": false, "description": "d", "mode": "subagent",
        "hidden": true, "color": "primary", "steps": 10,
        "tools": ["read", "grep"], "delegates": ["researcher"],
        "requiredSkills": ["codegraph"],
        "permission": { "mode": "allow_all" },
    }));
    assert!(agent.options.is_none(), "no named key may reach options");
    assert!(agent.extra.is_empty());
    assert_eq!(agent.mode, Some(AgentMode::Subagent));
    assert_eq!(agent.color, Some(AgentColor::Theme(ThemeColor::Primary)));
    assert_eq!(agent.steps.map(|s| s.get()), Some(10));
    assert_eq!(
        agent.tools.as_deref(),
        Some(["read".to_owned(), "grep".to_owned()].as_slice())
    );
    assert_eq!(
        agent.delegates.as_deref(),
        Some(["researcher".to_owned()].as_slice())
    );
    assert_eq!(
        agent.required_skills.as_deref(),
        Some(["codegraph".to_owned()].as_slice())
    );
}

#[test]
fn agent_colour_takes_hex_and_theme_names_only() {
    assert_eq!(
        agent(json!({ "color": "#Ff5733" })).color,
        Some(AgentColor::Hex("#Ff5733".to_owned()))
    );
    assert_eq!(
        agent(json!({ "color": "error" })).color,
        Some(AgentColor::Theme(ThemeColor::Error))
    );
    for bad in ["banana", "#fff", "#12345g", "ff5733"] {
        let error = parse_value(json!({ "agents": { "a": { "color": bad } } }))
            .expect_err("invalid colour must be rejected");
        assert_eq!(issue_path(&error), "agents.a.color", "for {bad}");
    }
}

// ---------------------------------------------------------------------------
// Acceptance: deprecated keys are absent, so the rejection pass can act.
// ---------------------------------------------------------------------------

#[test]
fn deprecated_top_level_keys_are_not_accepted() {
    for key in ["mode", "layout", "autoshare"] {
        let error = parse_value(json!({ key: json!({}) })).expect_err("must be rejected");
        assert_eq!(issue_path(&error), key);
        assert_eq!(issue_detail(&error), "unrecognized key");
    }
}

#[test]
fn every_unrecognized_top_level_key_gets_its_own_issue() {
    let error = parse_value(json!({ "themes": "system", "keybind": {}, "model": "a/b" }))
        .expect_err("must be rejected");
    let ConfigError::Invalid { issues, .. } = &error else {
        panic!("expected Invalid, got {error:?}");
    };
    let mut paths: Vec<String> = issues.iter().map(|i| i.key_path.join(".")).collect();
    paths.sort();
    assert_eq!(paths, vec!["keybind".to_owned(), "themes".to_owned()]);
    for issue in issues {
        assert_eq!(issue.detail, "unrecognized key");
    }
}

// ---------------------------------------------------------------------------
// Acceptance: Zuno accepts no retired top-level spellings.
// ---------------------------------------------------------------------------

#[test]
fn retired_tui_keys_are_unknown_top_level_fields() {
    for key in ["theme", "keybinds", "tui"] {
        let error = parse_value(json!({ key: {} })).expect_err("retired key must be rejected");
        assert!(
            error.report().contains("unrecognized key"),
            "{}",
            error.report()
        );
    }
}

#[test]
fn unsupported_agent_fields_fail_inside_the_native_schema() {
    let error = parse_value(json!({ "agents": { "build": { "maxSteps": 4 } } }))
        .expect_err("unsupported field");
    assert!(error.report().contains("maxSteps"), "{}", error.report());
}

#[test]
fn an_agent_variant_requires_an_explicit_model() {
    for field in ["variant", "reasoning"] {
        let error = parse_value(json!({ "agents": { "build": { field: "high" } } }))
            .expect_err("a model owns its reasoning vocabulary");
        assert!(
            error.report().contains("require an explicit `model`"),
            "{}",
            error.report()
        );
    }
}

#[test]
fn agent_orchestration_fields_are_structured_and_validated() {
    let config = parse_value(json!({
        "agents": {
            "researcher": {
                "model": "myopenai/gpt-5.6-sol",
                "reasoning": "high",
                "tools": ["read", "grep", "web_search", "skill"],
                "requiredSkills": ["codegraph"]
            },
            "implementer": {
                "model": "myopenai/gpt-5.6-sol",
                "reasoning": "max",
                "tools": ["read", "edit", "shell"],
                "delegates": ["researcher"]
            }
        },
        "workflows": {
            "release-hardening": {
                "maxParallel": 4,
                "maxAgents": 12,
                "nodes": [
                    {"id":"scan","agent":"researcher"},
                    {"id":"review","agent":"researcher"},
                    {"id":"implement","agent":"implementer","dependsOn":["scan","review"]},
                    {"id":"verify","agent":"implementer","dependsOn":["implement"]}
                ]
            }
        }
    }))
    .expect("structured orchestration config");
    let agents = config.agent.expect("agents");
    assert_eq!(
        agents.get("researcher").and_then(|agent| agent.reasoning),
        Some(AgentReasoning::High)
    );
    assert_eq!(
        agents
            .get("researcher")
            .and_then(|agent| agent.required_skills.as_deref()),
        Some(["codegraph".to_owned()].as_slice())
    );
    let workflow = config
        .workflows
        .expect("workflows")
        .get("release-hardening")
        .expect("workflow")
        .clone();
    workflow
        .validate("release-hardening", &agents)
        .expect("valid workflow");

    for bad in [json!([]), json!(["read", "read"]), json!([""])] {
        let error = parse_value(json!({"agents":{"worker":{"tools":bad}}}))
            .expect_err("invalid tool allowlist");
        assert!(error.report().contains("tools"), "{}", error.report());
    }

    for bad in [json!([]), json!(["codegraph", "codegraph"]), json!([""])] {
        let error = parse_value(json!({"agents":{"worker":{"requiredSkills":bad}}}))
            .expect_err("invalid required Skill list");
        assert!(
            error.report().contains("requiredSkills"),
            "{}",
            error.report()
        );
    }
}

// ---------------------------------------------------------------------------
// Acceptance: the unions.
// ---------------------------------------------------------------------------

#[test]
fn references_is_a_three_way_union() {
    let config = parse(&fixture("all-keys.json")).expect("fixture deserializes");
    let references = config.references.as_ref().expect("references present");
    assert!(matches!(
        references.get("shorthand"),
        Some(ReferenceEntry::Shorthand(_))
    ));
    let Some(ReferenceEntry::Git(git)) = references.get("gitform") else {
        panic!(
            "gitform is not the git arm: {:?}",
            references.get("gitform")
        );
    };
    assert_eq!(git.branch.as_deref(), Some("dev"));
    let Some(ReferenceEntry::Local(local)) = references.get("localform") else {
        panic!("localform is not the local arm");
    };
    assert_eq!(local.path, "../sibling-repo");
    assert_eq!(local.hidden, Some(true));
}

#[test]
fn retired_plugin_keys_fail_at_the_config_boundary() {
    for key in ["plugin", "plugin_runtime"] {
        let error =
            parse_value(json!({ key: [] })).expect_err("retired plugin key must be rejected");
        assert_eq!(issue_path(&error), key);
    }
}

#[test]
fn formatter_is_a_bool_or_a_map() {
    for flag in [true, false] {
        let config = parse_value(json!({ "formatter": flag })).expect("deserializes");
        assert_eq!(config.formatter, Some(FormatterConfig::Enabled(flag)));
    }
    let config = parse_value(json!({
        "formatter": { "prettier": { "command": ["prettier", "--write", "$FILE"] } }
    }))
    .expect("deserializes");
    let Some(FormatterConfig::Formatters(formatters)) = &config.formatter else {
        panic!("expected the map arm");
    };
    assert_eq!(
        formatters.get("prettier").expect("prettier").command,
        Some(vec![
            "prettier".to_owned(),
            "--write".to_owned(),
            "$FILE".to_owned()
        ])
    );
}

#[test]
fn lsp_is_a_bool_or_a_map() {
    for flag in [true, false] {
        let config = parse_value(json!({ "lsp": flag })).expect("deserializes");
        assert_eq!(config.lsp, Some(LspConfig::Enabled(flag)));
    }
    let config = parse_value(json!({ "lsp": { "typescript": { "disabled": true } } }))
        .expect("deserializes");
    let Some(LspConfig::Servers(servers)) = &config.lsp else {
        panic!("expected the map arm");
    };
    assert!(servers.get("typescript").expect("typescript").is_disabled());
}

#[test]
fn a_custom_lsp_server_must_declare_extensions() {
    assert!(!BUILTIN_SERVER_IDS.contains(&"my-lsp"));
    let error = parse_value(json!({ "lsp": { "my-lsp": { "command": ["my-lsp"] } } }))
        .expect_err("must be rejected");
    assert_eq!(issue_path(&error), "lsp.my-lsp");
    assert!(
        issue_detail(&error).contains("extensions"),
        "message must name the missing key: {}",
        issue_detail(&error)
    );
    parse_value(json!({ "lsp": { "gopls": { "command": ["gopls"] } } }))
        .expect("a built-in server infers its extensions");
    parse_value(json!({ "lsp": { "my-lsp": { "disabled": true } } }))
        .expect("a disabled custom server needs nothing");
}

#[test]
fn an_lsp_server_needs_a_command_unless_disabled() {
    let error = parse_value(json!({ "lsp": { "gopls": { "disabled": false } } }))
        .expect_err("must be rejected");
    assert!(
        issue_detail(&error).contains("command"),
        "message must name the missing key: {}",
        issue_detail(&error)
    );
}

#[test]
fn mcp_entries_cover_local_remote_and_the_enabled_only_toggle() {
    let config = parse(&fixture("all-keys.json")).expect("fixture deserializes");
    let servers = config.mcp.as_ref().expect("mcp present");
    let Some(McpServerConfig::Local(local)) = servers.get("local-one") else {
        panic!("local-one is not the local arm");
    };
    assert_eq!(local.command, vec!["uvx".to_owned(), "mcpdoc".to_owned()]);
    let Some(McpServerConfig::Remote(remote)) = servers.get("remote-one") else {
        panic!("remote-one is not the remote arm");
    };
    assert!(matches!(remote.oauth, Some(McpOauth::Config(_))));
    let Some(McpServerConfig::Remote(no_oauth)) = servers.get("remote-no-oauth") else {
        panic!("remote-no-oauth is not the remote arm");
    };
    assert_eq!(no_oauth.oauth, Some(McpOauth::Disabled(False)));
    let Some(McpServerConfig::Toggle(toggle)) = servers.get("inherited-off") else {
        panic!("inherited-off is not the toggle arm");
    };
    assert!(!toggle.enabled);
}

#[test]
fn a_broken_mcp_entry_reports_the_arm_its_type_names() {
    let error =
        parse_value(json!({ "mcp": { "x": { "type": "remote" } } })).expect_err("url is required");
    assert_eq!(issue_path(&error), "mcp.x");
    assert!(
        issue_detail(&error).contains("url"),
        "message must name the missing key: {}",
        issue_detail(&error)
    );
}

#[test]
fn autoupdate_is_a_bool_or_notify() {
    assert_eq!(
        parse_value(json!({ "autoupdate": true }))
            .expect("bool")
            .autoupdate,
        Some(Autoupdate::Enabled(true))
    );
    assert_eq!(
        parse_value(json!({ "autoupdate": "notify" }))
            .expect("notify")
            .autoupdate,
        Some(Autoupdate::Mode(AutoupdateMode::Notify))
    );
    let error = parse_value(json!({ "autoupdate": "later" })).expect_err("must be rejected");
    assert_eq!(issue_path(&error), "autoupdate");
}

#[test]
fn a_provider_timeout_is_millis_or_the_literal_false() {
    let config = parse_value(json!({
        "provider": { "p": { "options": { "timeout": 1000, "headerTimeout": false } } }
    }))
    .expect("deserializes");
    let options = config
        .provider
        .as_ref()
        .and_then(|p| p.get("p"))
        .and_then(|p| p.options.as_ref())
        .expect("options present");
    assert_eq!(
        options.timeout,
        Some(Timeout::Millis(1000.try_into().unwrap()))
    );
    assert_eq!(options.header_timeout, Some(Timeout::Disabled(False)));
    let error = parse_value(json!({ "provider": { "p": { "options": { "timeout": true } } } }))
        .expect_err("true is not a timeout");
    assert_eq!(issue_path(&error), "provider.p.options.timeout");
}

#[test]
fn unknown_provider_options_are_kept_for_the_sdk() {
    let config = parse_value(json!({
        "provider": { "p": { "options": { "apiKey": "k", "customKnob": { "deep": 1 } } } }
    }))
    .expect("deserializes");
    let options = config
        .provider
        .as_ref()
        .and_then(|p| p.get("p"))
        .and_then(|p| p.options.as_ref())
        .expect("options present");
    assert_eq!(options.api_key.as_deref(), Some("k"));
    assert_eq!(options.extra["customKnob"], json!({ "deep": 1 }));
}

#[test]
fn responses_text_blocks_is_typed_and_rejects_unknown_modes() {
    let config = parse_value(json!({
        "provider": { "p": { "options": { "responsesTextBlocks": "single" } } }
    }))
    .expect("single-text Responses projection is valid");
    let options = config
        .provider
        .as_ref()
        .and_then(|providers| providers.get("p"))
        .and_then(|provider| provider.options.as_ref())
        .expect("options present");
    assert_eq!(
        options.responses_text_blocks,
        Some(ResponsesTextBlocks::Single)
    );
    assert!(
        !options.extra.contains_key("responsesTextBlocks"),
        "the native request-shape option must not be an unvalidated SDK extra"
    );

    let error = parse_value(json!({
        "provider": { "p": { "options": { "responsesTextBlocks": "merge-sometimes" } } }
    }))
    .expect_err("unknown projection modes must fail validation");
    assert_eq!(issue_path(&error), "provider.p.options.responsesTextBlocks");
}

#[test]
fn provider_and_model_headers_are_typed_configuration() {
    let config = parse_value(json!({
        "provider": {
            "gateway": {
                "headers": {"X-Provider": "provider"},
                "models": {
                    "primary": {"headers": {"X-Model": "model"}}
                }
            }
        }
    }))
    .expect("provider and model headers deserialize");
    let provider = config
        .provider
        .as_ref()
        .and_then(|providers| providers.get("gateway"))
        .expect("gateway provider");
    assert_eq!(
        provider.headers.as_ref().expect("provider headers")["X-Provider"],
        "provider"
    );
    let model = provider
        .models
        .as_ref()
        .and_then(|models| models.get("primary"))
        .expect("primary model");
    assert_eq!(
        model.headers.as_ref().expect("model headers")["X-Model"],
        "model"
    );
}

// ---------------------------------------------------------------------------
// Acceptance: permission order and shape.
// ---------------------------------------------------------------------------

#[test]
fn permission_keeps_the_authors_key_order() {
    // Rule precedence follows the order authored inside `permission.rules`.
    let config = parse(
        r#"{"permission":{"rules":{"zebra":"deny","shell":"ask","alpha":"allow","read":"allow"}}}"#,
    )
    .expect("deserializes");
    let rules = &config.permission.as_ref().expect("permission").rules;
    let keys: Vec<&str> = rules.iter().map(|(key, _)| key).collect();
    assert_eq!(keys, vec!["zebra", "shell", "alpha", "read"]);
}

#[test]
fn parsing_through_a_json_value_forfeits_key_order() {
    // Pinned, not aspirational: `serde_json::Map` is a `BTreeMap` here, so a
    // document that has been through `Value` is already sorted and no downstream
    // type can recover the author's order. Anything that needs permission
    // precedence must parse from the text.
    let text = r#"{"permission":{"rules":{"zebra":"deny","alpha":"allow"}}}"#;
    let value: Value = serde_json::from_str(text).expect("valid JSON");
    let from_value = parse_value(value).expect("deserializes");
    let value_rules = &from_value.permission.as_ref().expect("permission").rules;
    assert_eq!(
        value_rules.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        vec!["alpha", "zebra"]
    );

    let from_text = parse(text).expect("deserializes");
    let text_rules = &from_text.permission.as_ref().expect("permission").rules;
    assert_eq!(
        text_rules.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        vec!["zebra", "alpha"]
    );
}

#[test]
fn legacy_permission_shorthands_are_rejected() {
    for value in [
        json!({ "permission": "deny" }),
        json!({ "permission": { "read": "allow" } }),
    ] {
        parse_value(value).expect_err("only permission.mode/rules is accepted");
    }
}

#[test]
fn action_only_permissions_reject_per_pattern_rules() {
    let error = parse_value(json!({
        "permission": { "rules": { "webfetch": { "*": "allow" } } }
    }))
    .expect_err("webfetch takes a bare action");
    assert_eq!(issue_path(&error), "permission.rules.webfetch");
    assert!(issue_detail(&error).contains("webfetch"));
    parse_value(json!({
        "permission": { "rules": { "shell": { "git push": "ask" } } }
    }))
    .expect("shell does take per-pattern rules");
}

#[test]
fn an_unknown_permission_key_is_kept() {
    let config = parse_value(json!({
        "permission": { "rules": { "custom_audit": "allow" } }
    }))
    .expect("deserializes");
    let rules = &config.permission.as_ref().expect("permission").rules;
    assert_eq!(
        rules.get("custom_audit"),
        Some(&PermissionRule::Action(PermissionAction::Allow))
    );
}

// ---------------------------------------------------------------------------
// Acceptance: bounded integers.
// ---------------------------------------------------------------------------

#[test]
fn positive_int_fields_reject_zero_and_negatives() {
    for bad in [json!(0), json!(-1)] {
        let error =
            parse_value(json!({ "server": { "port": bad } })).expect_err("PositiveInt excludes it");
        assert_eq!(issue_path(&error), "server.port");
    }
    // NonNegativeInt does allow zero.
    assert_eq!(
        parse_value(json!({ "subagent_depth": 0 }))
            .expect("zero depth is legal")
            .subagent_depth,
        Some(0)
    );
    let error = parse_value(json!({ "subagent_depth": -1 })).expect_err("negative is not");
    assert_eq!(issue_path(&error), "subagent_depth");
}

#[test]
fn an_mcp_callback_port_stays_inside_the_port_range() {
    parse_value(json!({
        "mcp": { "x": { "type": "remote", "url": "u", "oauth": { "callbackPort": 65535 } } }
    }))
    .expect("65535 is a port");
    let error = parse_value(json!({
        "mcp": { "x": { "type": "remote", "url": "u", "oauth": { "callbackPort": 65536 } } }
    }))
    .expect_err("65536 is not");
    assert_eq!(issue_path(&error), "mcp.x.oauth.callbackPort");
}

// ---------------------------------------------------------------------------
// Acceptance: unknown-key policy per level.
// ---------------------------------------------------------------------------

#[test]
fn nested_unknown_keys_are_ignored_like_the_oracle() {
    // Effect Schema's default `onExcessProperty` is "ignore", so this is accepted
    // and the stray key is dropped rather than rejected.
    let config = parse_value(json!({ "server": { "port": 1234, "prot": 4321 } }))
        .expect("a nested typo is not fatal");
    assert_eq!(
        config.server.as_ref().and_then(|s| s.port).map(|p| p.get()),
        Some(1234)
    );
    let after = serde_json::to_value(&config).expect("serializes");
    assert_eq!(after["server"].as_object().expect("object").len(), 1);
}

// ---------------------------------------------------------------------------
// QA: the failure scenario, and the depth of the recovered key path.
// ---------------------------------------------------------------------------

#[test]
fn a_bad_server_port_names_the_key_path_instead_of_panicking() {
    let error = parse(r#"{"server": {"port": "not-a-number"}}"#)
        .expect_err("a string is not a port number");
    assert_eq!(issue_path(&error), "server.port");
    assert!(
        issue_detail(&error).contains("invalid type: string"),
        "detail carries the validator's own words: {}",
        issue_detail(&error)
    );
    let ConfigError::Invalid { path, .. } = &error else {
        panic!("expected Invalid");
    };
    assert_eq!(path, Path::new("opencode.json"));
    assert_eq!(
        error.to_string(),
        "config file opencode.json failed validation (1 issue(s))"
    );
}

#[test]
fn the_key_path_reaches_through_maps_and_arrays() {
    let error = parse_value(json!({
        "provider": {
            "anthropic": { "models": { "m": { "limit": { "context": "big", "output": 64000 } } } }
        }
    }))
    .expect_err("a string is not a limit");
    assert_eq!(
        issue_path(&error),
        "provider.anthropic.models.m.limit.context"
    );

    let error = parse_value(json!({
        "experimental": { "policies": [{ "action": "provider.use", "effect": "maybe", "resource": "*" }] }
    }))
    .expect_err("maybe is not an effect");
    // The probe stops at the enclosing object: `effect` is required, and no probe
    // value satisfies a closed set of string variants, so its removal or
    // substitution cannot be shown to fix the document. The detail carries the rest.
    assert_eq!(issue_path(&error), "experimental.policies.0");
    assert!(
        issue_detail(&error).contains("unknown variant `maybe`"),
        "detail must still identify the offending value: {}",
        issue_detail(&error)
    );
}

#[test]
fn malformed_json_is_reported_as_json_not_validation() {
    let error = parse("{\n  \"model\": ,\n}").expect_err("not valid JSON");
    let ConfigError::Json { source, .. } = &error else {
        panic!("expected Json, got {error:?}");
    };
    assert_eq!(source.line(), 2);
}

#[test]
fn goal_retry_settings_preserve_every_backoff_tunable() {
    let config = parse_value(json!({
        "goal": {
            "retry": {
                "initial_delay_ms": 2_000,
                "max_delay_ms": 300_000,
                "jitter_percent": 20,
                "poll_interval_ms": 250
            }
        }
    }))
    .expect("goal retry config parses");
    let retry = config
        .goal
        .expect("goal config")
        .retry
        .expect("retry config");

    assert_eq!(retry.initial_delay_ms.map(NonZeroU64::get), Some(2_000));
    assert_eq!(retry.max_delay_ms.map(NonZeroU64::get), Some(300_000));
    assert_eq!(retry.jitter_percent, Some(20));
    assert_eq!(retry.poll_interval_ms.map(NonZeroU64::get), Some(250));
}

// ---------------------------------------------------------------------------
// QA: the real corpora.
// ---------------------------------------------------------------------------

#[test]
fn the_curated_config_examples_all_deserialize() {
    let dir = PathBuf::from(FIXTURES).join("docs");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("docs fixtures exist")
        .map(|entry| entry.expect("readable entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    names.sort();
    assert_eq!(
        names.len(),
        32,
        "the curated config example corpus must not silently shrink"
    );
    for path in &names {
        let text = std::fs::read_to_string(path).expect("readable");
        let before: Value = serde_json::from_str(&text).expect("valid JSON");
        let config = Config::from_json_str(path, &text)
            .unwrap_or_else(|e| panic!("{} failed: {e:?}", path.display()));
        let after = serde_json::to_value(&config).expect("serializes");
        assert_contains(&after, &before, path.to_str().expect("utf-8 path"));
    }
}

#[test]
fn the_real_user_config_deserializes() {
    let text = fixture("user-config.json");
    let strict = crate::discovery::strip_jsonc(&text);
    let before: Value = serde_json::from_str(&strict).expect("valid JSONC");
    let config = parse(&strict).expect("the user's own config must load");
    let after = serde_json::to_value(&config).expect("serializes");

    assert_contains(&after, &before, "user-config.json");
    assert!(config.mcp.as_ref().expect("mcp present").len() >= 8);
    // Canonical permission rules survive the checked user fixture.
    let permission = &config.permission.as_ref().expect("permission").rules;
    assert!(permission.get("todo_get").is_some());
}

#[test]
fn checked_native_provider_starter_deserializes() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/config/zuno.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let config = parse(&text).expect("the checked native provider starter must load");
    let value = serde_json::to_value(config).expect("starter serializes");

    assert_eq!(value["model"], "myopenai/primary-model");
    assert_eq!(value["small_model"], "myopenai/fast-model");
    assert_eq!(value["provider"]["myopenai"]["transport"], "openai");
    assert!(
        value["provider"]["myopenai"].get("npm").is_none(),
        "Zuno provider config must not grow a package-manager selector"
    );
}

#[test]
fn checked_multi_provider_starter_deserializes() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/config/zuno-multi-provider.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let config = parse(&text).expect("the checked multi-provider starter must load");
    let value = serde_json::to_value(config).expect("starter serializes");

    assert_eq!(value["preset"], "myopenai");
    assert_eq!(value["provider"]["kiro-local"]["transport"], "openai");
    assert_eq!(value["provider"]["myopenai"]["transport"], "openai");
    assert_eq!(
        value["presets"]["hybrid"]["agents"]["orchestrator"]["model"],
        "myopenai/us.anthropic.claude-fable-5"
    );
    assert_eq!(
        value["presets"]["hybrid"]["agents"]["build"]["model"],
        "kiro-local/gpt-5.6-sol"
    );

    for (directory, expected) in [
        ("hybrid", "hybrid"),
        ("kiro", "kiro-local"),
        ("myopenai", "myopenai"),
    ] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../examples/config/profiles/{directory}/zuno.json"
        ));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let overlay =
            parse(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        assert_eq!(overlay.preset.as_deref(), Some(expected));
    }
}

// ---------------------------------------------------------------------------
// The order-preserving map itself.
// ---------------------------------------------------------------------------

#[test]
fn the_ordered_map_preserves_order_and_resolves_duplicates_last_wins() {
    let map: ordered::OrderedMap<u8> =
        serde_json::from_str(r#"{"b": 1, "a": 2, "b": 3}"#).expect("deserializes");
    assert_eq!(map.keys().collect::<Vec<_>>(), vec!["b", "a"]);
    assert_eq!(map.get("b"), Some(&3));
    assert_eq!(map.len(), 2);
    assert!(!map.is_empty());
    assert_eq!(
        serde_json::to_string(&map).expect("serializes"),
        r#"{"b":3,"a":2}"#
    );
}

#[test]
fn the_false_literal_rejects_true() {
    assert_eq!(
        serde_json::from_value::<False>(json!(false)).expect("false is the literal"),
        False
    );
    assert!(serde_json::from_value::<False>(json!(true)).is_err());
    assert_eq!(
        serde_json::to_value(False).expect("serializes"),
        json!(false)
    );
}

#[test]
fn an_empty_config_round_trips_to_an_empty_object() {
    let config = parse("{}").expect("an empty config is valid");
    assert_eq!(config, Config::default());
    assert_eq!(
        serde_json::to_value(&config).expect("serializes"),
        json!({}),
        "absent keys must not become nulls"
    );
}
