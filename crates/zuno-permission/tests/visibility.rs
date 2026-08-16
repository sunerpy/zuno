//! Tool visibility: hidden tools, alias keys, and the agent/session merge.
//!
//! Oracle parity target: `packages/opencode/src/permission/index.ts:204-219`
//! plus the merge call sites at `session/tools.ts:82` and `tool/registry.ts:280`.

use proptest::prelude::*;
use zuno_config::schema::permission::PermissionConfig;
use zuno_error::ToolError;
use zuno_permission::visibility::{
    EDIT_TOOLS, READ_TOOLS, disabled_tools, is_tool_hidden, is_tool_visible, merge_agent_session,
    merge_rulesets, permission_key, retain_visible_tools, visible_tools,
};
use zuno_permission::{
    Authorization, PermissionAction, PermissionEngine, PermissionRequest, Rule, evaluate,
    rules_from_config,
};

/// Every builtin tool name the model can be offered, plus the MCP resource trio.
const TOOL_LIST: [&str; 15] = [
    "bash",
    "edit",
    "write",
    "apply_patch",
    "read",
    "grep",
    "glob",
    "list",
    "task",
    "webfetch",
    "todowrite",
    "skill",
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

fn rules(json: &str) -> Vec<Rule> {
    let config: PermissionConfig =
        serde_json::from_str(json).expect("permission fixture must parse");
    rules_from_config(&config)
}

fn rule(permission: &str, pattern: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action,
    }
}

fn resolved_tools(json: &str) -> Vec<&'static str> {
    visible_tools(TOOL_LIST, &rules(json), |tool| *tool)
}

fn request(permission: &str, patterns: &[&str]) -> PermissionRequest {
    PermissionRequest {
        id: format!("per_{permission}"),
        session_id: "ses_visibility".to_owned(),
        permission: permission.to_owned(),
        patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
        metadata: serde_json::Map::new(),
        always: Vec::new(),
        tool: None,
    }
}

// ---------------------------------------------------------------------------
// The acceptance criterion: a fully denied tool never reaches the model.
// ---------------------------------------------------------------------------

