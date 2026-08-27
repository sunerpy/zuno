use zuno_agent::profile::{AgentProfile, ShellFilesystemAccess};
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
fn inherited_parent_tools_are_an_upper_bound_on_child_capabilities() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
        rule("skill", PermissionAction::Allow),
        rule("shell", PermissionAction::Allow),
        rule("web_search", PermissionAction::Allow),
    ];
    let profile = AgentProfile::resolve(native("general"), rules, false)
        .with_tool_authority(["read".to_owned(), "skill".to_owned()]);

    assert!(profile.capabilities().tool_available("read"));
    assert!(profile.capabilities().tool_available("skill"));
    assert!(!profile.capabilities().tool_available("shell"));
    assert!(!profile.capabilities().tool_available("web_search"));
    assert!(!profile.capabilities().tool_available("task"));
    assert!(
        profile.prompt_policy().contains("shell: unavailable"),
        "{}",
        profile.prompt_policy()
    );
}

#[test]
fn extension_inheritance_precedes_later_user_denies_and_stays_role_bounded() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("codegraph_query", PermissionAction::Deny),
    ];
    let deep =
        AgentProfile::resolve_with_extension_boundary(native("deep"), rules.clone(), 1, false);
    let effective = deep.rules_with_extension_tools(&["codegraph_query", "codegraph_status"]);

    assert!(is_tool_hidden("codegraph_query", &effective));
    assert!(!is_tool_hidden("codegraph_status", &effective));

    let explorer =
        AgentProfile::resolve_with_extension_boundary(native("explorer"), rules, 1, false);
    let effective = explorer.rules_with_extension_tools(&["codegraph_status"]);
    assert!(is_tool_hidden("codegraph_status", &effective));
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

#[test]
fn shell_filesystem_access_is_derived_from_the_effective_edit_capability() {
    let read_only = AgentProfile::resolve(
        native("explorer"),
        vec![
            rule("*", PermissionAction::Deny),
            rule("shell", PermissionAction::Allow),
        ],
        false,
    );
    assert_eq!(
        read_only.capabilities().shell_filesystem_access(),
        ShellFilesystemAccess::ReadOnly
    );

    let writable = AgentProfile::resolve(
        native("build"),
        vec![
            rule("*", PermissionAction::Deny),
            rule("edit", PermissionAction::Allow),
            rule("shell", PermissionAction::Allow),
        ],
        false,
    );
    assert_eq!(
        writable.capabilities().shell_filesystem_access(),
        ShellFilesystemAccess::WorkspaceWrite
    );
}
