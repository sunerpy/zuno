use oc_error::ToolError;
use oc_permission::{
    Authorization, PermissionAction, PermissionEngine, PermissionReply, PermissionRequest,
    ReplyKind, Rule,
};

fn rule(permission: &str, pattern: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action,
    }
}

fn request(id: &str, session_id: &str) -> PermissionRequest {
    PermissionRequest {
        id: id.to_owned(),
        session_id: session_id.to_owned(),
        permission: "bash".to_owned(),
        patterns: vec!["git push".to_owned()],
        metadata: serde_json::Map::new(),
        always: vec!["git *".to_owned()],
        tool: None,
    }
}

#[test]
fn deny_returns_a_typed_error_without_creating_a_pending() {
    let mut engine = PermissionEngine::new();
    let rules = [rule("bash", "*", PermissionAction::Deny)];

    let error = engine
        .authorize(request("per_deny", "ses_a"), &rules)
        .expect_err("deny must stop authorization");

    assert!(matches!(error, ToolError::Denied { ref tool } if tool == "bash"));
    assert!(engine.pending().is_empty());
}

#[test]
fn later_denied_pattern_prevents_an_earlier_ask_from_becoming_pending() {
    let mut engine = PermissionEngine::new();
    let mut input = request("per_deny_after_ask", "ses_a");
    input.patterns.push("rm -rf /tmp/build".to_owned());
    let rules = [
        rule("bash", "git *", PermissionAction::Ask),
        rule("bash", "rm *", PermissionAction::Deny),
    ];

    let error = engine
        .authorize(input, &rules)
        .expect_err("a deny on any pattern must stop authorization");

    assert!(matches!(error, ToolError::Denied { ref tool } if tool == "bash"));
    assert!(engine.pending().is_empty());
}

#[test]
fn all_allowed_patterns_authorize_without_creating_a_pending() {
    let mut engine = PermissionEngine::new();
    let rules = [rule("bash", "*", PermissionAction::Allow)];

    let outcome = engine
        .authorize(request("per_allow", "ses_a"), &rules)
        .expect("allow is not an error");

    assert_eq!(outcome, Authorization::Allowed);
    assert!(engine.pending().is_empty());
}

#[test]
fn any_ask_pattern_creates_one_pending_request() {
    let mut engine = PermissionEngine::new();
    let mut input = request("per_ask", "ses_a");
    input.patterns.push("cargo test".to_owned());
    let rules = [
        rule("bash", "git *", PermissionAction::Allow),
        rule("bash", "*", PermissionAction::Ask),
    ];

    let outcome = engine
        .authorize(input, &rules)
        .expect("ask is not a denial");

    assert_eq!(outcome, Authorization::Pending);
    assert_eq!(engine.pending()[0].id, "per_ask");
}

#[test]
fn reject_resolves_the_target_and_all_same_session_siblings() {
    let mut engine = PermissionEngine::new();
    let ask = [rule("*", "*", PermissionAction::Ask)];
    for pending in [
        request("per_target", "ses_a"),
        request("per_sibling", "ses_a"),
        request("per_other", "ses_b"),
    ] {
        engine
            .authorize(pending, &ask)
            .expect("ask creates a pending");
    }

    let outcome = engine
        .reply(PermissionReply {
            request_id: "per_target".to_owned(),
            reply: ReplyKind::Reject,
            message: Some("use a read-only command".to_owned()),
        })
        .expect("target pending exists");

    let resolved_ids: Vec<_> = outcome
        .resolved
        .iter()
        .map(|item| item.request.id.as_str())
        .collect();
    assert_eq!(resolved_ids, ["per_target", "per_sibling"]);
    assert_eq!(
        outcome.resolved[0].message.as_deref(),
        Some("use a read-only command")
    );
    assert_eq!(outcome.resolved[1].message, None);
    assert_eq!(engine.pending()[0].id, "per_other");
}