#[test]
fn visibility_a_denied_tool_is_absent_from_the_resolved_tool_list() {
    let visible = resolved_tools(r#"{"bash": "deny"}"#);

    assert!(
        !visible.contains(&"bash"),
        "bash must not be advertised: {visible:?}"
    );
    assert!(visible.contains(&"read"), "unrelated tools stay visible");
    assert_eq!(visible.len(), TOOL_LIST.len() - 1);
}

#[test]
fn visibility_denying_edit_removes_edit_write_and_apply_patch_together() {
    let visible = resolved_tools(r#"{"edit": "deny"}"#);

    for tool in EDIT_TOOLS {
        assert!(
            !visible.contains(&tool),
            "{tool} routes through the edit key and must be hidden: {visible:?}"
        );
    }
    assert!(visible.contains(&"read"), "read is unaffected by edit deny");
    assert_eq!(visible.len(), TOOL_LIST.len() - EDIT_TOOLS.len());
}

#[test]
fn visibility_denying_read_removes_all_three_mcp_resource_tools() {
    let visible = resolved_tools(r#"{"read": "deny"}"#);

    for tool in READ_TOOLS {
        assert!(
            !visible.contains(&tool),
            "{tool} routes through the read key and must be hidden: {visible:?}"
        );
    }
    assert!(
        !visible.contains(&"read"),
        "the read tool itself is hidden too"
    );
    assert!(visible.contains(&"edit"), "edit is unaffected by read deny");
}

#[test]
fn visibility_alias_keys_cover_exactly_the_oracle_groups() {
    assert_eq!(EDIT_TOOLS, ["edit", "write", "apply_patch"]);
    assert_eq!(
        READ_TOOLS,
        [
            "list_mcp_resources",
            "list_mcp_resource_templates",
            "read_mcp_resource",
        ]
    );
    for tool in EDIT_TOOLS {
        assert_eq!(permission_key(tool), "edit");
    }
    for tool in READ_TOOLS {
        assert_eq!(permission_key(tool), "read");
    }
    assert_eq!(
        permission_key("bash"),
        "bash",
        "unaliased tools own their key"
    );
    assert_eq!(
        permission_key("mcp_github_create_issue"),
        "mcp_github_create_issue"
    );
}

// ---------------------------------------------------------------------------
// Hiding and refusing are different code paths.
// ---------------------------------------------------------------------------

#[test]
fn visibility_a_narrower_deny_pattern_keeps_the_tool_visible_and_refuses_only_matches() {
    let ruleset = rules(r#"{"bash": {"rm *": "deny"}}"#);
    let visible = visible_tools(TOOL_LIST, &ruleset, |tool| *tool);

    assert!(
        visible.contains(&"bash"),
        "a pattern-scoped deny is not a hide: {visible:?}"
    );

    let mut engine = PermissionEngine::new();
    let error = engine
        .authorize(request("bash", &["rm -rf /tmp/build"]), &ruleset)
        .expect_err("a matching invocation is refused at call time");
    assert!(matches!(error, ToolError::Denied { ref tool } if tool == "bash"));

    let authorization = engine
        .authorize(request("bash", &["git status"]), &ruleset)
        .expect("a non-matching invocation is not refused");
    assert_eq!(authorization, Authorization::Pending);
}

#[test]
fn visibility_an_ask_rule_never_hides_a_tool() {
    let visible = resolved_tools(r#"{"bash": "ask", "edit": "ask"}"#);

    assert!(visible.contains(&"bash"));
    assert!(visible.contains(&"edit"));
    assert_eq!(visible.len(), TOOL_LIST.len());
}

#[test]
fn visibility_a_specific_allow_after_a_wildcard_deny_keeps_the_tool_visible() {
    let ruleset = rules(r#"{"bash": {"*": "deny", "echo *": "allow"}}"#);

    assert!(
        is_tool_visible("bash", &ruleset),
        "echo is still reachable, so bash must be advertised"
    );
    assert_eq!(
        evaluate("bash", "rm -rf /", &ruleset),
        PermissionAction::Deny,
        "the wildcard deny still refuses other commands"
    );
    assert_eq!(
        evaluate("bash", "echo hi", &ruleset),
        PermissionAction::Allow
    );
}

#[test]
fn visibility_a_wildcard_allow_after_a_narrow_deny_keeps_the_tool_visible() {
    let ruleset = rules(r#"{"bash": {"rm *": "deny", "*": "allow"}}"#);

    assert!(is_tool_visible("bash", &ruleset));
    assert_eq!(
        evaluate("bash", "rm -rf /", &ruleset),
        PermissionAction::Allow
    );
}

// ---------------------------------------------------------------------------
// Permission keys are themselves wildcard patterns.
// ---------------------------------------------------------------------------

#[test]
fn visibility_a_wildcard_key_deny_hides_every_tool() {
    let visible = resolved_tools(r#"{"*": "deny"}"#);

    assert!(
        visible.is_empty(),
        "nothing survives a blanket deny: {visible:?}"
    );
    assert_eq!(
        disabled_tools(TOOL_LIST, &rules(r#"{"*": "deny"}"#)).len(),
        TOOL_LIST.len()
    );
}

#[test]
fn visibility_a_bare_deny_action_hides_every_tool() {
    let visible = resolved_tools(r#""deny""#);

    assert!(
        visible.is_empty(),
        "a bare deny normalizes to the * key: {visible:?}"
    );
}

#[test]
fn visibility_a_later_specific_allow_survives_an_earlier_wildcard_key_deny() {
    let visible = resolved_tools(r#"{"*": "deny", "bash": "allow"}"#);

    assert_eq!(visible, vec!["bash"]);
}

#[test]
fn visibility_an_earlier_specific_allow_loses_to_a_later_wildcard_key_deny() {
    let visible = resolved_tools(r#"{"bash": "allow", "*": "deny"}"#);

    assert!(
        visible.is_empty(),
        "outer key order decides the winner: {visible:?}"
    );
}

#[test]
fn visibility_a_wildcard_key_deny_hides_aliases_through_their_permission_key() {
    let ruleset = rules(r#"{"*": "deny", "edit": "allow"}"#);
    let visible = visible_tools(TOOL_LIST, &ruleset, |tool| *tool);

    for tool in EDIT_TOOLS {
        assert!(
            visible.contains(&tool),
            "{tool} is rescued by the edit key: {visible:?}"
        );
    }
    for tool in READ_TOOLS {
        assert!(!visible.contains(&tool), "{tool} still falls under * deny");
    }
}

// ---------------------------------------------------------------------------
// The agent/session merge, and its precedence.
// ---------------------------------------------------------------------------

#[test]
fn visibility_session_rules_are_appended_after_agent_rules_and_therefore_win() {
    let agent = rules(r#"{"edit": "deny"}"#);
    let session = rules(r#"{"edit": "allow"}"#);

    let merged = merge_agent_session(&agent, &session);

    assert_eq!(
        merged,
        [
            rule("edit", "*", PermissionAction::Deny),
            rule("edit", "*", PermissionAction::Allow),
        ],
        "agent rules are the base layer, session rules the override"
    );
    for tool in EDIT_TOOLS {
        assert!(
            is_tool_visible(tool, &merged),
            "{tool} is re-enabled by the session ruleset"
        );
    }
}

#[test]
fn visibility_an_agent_deny_still_hides_when_the_session_says_nothing() {
    let agent = rules(r#"{"edit": "deny"}"#);

    let merged = merge_agent_session(&agent, &[]);

    for tool in EDIT_TOOLS {
        assert!(is_tool_hidden(tool, &merged), "{tool} stays hidden");
    }
}

#[test]
fn visibility_a_session_deny_hides_a_tool_the_agent_allowed() {
    let agent = rules(r#"{"bash": "allow"}"#);
    let session = rules(r#"{"bash": "deny"}"#);

    let merged = merge_agent_session(&agent, &session);

    assert!(is_tool_hidden("bash", &merged));
}

#[test]
fn visibility_merge_rulesets_concatenates_in_argument_order() {
    let first = [rule("bash", "*", PermissionAction::Allow)];
    let second = [rule("bash", "rm *", PermissionAction::Deny)];
    let third = [rule("edit", "*", PermissionAction::Ask)];

    let merged = merge_rulesets(&[&first, &second, &third]);

    assert_eq!(
        merged,
        [first[0].clone(), second[0].clone(), third[0].clone()]
    );
    assert_eq!(merge_rulesets(&[]), Vec::<Rule>::new());
}

// ---------------------------------------------------------------------------
// QA scenario: a plan-style agent exposes no write-capable tools.
// ---------------------------------------------------------------------------

#[test]
fn visibility_a_plan_style_agent_exposes_no_write_capable_tools() {
    let agent = rules(r#"{"edit": "deny", "bash": {"*": "deny", "git status": "allow"}}"#);

    let visible = visible_tools(TOOL_LIST, &agent, |tool| *tool);

    assert_eq!(
        visible,
        vec![
            "bash",
            "read",
            "grep",
            "glob",
            "list",
            "task",
            "webfetch",
            "todowrite",
            "skill",
            "list_mcp_resources",
            "list_mcp_resource_templates",
            "read_mcp_resource",
        ],
        "no write-capable tool may be advertised to a plan agent"
    );
    for tool in EDIT_TOOLS {
        assert!(!visible.contains(&tool), "{tool} must be hidden");
    }
    assert_eq!(
        evaluate("bash", "rm -rf /", &agent),
        PermissionAction::Deny,
        "bash stays visible because `git status` is reachable, and is still refused otherwise"
    );
}

// ---------------------------------------------------------------------------
// Shape of the filter Todos 38/44 call.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct ToolDef {
    id: String,
    description: String,
}

#[test]
fn visibility_visible_tools_filters_records_and_preserves_order() {
    let defs = vec![
        ToolDef {
            id: "bash".to_owned(),
            description: "run a command".to_owned(),
        },
        ToolDef {
            id: "read".to_owned(),
            description: "read a file".to_owned(),
        },
        ToolDef {
            id: "write".to_owned(),
            description: "write a file".to_owned(),
        },
    ];
    let ruleset = rules(r#"{"edit": "deny"}"#);

    let visible = visible_tools(defs, &ruleset, |def| def.id.as_str());

    assert_eq!(
        visible
            .iter()
            .map(|def| def.id.as_str())
            .collect::<Vec<_>>(),
        ["bash", "read"]
    );
}

#[test]
fn visibility_retain_visible_tools_drops_hidden_entries_in_place() {
    let mut ids: Vec<String> = TOOL_LIST.iter().map(|tool| (*tool).to_owned()).collect();

    retain_visible_tools(&mut ids, &rules(r#"{"edit": "deny"}"#), String::as_str);

    assert!(!ids.iter().any(|id| EDIT_TOOLS.contains(&id.as_str())));
    assert_eq!(ids.len(), TOOL_LIST.len() - EDIT_TOOLS.len());
}

#[test]
fn visibility_an_empty_ruleset_hides_nothing() {
    assert_eq!(resolved_tools("{}").len(), TOOL_LIST.len());
    assert!(disabled_tools(TOOL_LIST, &[]).is_empty());
}

#[test]
fn visibility_disabled_tools_reports_every_hidden_name() {
    let hidden = disabled_tools(TOOL_LIST, &rules(r#"{"edit": "deny", "bash": "deny"}"#));

    assert_eq!(
        hidden.iter().map(String::as_str).collect::<Vec<_>>(),
        ["apply_patch", "bash", "edit", "write"]
    );
}

// ---------------------------------------------------------------------------
// Consistency with the evaluator: a hidden tool can never be invoked.
// ---------------------------------------------------------------------------

fn action_strategy() -> impl Strategy<Value = PermissionAction> {
    prop_oneof![
        Just(PermissionAction::Ask),
        Just(PermissionAction::Allow),
        Just(PermissionAction::Deny),
    ]
}

proptest! {
    /// A hidden tool must be unreachable: no input may evaluate to anything but
    /// deny. The generated input deliberately excludes the glob metacharacters
    /// `*` and `?` because `wildcard_match` currently mismatches an input that
    /// *contains* `*` against the pattern `"*"` (see `.omo/notepads` Task 17
    /// issues: `wildcard_match("*.txt", "*")` is `false` in Rust and `true` in
    /// the oracle). That defect lives in Todo 16's matcher, not in visibility,
    /// and is reported rather than silently patched here.
    #[test]
    fn visibility_a_hidden_tool_is_denied_for_every_input(
        specs in prop::collection::vec((0usize..4, 0usize..3, action_strategy()), 0..24),
        input in "[a-z/ .-]{0,12}",
    ) {
        let ruleset: Vec<_> = specs
            .iter()
            .map(|(permission, pattern, action)| {
                let permission = ["bash", "b*", "*", "edit"][*permission];
                let pattern = ["*", "rm *", "git status"][*pattern];
                rule(permission, pattern, *action)
            })
            .collect();

        if is_tool_hidden("bash", &ruleset) {
            prop_assert_eq!(
                evaluate("bash", &input, &ruleset),
                PermissionAction::Deny,
                "a hidden tool must be unreachable for input {:?}",
                input
            );
        }
    }
}
