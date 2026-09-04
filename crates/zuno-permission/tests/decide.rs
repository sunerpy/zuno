//! A configured `deny` is terminal, so the refusal has to say which rule and why.

use zuno_permission::{
    Authorization, MatchReason, PermissionAction, PermissionEngine, PermissionRequest, Rule,
    decide, evaluate, rules_from_config,
};

fn rules_from_json(configuration: &str) -> Vec<Rule> {
    let config = serde_json::from_str(configuration).expect("fixture is valid permission config");
    rules_from_config(&config)
}

fn allow_all_but_rm_rf() -> Vec<Rule> {
    rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"allow","rm -rf*":"deny"}}}"#)
}

fn request(permission: &str, pattern: &str) -> PermissionRequest {
    PermissionRequest {
        id: "req".to_owned(),
        session_id: "session".to_owned(),
        permission: permission.to_owned(),
        patterns: vec![pattern.to_owned()],
        metadata: serde_json::Map::new(),
        always: Vec::new(),
        tool: None,
    }
}

#[test]
fn a_refusal_of_an_unresolvable_program_names_the_rule_and_the_reason() {
    // Under any shell deny a bare `$EDITOR`, `*.sh` or `$(date +%s)` is refused,
    // terminally, because the program can only be resolved by the shell and nothing
    // else about the line is knowable. That is the fail-closed direction and it stays;
    // what changes is that the refusal is no longer an unexplained `Denied { tool }`.
    let rules = allow_all_but_rm_rf();
    for command in [
        "$EDITOR",
        "*.sh",
        "$(date +%s)",
        "`git rev-parse HEAD`",
        "$HOME/bin/build",
        "\"unterminated -rf x",
    ] {
        let decision = decide("shell", command, &rules);
        assert_eq!(decision.action, PermissionAction::Deny, "`{command}`");
        let matched = decision.matched.as_ref().expect("a deny names its rule");
        assert_eq!(matched.rule.pattern, "rm -rf*");
        assert_eq!(
            matched.reason,
            MatchReason::UnresolvableProgram {
                program: command.to_owned()
            },
            "`{command}`"
        );
        let denial = decision
            .denial("shell", command)
            .expect("a deny decision renders a denial");
        let rendered = denial.to_string();
        assert!(
            rendered.contains("rm -rf*") && rendered.contains(command),
            "the account names the rule and the token: {rendered}"
        );
        assert!(
            rendered.contains("only the shell can resolve"),
            "the account says why: {rendered}"
        );
    }
    // The S3 inputs stay Deny: the fail-closed reading was not narrowed.
    for command in [
        "rm${IFS}-rf${IFS}/",
        "rm${IFS}-rf${IFS}/tmp/build",
        "$(echo rm -rf /tmp/build)",
        "`echo rm -rf /tmp/build`",
        "rm'${IFS}-rf${IFS}/tmp/build",
    ] {
        assert_eq!(evaluate("shell", command, &rules), PermissionAction::Deny);
    }
    // The way out the account points at: a quoted expansion is one word.
    assert_eq!(
        evaluate("shell", "\"$EDITOR\"", &rules),
        PermissionAction::Allow
    );
    // An expansion in argument position was never refused.
    for command in ["echo $(date)", "make -j$(nproc)", "./run.sh $(id -u)"] {
        assert_eq!(evaluate("shell", command, &rules), PermissionAction::Allow);
    }
}

#[test]
fn every_deny_reading_is_reported_as_what_it_is() {
    let rules = allow_all_but_rm_rf();

    let identity = decide("shell", "rm -rf /tmp/build", &rules);
    assert_eq!(identity.action, PermissionAction::Deny);
    assert_eq!(
        identity.matched.as_ref().map(|matched| &matched.reason),
        Some(&MatchReason::Identity)
    );

    let respelled = decide("shell", "/bin/rm -rf /tmp/build", &rules);
    assert_eq!(
        respelled.matched.as_ref().map(|matched| &matched.reason),
        Some(&MatchReason::DenySpelling("rm -rf /tmp/build".to_owned())),
        "the deny-side spelling that matched is the one reported"
    );

    let folded = decide("shell", "RM -rf /tmp/build", &rules);
    assert_eq!(
        folded.matched.as_ref().map(|matched| &matched.reason),
        Some(&MatchReason::Folded("rm -rf /tmp/build".to_owned())),
        "a case-folded match reports the folded spelling, so the user sees what matched"
    );

    let retried = decide("shell", "$PROG -rf /tmp/build", &rules);
    assert_eq!(
        retried.matched.as_ref().map(|matched| &matched.reason),
        Some(&MatchReason::UnresolvableProgramArguments {
            program: "$PROG".to_owned(),
            retried_as: "rm -rf /tmp/build".to_owned(),
        })
    );

    let absolute_deny = rules_from_json(
        r#"{"mode":"standard","rules":{"read":{"*":"allow","/etc/ssh/*":"deny"}}}"#,
    );
    let tail = decide("read", "ssh/id", &absolute_deny);
    assert_eq!(tail.action, PermissionAction::Deny);
    assert_eq!(
        tail.matched.as_ref().map(|matched| &matched.reason),
        Some(&MatchReason::RelativeTail("ssh/*".to_owned()))
    );

    let granted = decide("shell", "git status", &rules);
    assert_eq!(granted.action, PermissionAction::Allow);
    assert!(
        granted.denial("shell", "git status").is_none(),
        "only a deny renders a denial"
    );
    let unmatched = decide("shell", "anything", &[]);
    assert_eq!(unmatched.action, PermissionAction::Ask);
    assert!(unmatched.matched.is_none());
}

#[test]
fn the_engine_reports_the_configured_rule_that_refused_the_request() {
    let rules = allow_all_but_rm_rf();
    let mut engine = PermissionEngine::new();

    let denial = engine
        .authorize_explained(request("shell", "$EDITOR"), &rules)
        .expect_err("a configured deny is terminal");
    assert_eq!(denial.permission, "shell");
    assert_eq!(denial.resource, "$EDITOR");
    assert_eq!(denial.rule.pattern, "rm -rf*");
    assert_eq!(denial.rule.action, PermissionAction::Deny);
    assert!(matches!(
        denial.reason,
        MatchReason::UnresolvableProgram { ref program } if program == "$EDITOR"
    ));
    assert!(
        engine.pending().is_empty(),
        "a denied request is never inserted into pending state"
    );

    // The plain channel loses nothing it used to carry.
    let error = engine
        .authorize(request("shell", "$EDITOR"), &rules)
        .expect_err("the same request is still refused through `authorize`");
    assert!(matches!(error, zuno_error::ToolError::Denied { ref tool } if tool == "shell"));
    let converted: zuno_error::ToolError = denial.into();
    assert!(matches!(converted, zuno_error::ToolError::Denied { ref tool } if tool == "shell"));

    assert_eq!(
        engine
            .authorize_explained(request("shell", "git status"), &rules)
            .expect("an allowed request is not a denial"),
        Authorization::Allowed
    );
}