#[test]
fn once_resolves_only_the_target_without_installing_a_rule() {
    let mut engine = PermissionEngine::new();
    let ask = [rule("*", "*", PermissionAction::Ask)];
    for pending in [
        request("per_target", "ses_a"),
        request("per_sibling", "ses_a"),
    ] {
        engine
            .authorize(pending, &ask)
            .expect("ask creates a pending");
    }

    let outcome = engine
        .reply(PermissionReply {
            request_id: "per_target".to_owned(),
            reply: ReplyKind::Once,
            message: None,
        })
        .expect("target pending exists");

    assert_eq!(outcome.resolved.len(), 1);
    assert_eq!(outcome.resolved[0].reply, ReplyKind::Once);
    assert!(outcome.installed_rules.is_empty());
    assert!(engine.approved_rules().is_empty());
    assert_eq!(engine.pending()[0].id, "per_sibling");
}

#[test]
fn always_clears_exactly_covered_same_session_pendings() {
    let mut engine = PermissionEngine::new();
    let ask = [rule("*", "*", PermissionAction::Ask)];
    let target = request("per_target", "ses_a");
    let mut covered = request("per_covered", "ses_a");
    covered.patterns = vec!["git status".to_owned()];
    let mut partially_covered = request("per_partial", "ses_a");
    partially_covered.patterns = vec!["git status".to_owned(), "cargo test".to_owned()];
    let mut other_permission = request("per_edit", "ses_a");
    other_permission.permission = "edit".to_owned();
    other_permission.patterns = vec!["git status".to_owned()];
    let mut other_session = request("per_other_session", "ses_b");
    other_session.patterns = vec!["git status".to_owned()];
    for pending in [
        target,
        covered,
        partially_covered,
        other_permission,
        other_session,
    ] {
        engine
            .authorize(pending, &ask)
            .expect("ask creates a pending");
    }

    let outcome = engine
        .reply(PermissionReply {
            request_id: "per_target".to_owned(),
            reply: ReplyKind::Always,
            message: None,
        })
        .expect("target pending exists");

    let resolved_ids: Vec<_> = outcome
        .resolved
        .iter()
        .map(|item| item.request.id.as_str())
        .collect();
    let pending_ids: Vec<_> = engine
        .pending()
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    assert_eq!(resolved_ids, ["per_target", "per_covered"]);
    assert_eq!(
        pending_ids,
        ["per_partial", "per_edit", "per_other_session"]
    );
    assert_eq!(
        outcome.installed_rules,
        [rule("bash", "git *", PermissionAction::Allow)]
    );
    assert_eq!(engine.approved_rules(), outcome.installed_rules);
}

#[test]
fn always_rules_override_supplied_rules_for_subsequent_requests() {
    let mut engine = PermissionEngine::new();
    let ask = [rule("bash", "*", PermissionAction::Ask)];
    engine
        .authorize(request("per_target", "ses_a"), &ask)
        .expect("ask creates a pending");
    engine
        .reply(PermissionReply {
            request_id: "per_target".to_owned(),
            reply: ReplyKind::Always,
            message: None,
        })
        .expect("target pending exists");

    let outcome = engine
        .authorize(request("per_future", "ses_a"), &ask)
        .expect("runtime allow overrides the supplied ask");

    assert_eq!(outcome, Authorization::Allowed);
    assert!(engine.pending().is_empty());
}

#[test]
fn unknown_reply_target_leaves_state_unchanged() {
    let mut engine = PermissionEngine::new();
    let ask = [rule("*", "*", PermissionAction::Ask)];
    engine
        .authorize(request("per_existing", "ses_a"), &ask)
        .expect("ask creates a pending");

    let outcome = engine.reply(PermissionReply {
        request_id: "per_missing".to_owned(),
        reply: ReplyKind::Reject,
        message: None,
    });

    assert_eq!(outcome, None);
    assert_eq!(engine.pending()[0].id, "per_existing");
}
