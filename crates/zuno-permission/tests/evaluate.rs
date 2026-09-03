use proptest::prelude::*;
use zuno_permission::{PermissionAction, Rule, evaluate, rules_from_config};

fn rule(permission: &str, pattern: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: pattern.to_owned(),
        action,
    }
}

#[test]
fn later_ask_overrides_earlier_allow_for_the_same_command() {
    let rules = [
        rule("shell", "git *", PermissionAction::Allow),
        rule("shell", "*", PermissionAction::Ask),
    ];

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

#[test]
fn later_allow_overrides_earlier_ask_for_the_same_command() {
    let rules = [
        rule("shell", "*", PermissionAction::Ask),
        rule("shell", "git *", PermissionAction::Allow),
    ];

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Allow);
}

#[test]
fn no_matching_rule_defaults_to_ask() {
    let rules = [rule("edit", "*", PermissionAction::Allow)];

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

#[test]
fn permission_keys_and_patterns_both_support_wildcards() {
    let rules = [rule("sh?ll", "git *", PermissionAction::Allow)];

    let action = evaluate("shell", "git status", &rules);

    assert_eq!(action, PermissionAction::Allow);
}

#[test]
fn arbitrary_custom_permission_keys_are_evaluated() {
    let rules = [rule("deploy_production", "blue-*", PermissionAction::Deny)];

    let action = evaluate("deploy_production", "blue-api", &rules);

    assert_eq!(action, PermissionAction::Deny);
}

#[test]
fn config_conversion_preserves_outer_and_nested_source_order() {
    let config = serde_json::from_str(
        r#"{
            "mode": "standard",
            "rules": {
                "*": "deny",
                "shell": {
                    "git *": "allow",
                    "*": "ask"
                },
                "deploy_production": "allow"
            }
        }"#,
    )
    .expect("fixture is valid permission config");

    let rules = rules_from_config(&config);

    assert_eq!(
        rules,
        vec![
            rule("*", "*", PermissionAction::Deny),
            rule("shell", "git *", PermissionAction::Allow),
            rule("shell", "*", PermissionAction::Ask),
            rule("deploy_production", "*", PermissionAction::Allow),
        ]
    );
}

