use zuno_agent::profile::AgentProfile;
use zuno_catalog::agent;
use zuno_config::schema::ordered::OrderedMap;
use zuno_permission::visibility::is_tool_hidden;
use zuno_permission::{PermissionAction, Rule};

fn rule(permission: &str, action: PermissionAction) -> Rule {
    Rule {
        permission: permission.to_owned(),
        pattern: "*".to_owned(),
        action,
    }
}

fn native(name: &str) -> agent::Agent {
    agent::resolve(&OrderedMap::new(), &[])
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("native agent `{name}`"))
}

#[test]
fn one_profile_snapshots_definition_rules_and_delegation_targets() {
    let mut entry = native("orchestrator");
    entry.model = Some("example/reasoner".to_owned());
    entry.delegates = Some(vec!["explorer".to_owned(), "librarian".to_owned()]);
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
        rule("task", PermissionAction::Allow),
    ];

    let profile = AgentProfile::resolve(entry, rules.clone(), false);

    assert_eq!(profile.name(), "orchestrator");
    assert_eq!(
        profile.definition().model.as_deref(),
        Some("example/reasoner")
    );
    assert_eq!(profile.capabilities().rules(), rules);
    assert_eq!(
        profile.capabilities().delegation_targets(),
        Some(["explorer".to_owned(), "librarian".to_owned()].as_slice())
    );
    assert!(profile.capabilities().can_delegate());
    assert!(!is_tool_hidden("read", profile.capabilities().rules()));
}

#[test]
fn prompt_policy_describes_enforced_rules_instead_of_an_assumed_role() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
        // `edit` is the shared permission key for edit/write/apply_patch.
        rule("edit", PermissionAction::Allow),
    ];
    let profile = AgentProfile::resolve(native("fixer"), rules, false);
    let policy = profile.prompt_policy();

    assert!(policy.contains("Enforced capability snapshot"), "{policy}");
    assert!(policy.contains("delegation: unavailable"), "{policy}");
    assert!(policy.contains("workspace edits: available"), "{policy}");
    assert!(policy.contains("shell: unavailable"), "{policy}");
    assert!(
        policy.contains("external research: unavailable"),
        "{policy}"
    );
}

#[test]
fn native_routing_advice_and_runtime_authority_are_both_preserved() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
    ];
    let profile = AgentProfile::resolve(native("oracle"), rules, false);
    let policy = profile.prompt_policy();

    assert!(policy.contains("Don't delegate when"), "{policy}");
    assert!(
        policy.contains("The runtime capability snapshot is authoritative"),
        "{policy}"
    );
}

#[test]
fn native_orchestrator_freezes_only_currently_available_delegate_targets() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("task", PermissionAction::Allow),
    ];
    let with_vision = AgentProfile::resolve(native("orchestrator"), rules.clone(), true);
    assert_eq!(
        with_vision.capabilities().delegation_targets(),
        Some(
            [
                "deep".to_owned(),
                "fixer".to_owned(),
                "general".to_owned(),
                "explorer".to_owned(),
                "librarian".to_owned(),
                "oracle".to_owned(),
                "looker".to_owned(),
            ]
            .as_slice()
        )
    );

    let without_vision = AgentProfile::resolve(native("orchestrator"), rules, false);
    assert_eq!(
        without_vision.capabilities().delegation_targets(),
        Some(
            [
                "deep".to_owned(),
                "fixer".to_owned(),
                "general".to_owned(),
                "explorer".to_owned(),
                "librarian".to_owned(),
                "oracle".to_owned(),
            ]
            .as_slice()
        )
    );
}

#[test]
fn capability_filter_preserves_custom_targets_for_runtime_validation() {
    let mut entry = native("orchestrator");
    entry.delegates = Some(vec!["looker".to_owned(), "custom-review".to_owned()]);
    let rules = vec![rule("task", PermissionAction::Allow)];

    let profile = AgentProfile::resolve(entry, rules, false);

    assert_eq!(
        profile.capabilities().delegation_targets(),
        Some(["custom-review".to_owned()].as_slice())
    );
}
