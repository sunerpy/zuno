use oc_permission::{PermissionAction, Rule, evaluate, rules_from_config};
use proptest::prelude::*;

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
        rule("bash", "git *", PermissionAction::Allow),
        rule("bash", "*", PermissionAction::Ask),
    ];

    let action = evaluate("bash", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

#[test]
fn later_allow_overrides_earlier_ask_for_the_same_command() {
    let rules = [
        rule("bash", "*", PermissionAction::Ask),
        rule("bash", "git *", PermissionAction::Allow),
    ];

    let action = evaluate("bash", "git push", &rules);

    assert_eq!(action, PermissionAction::Allow);
}

#[test]
fn no_matching_rule_defaults_to_ask() {
    let rules = [rule("edit", "*", PermissionAction::Allow)];

    let action = evaluate("bash", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
}

#[test]
fn permission_keys_and_patterns_both_support_wildcards() {
    let rules = [rule("b?sh", "git *", PermissionAction::Allow)];

    let action = evaluate("bash", "git status", &rules);

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
            "*": "deny",
            "bash": {
                "git *": "allow",
                "*": "ask"
            },
            "deploy_production": "allow"
        }"#,
    )
    .expect("fixture is valid permission config");

    let rules = rules_from_config(&config);

    assert_eq!(
        rules,
        vec![
            rule("*", "*", PermissionAction::Deny),
            rule("bash", "git *", PermissionAction::Allow),
            rule("bash", "*", PermissionAction::Ask),
            rule("deploy_production", "*", PermissionAction::Allow),
        ]
    );
}

#[test]
fn outer_config_key_order_controls_overlapping_permission_keys() {
    let config = serde_json::from_str(r#"{"bash":"allow","*":"ask"}"#)
        .expect("fixture is valid permission config");
    let rules = rules_from_config(&config);

    let action = evaluate("bash", "git push", &rules);

    assert_eq!(action, PermissionAction::Ask);
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
                    let permission = if index % 2 == 0 { "bash" } else { "b*" };
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

        prop_assert_eq!(evaluate("bash", "git push", &rules), expected);
        prop_assert_eq!(evaluate("bash", "git push", &[]), PermissionAction::Ask);
    }
}
