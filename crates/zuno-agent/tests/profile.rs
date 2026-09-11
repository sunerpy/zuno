use zuno_agent::profile::{AgentProfile, ShellFilesystemAccess};
use zuno_catalog::agent;
use zuno_config::schema::ordered::OrderedMap;
use zuno_permission::visibility::is_tool_hidden;
use zuno_permission::{PermissionAction, Rule};

fn rule(permission: &str, action: PermissionAction) -> Rule {
    Rule {
        source: None,
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
fn native_memory_tools_follow_role_and_later_user_denies() {
    for name in [
        "build",
        "orchestrator",
        "deep",
        "general",
        "fixer",
        "explorer",
    ] {
        let role = zuno_agent::builtin::get(name, false).expect("role");
        let rules = role.rules();
        assert!(!is_tool_hidden("memory_read", &rules), "{name}");
        assert!(!is_tool_hidden("experience_search", &rules), "{name}");
        let can_write = matches!(
            name,
            "build" | "orchestrator" | "deep" | "general" | "fixer"
        );
        assert_eq!(
            !is_tool_hidden("memory_update", &rules),
            can_write,
            "{name}"
        );
        let mut denied = rules;
        denied.push(rule("memory_update", PermissionAction::Deny));
        assert!(is_tool_hidden("memory_update", &denied));
    }
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
fn profile_exposes_capability_facts_without_rendering_a_premature_prompt() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
        // `edit` is the shared permission key for edit/write/apply_patch.
        rule("edit", PermissionAction::Allow),
    ];
    let profile = AgentProfile::resolve(native("fixer"), rules, false);

    assert!(profile.capabilities().tool_available("edit"));
    assert!(!profile.capabilities().tool_available("shell"));
    assert!(!profile.capabilities().tool_available("task"));
}

#[test]
fn native_routing_advice_is_retained_as_data_for_late_runtime_rendering() {
    let rules = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
    ];
    let profile = AgentProfile::resolve(native("oracle"), rules, false);
    let guidance = profile
        .delegation_guidance()
        .expect("native delegation boundary");

    assert!(guidance.contains("Don't delegate when"), "{guidance}");
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

#[test]
fn a_readonly_child_narrows_an_allow_all_parent_without_regranting_edits() {
    let profile = AgentProfile::resolve(
        native("explorer"),
        vec![rule("*", PermissionAction::Allow)],
        false,
    )
    .with_parent_authority(
        vec![rule("*", PermissionAction::Allow)],
        [
            "read",
            "shell",
            "bg",
            "apply_patch",
            "write",
            "memory_update",
            "mcp_write",
        ]
        .map(str::to_owned),
        ShellFilesystemAccess::WorkspaceWrite,
    );
    assert!(profile.capabilities().tool_available("read"));
    assert!(profile.capabilities().tool_available("shell"));
    for tool in ["apply_patch", "write", "memory_update", "mcp_write"] {
        assert!(!profile.capabilities().tool_available(tool), "{tool}");
    }
    assert_eq!(
        profile.capabilities().shell_filesystem_access(),
        ShellFilesystemAccess::ReadOnly
    );
    assert_eq!(
        zuno_permission::evaluate("edit", "/workspace/file", profile.capabilities().rules()),
        PermissionAction::Deny
    );
}

#[test]
fn children_retain_parent_ask_deny_resources_and_rule_precedence() {
    let parent = vec![
        rule("*", PermissionAction::Deny),
        rule("read", PermissionAction::Allow),
        Rule {
            source: Some("current-parent".to_owned()),
            permission: "read".to_owned(),
            pattern: "/workspace/private/*".to_owned(),
            action: PermissionAction::Deny,
        },
        Rule {
            source: Some("current-parent".to_owned()),
            permission: "read".to_owned(),
            pattern: "/workspace/review/*".to_owned(),
            action: PermissionAction::Ask,
        },
        rule("mcp_query", PermissionAction::Ask),
    ];
    let profile = AgentProfile::resolve_with_extension_boundary(
        native("general"),
        vec![rule("*", PermissionAction::Allow)],
        1,
        false,
    )
    .with_parent_authority(
        parent.clone(),
        ["read", "mcp_query"].map(str::to_owned),
        ShellFilesystemAccess::WorkspaceWrite,
    );
    assert_eq!(&profile.capabilities().rules()[..parent.len()], parent);
    let expanded = profile.rules_with_extension_tools(&["mcp_query", "new_unapproved_mcp"]);
    assert_eq!(expanded, profile.capabilities().rules());
    for (resource, expected) in [
        ("/workspace/private/key", PermissionAction::Deny),
        ("/workspace/review/notes", PermissionAction::Ask),
        ("/workspace/public", PermissionAction::Allow),
    ] {
        assert_eq!(
            zuno_permission::evaluate("read", resource, &expanded),
            expected
        );
    }
    assert_eq!(
        zuno_permission::evaluate("mcp_query", "*", &expanded),
        PermissionAction::Ask
    );
    assert!(profile.capabilities().tool_available("mcp_query"));
    assert!(!profile.capabilities().tool_available("new_unapproved_mcp"));
}

#[test]
fn ordinary_children_inherit_writable_shell_and_authorized_deferred_tools() {
    for name in ["deep", "general", "fixer"] {
        let profile =
            AgentProfile::resolve(native(name), vec![rule("*", PermissionAction::Deny)], false)
                .with_parent_authority(
                    vec![rule("*", PermissionAction::Allow)],
                    ["shell", "bg", "mcp_query", "task"].map(str::to_owned),
                    ShellFilesystemAccess::WorkspaceWrite,
                );
        assert!(profile.capabilities().tool_available("shell"), "{name}");
        assert!(profile.capabilities().tool_available("mcp_query"), "{name}");
        assert_eq!(
            profile.capabilities().shell_filesystem_access(),
            ShellFilesystemAccess::WorkspaceWrite,
            "{name}: a script does not need a separate edit schema"
        );
        assert_eq!(
            profile.capabilities().can_delegate(),
            name == "deep",
            "{name}"
        );
    }
}

#[test]
fn a_readonly_parent_and_repeated_tool_bounds_can_never_be_widened() {
    let profile = AgentProfile::resolve(
        native("deep"),
        vec![rule("*", PermissionAction::Allow)],
        false,
    )
    .with_tool_authority(["read".to_owned(), "shell".to_owned()])
    .with_parent_authority(
        vec![rule("*", PermissionAction::Allow)],
        ["read", "shell", "write"].map(str::to_owned),
        ShellFilesystemAccess::ReadOnly,
    )
    .with_tool_authority(["read", "shell", "write", "mcp_query"].map(str::to_owned));
    assert!(!profile.capabilities().tool_available("write"));
    assert!(!profile.capabilities().tool_available("mcp_query"));
    assert_eq!(
        profile.capabilities().shell_filesystem_access(),
        ShellFilesystemAccess::ReadOnly
    );
}

#[test]
fn explicit_child_denies_narrow_parent_authority_and_allowlists_do_not_deny_sibling_aliases() {
    let mut definition = native("general");
    definition.source = agent::AgentSource::NativeOverridden;
    definition.tools = Some(vec![
        "read".to_owned(),
        "shell".to_owned(),
        "mcp_drop".to_owned(),
    ]);
    definition.permission = Some(
        serde_json::from_value(serde_json::json!({
            "rules": {"shell":{"rm *":"deny"}, "mcp_drop":"deny"}
        }))
        .expect("explicit child restrictions"),
    );
    let profile = AgentProfile::resolve(definition, Vec::new(), false).with_parent_authority(
        vec![rule("*", PermissionAction::Allow)],
        ["read", "read_mcp_resource", "shell", "mcp_drop"].map(str::to_owned),
        ShellFilesystemAccess::WorkspaceWrite,
    );
    assert!(profile.capabilities().tool_available("read"));
    assert!(!profile.capabilities().tool_available("read_mcp_resource"));
    assert!(!profile.capabilities().tool_available("mcp_drop"));
    assert_eq!(
        zuno_permission::evaluate("shell", "rm file", profile.capabilities().rules()),
        PermissionAction::Deny,
    );
    assert_eq!(
        zuno_permission::evaluate("shell", "cat file", profile.capabilities().rules()),
        PermissionAction::Allow,
    );
}

#[test]
fn native_catalog_deny_baselines_cannot_shadow_inherited_working_permissions() {
    for name in ["deep", "general", "fixer"] {
        for materialized in [false, true] {
            let mut definition = native(name);
            assert_eq!(definition.source, agent::AgentSource::Native);
            let baseline = agent::builtin::get(name)
                .expect("native")
                .permission_overlay()
                .expect("native permission baseline");
            let baseline_rules = zuno_permission::rules_from_config(&baseline);
            assert_eq!(baseline_rules[0].permission, "*");
            assert_eq!(baseline_rules[0].action, PermissionAction::Deny);
            if materialized {
                definition.permission = Some(baseline);
            }
            let parent = vec![
                rule("*", PermissionAction::Allow),
                rule("external_directory", PermissionAction::Ask),
            ];
            let profile = AgentProfile::resolve(definition, baseline_rules, false)
                .with_parent_authority(
                    parent.clone(),
                    ["shell", "read", "write", "apply_patch", "mcp_query"].map(str::to_owned),
                    ShellFilesystemAccess::WorkspaceWrite,
                );
            assert_eq!(&profile.capabilities().rules()[..parent.len()], parent);
            for tool in ["shell", "write", "apply_patch", "mcp_query"] {
                assert!(
                    profile.capabilities().tool_available(tool),
                    "{name}/{materialized}: {tool}"
                );
            }
            assert_eq!(
                zuno_permission::evaluate(
                    "external_directory",
                    "/parent-extra/file",
                    profile.capabilities().rules()
                ),
                PermissionAction::Ask,
                "{name}/{materialized}"
            );
        }
    }
}
