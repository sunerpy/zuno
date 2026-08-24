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
    let mut entry = native("build");
    entry.model = Some("example/reasoner".to_owned());
    entry.delegates = Some(vec!["explorer".to_owned(), "librarian".to_owned()]);
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
        rule("task", PermissionAction::Allow),
    ];

    let profile = AgentProfile::resolve(entry, rules.clone(), false);

    assert_eq!(profile.name(), "build");
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
    let profile = AgentProfile::resolve(native("worker"), rules, false);
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
    let profile = AgentProfile::resolve(native("advisor"), rules, false);
    let policy = profile.prompt_policy();

    assert!(policy.contains("Don't delegate when"), "{policy}");
    assert!(
        policy.contains("The runtime capability snapshot is authoritative"),
        "{policy}"
    );
}