#[test]
fn outer_config_key_order_controls_overlapping_permission_keys() {
    let config = serde_json::from_str(r#"{"mode":"standard","rules":{"shell":"allow","*":"ask"}}"#)
        .expect("fixture is valid permission config");
    let rules = rules_from_config(&config);

    let action = evaluate("shell", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

// ---------------------------------------------------------------------------
// Documented configuration examples.
//
// The JSON below is the corrected form of the examples in
// `docs/guide/permissions.md` ("Per-tool rules") and `docs/guide/headless.md`
// ("Permission modes without a human"). Both documents used to be written for a
// first-match-wins evaluator, which this engine is not: `evaluate` takes the
// **last** matching rule, so the catch-all has to come first and the narrow rules
// that override it have to come last.
//
// Keep the JSON here identical to the JSON in those documents. If either example
// is ever reordered back to "narrow first, catch-all last", copying it into these
// tests makes them fail, which is the point: the guide and the engine must not
// drift apart again.
// ---------------------------------------------------------------------------

/// `docs/guide/permissions.md`, section "Per-tool rules".
const DOCUMENTED_INTERACTIVE_CONFIG: &str = r#"{
  "mode": "standard",
  "rules": {
    "read": "allow",
    "edit": "ask",
    "shell": {
      "*": "ask",
      "git *": "allow",
      "git push*": "deny",
      "rm -rf*": "deny"
    }
  }
}"#;

/// `docs/guide/headless.md`, section "Permission modes without a human".
const DOCUMENTED_HEADLESS_CONFIG: &str = r#"{
  "mode": "standard",
  "rules": {
    "read": "allow",
    "glob": "allow",
    "grep": "allow",
    "edit": "deny",
    "shell": {
      "*": "deny",
      "cargo test*": "allow",
      "git push*": "deny"
    }
  }
}"#;

fn rules_from_json(configuration: &str) -> Vec<Rule> {
    let config = serde_json::from_str(configuration).expect("documented example is valid config");
    rules_from_config(&config)
}

#[test]
fn the_documented_interactive_example_denies_what_it_says_it_denies() {
    let rules = rules_from_json(DOCUMENTED_INTERACTIVE_CONFIG);

    assert_eq!(
        evaluate("shell", "rm -rf /", &rules),
        PermissionAction::Deny,
        "the guide presents this example as the one that stops `rm -rf /`"
    );
    assert_eq!(
        evaluate("shell", "git push origin main", &rules),
        PermissionAction::Deny,
        "`git push*` is written after `git *` so that a push is denied"
    );
    assert_eq!(
        evaluate("shell", "git status", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "cargo build", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate("read", "src/main.rs", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("edit", "src/main.rs", &rules),
        PermissionAction::Ask
    );
}

#[test]
fn the_documented_headless_example_allows_the_command_it_exists_to_allow() {
    let rules = rules_from_json(DOCUMENTED_HEADLESS_CONFIG);

    assert_eq!(
        evaluate("shell", "cargo test", &rules),
        PermissionAction::Allow,
        "the headless example exists to let CI run the test suite unattended"
    );
    assert_eq!(
        evaluate("shell", "cargo test -p zuno-permission", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "git push origin main", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "curl example.invalid", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("edit", "src/main.rs", &rules),
        PermissionAction::Deny
    );
    assert_eq!(evaluate("grep", "TODO", &rules), PermissionAction::Allow);
}

#[test]
fn first_match_wins_ordering_of_the_same_examples_loses_both_guarantees() {
    // Exactly the documented rules with the catch-all moved last, which is how both
    // guides used to print them. This is not a second opinion about ordering; it is
    // the evidence that the order carries the security property, so a guide that
    // prints the reverse order is printing a configuration that does not work.
    let interactive = rules_from_json(
        r#"{
          "mode": "standard",
          "rules": {
            "shell": {
              "git push*": "deny",
              "git *": "allow",
              "rm -rf*": "deny",
              "*": "ask"
            }
          }
        }"#,
    );
    assert_eq!(
        evaluate("shell", "rm -rf /", &interactive),
        PermissionAction::Ask,
        "the trailing catch-all swallows every narrow rule before it"
    );
    assert_eq!(
        evaluate("shell", "git push origin main", &interactive),
        PermissionAction::Ask
    );

    let headless = rules_from_json(
        r#"{
          "mode": "standard",
          "rules": {
            "shell": {
              "git push*": "deny",
              "cargo test*": "allow",
              "*": "deny"
            }
          }
        }"#,
    );
    assert_eq!(
        evaluate("shell", "cargo test", &headless),
        PermissionAction::Deny,
        "a trailing catch-all deny blocks the command the example allows"
    );
}

#[test]
fn a_wildcard_deny_covers_a_command_that_contains_a_star() {
    // The matcher used to compare a literal `*` in the command against the `*` in the
    // pattern and consume it, so this exact configuration did not stop `rm *.txt`.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm *":"deny"}}}"#);

    assert_eq!(
        evaluate("shell", "rm *.txt", &rules),
        PermissionAction::Deny
    );
    assert_eq!(evaluate("shell", "rm *", &rules), PermissionAction::Deny);
    assert_eq!(
        evaluate("shell", "rm -rf x", &rules),
        PermissionAction::Deny
    );
    assert_eq!(evaluate("shell", "rmdir x", &rules), PermissionAction::Ask);
}

// ---------------------------------------------------------------------------
// Shell rules match the command, not one spelling of it.
// ---------------------------------------------------------------------------

fn deny_rm_rf() -> Vec<Rule> {
    rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm -rf*":"deny"}}}"#)
}

#[test]
fn a_shell_deny_survives_extra_whitespace() {
    assert_eq!(
        evaluate("shell", "rm  -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "rm\t-rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_a_quoted_program() {
    assert_eq!(
        evaluate("shell", "'rm' -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "\"rm\" -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_an_escaped_program() {
    assert_eq!(
        evaluate("shell", "\\rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_an_absolute_program_path() {
    assert_eq!(
        evaluate("shell", "/bin/rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "/usr/bin/rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "./rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn a_shell_deny_survives_the_command_builtin() {
    assert_eq!(
        evaluate("shell", "command rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "command /bin/rm -rf /tmp/build", &deny_rm_rf()),
        PermissionAction::Deny
    );
}

#[test]
fn the_raw_spelling_keeps_matching_the_rules_written_for_it() {
    // Normalization only ever adds spellings. A rule written against the exact text
    // a user sees in their terminal must keep working.
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"shell":{"*":"ask","/bin/rm *":"deny","'quoted' *":"deny"}}}"#,
    );

    assert_eq!(
        evaluate("shell", "/bin/rm -rf /tmp/build", &rules),
        PermissionAction::Deny
    );
    assert_eq!(
        evaluate("shell", "'quoted' argument", &rules),
        PermissionAction::Deny
    );
}

#[test]
fn an_allow_rule_never_inherits_the_identity_of_another_program() {
    // The relaxations that drop which executable was named widen a deny only. An
    // allow for `rm` must not authorize a different `rm` found somewhere else.
    let rules =
        rules_from_json(r#"{"mode":"standard","rules":{"shell":{"*":"ask","rm *":"allow"}}}"#);

    assert_eq!(
        evaluate("shell", "rm  /tmp/build", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "'rm' /tmp/build", &rules),
        PermissionAction::Allow
    );
    assert_eq!(
        evaluate("shell", "/tmp/attacker/rm /tmp/build", &rules),
        PermissionAction::Ask,
        "an allow rule must not cover an unrelated executable that shares a file name"
    );
}

// ---------------------------------------------------------------------------
// Home expansion.
// ---------------------------------------------------------------------------

#[test]
fn home_expansion_requires_a_path_boundary() {
    let home = dirs::home_dir().expect("test host has a home directory");
    let home = home.to_string_lossy().into_owned();
    let rules = rules_from_json(
        r#"{"mode":"standard","rules":{"read":{"$HOME/x":"deny","$HOMEBREW/*":"deny","$HOME":"deny","~/y":"deny"}}}"#,
    );

    let patterns: Vec<&str> = rules.iter().map(|rule| rule.pattern.as_str()).collect();
    assert_eq!(
        patterns,
        vec![
            format!("{home}/x").as_str(),
            "$HOMEBREW/*",
            home.as_str(),
            format!("{home}/y").as_str(),
        ],
        "`$HOME` expands only when the next character starts a new path segment"
    );
}

fn action_strategy() -> impl Strategy<Value = PermissionAction> {
    prop_oneof![
        Just(PermissionAction::Ask),
        Just(PermissionAction::Allow),
        Just(PermissionAction::Deny),
    ]
}

proptest! {
    #[test]
    fn random_rule_orders_use_the_last_match_and_empty_rules_ask(
        specs in prop::collection::vec((any::<bool>(), action_strategy()), 0..64)
    ) {
        let rules: Vec<_> = specs
            .iter()
            .enumerate()
            .map(|(index, (matches, action))| {
                if *matches {
                    let permission = if index % 2 == 0 { "shell" } else { "s*" };
                    let pattern = if index % 3 == 0 { "git push" } else { "git *" };
                    rule(permission, pattern, *action)
                } else {
                    rule("edit", "cargo *", *action)
                }
            })
            .collect();
        let expected = specs
            .iter()
            .rev()
            .find(|(matches, _)| *matches)
            .map_or(PermissionAction::Ask, |(_, action)| *action);

        prop_assert_eq!(evaluate("shell", "git push", &rules), expected);
        prop_assert_eq!(evaluate("shell", "git push", &[]), PermissionAction::Ask);
    }
}

// ---------------------------------------------------------------------------
// Path rules: an allow matches respellings of the same path, a deny reaches
// further. `docs/guide/permissions.md`, section "Per-tool rules", tells users to
// plan around this asymmetry, so it is pinned here.
// ---------------------------------------------------------------------------

fn path_rules(action: PermissionAction) -> Vec<Rule> {
    vec![
        rule("edit", "*", PermissionAction::Ask),
        rule("edit", "src/main.rs", action),
    ]
}

#[test]
fn an_allow_covers_every_spelling_of_the_path_it_names() {
    let rules = path_rules(PermissionAction::Allow);

    for resource in [
        "src/main.rs",
        "./src/main.rs",
        "src//main.rs",
        "src\\main.rs",
        "./src/./main.rs",
    ] {
        assert_eq!(
            evaluate("edit", resource, &rules),
            PermissionAction::Allow,
            "{resource} is the same file the rule names, spelled differently"
        );
    }
}

#[test]
fn an_allow_never_widens_to_a_path_the_rule_did_not_name() {
    let rules = path_rules(PermissionAction::Allow);

    // The documented `filePath` contract is an absolute path, so a relative allow
    // does not cover the absolute one. Widening it would authorize a file outside
    // the workspace that happens to share the tail, which is why the guide tells
    // users to write an allow with `~`, `$HOME`, an absolute prefix, or a wildcard.
    assert_eq!(
        evaluate("edit", "/ws/src/main.rs", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate("edit", "other/src/main.rs", &rules),
        PermissionAction::Ask
    );
    assert_eq!(
        evaluate(
            "edit",
            "/ws/src/main.rs",
            &rules_from_json(
                r#"{"mode":"standard","rules":{"edit":{"*":"ask","*/src/*":"allow"}}}"#
            )
        ),
        PermissionAction::Allow,
        "the wildcard spelling the guide recommends does reach the absolute path"
    );
}

#[test]
fn a_deny_reaches_the_absolute_and_parent_resolved_spellings_too() {
    let rules = path_rules(PermissionAction::Deny);

    for resource in [
        "src/main.rs",
        "./src/main.rs",
        "/ws/src/main.rs",
        "src/../src/main.rs",
    ] {
        assert_eq!(
            evaluate("edit", resource, &rules),
            PermissionAction::Deny,
            "a deny must not be sidestepped by respelling {resource}"
        );
    }
}
