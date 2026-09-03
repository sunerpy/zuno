use proptest::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use zuno_config::Config;
use zuno_config::discovery::{
    DiscoveryOptions, ManagedPreferences, discover_with, json_error_byte_offset, merge_layers,
};
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::permission::{
    PermissionAction, PermissionConfig, PermissionMode, PermissionObject, PermissionRule,
};
use zuno_config::schema::sandbox::{SandboxMode, SandboxNetworkMode, SandboxUnavailableAction};
use zuno_error::ConfigError;
use zuno_paths::Env;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create config parent");
    }
    fs::write(path, contents).expect("write config");
}

fn fixture_options(
    root: &Path,
    cwd: &Path,
    extra_env: impl IntoIterator<Item = (String, String)>,
) -> DiscoveryOptions {
    let home = root.join("home");
    let xdg_config = root.join("xdg-config");
    for path in [&home, &xdg_config, cwd] {
        fs::create_dir_all(path).expect("create fixture directory");
    }
    let mut pairs = vec![
        ("HOME".to_owned(), home.display().to_string()),
        ("ZUNO_TEST_HOME".to_owned(), home.display().to_string()),
        (
            "XDG_CONFIG_HOME".to_owned(),
            xdg_config.display().to_string(),
        ),
    ];
    pairs.extend(extra_env);
    DiscoveryOptions::new(cwd, Some(root.join("project")), Env::from_pairs(pairs))
        .with_default_username("unknown")
}

fn instructions(config: &Config) -> Vec<&str> {
    config
        .instructions
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect()
}

#[test]
fn project_aware_options_keep_ancestor_config_visible_outside_git() {
    let root = tempfile::tempdir().expect("temporary discovery root");
    let parent = root.path().join("plain-project");
    let nested = parent.join("nested");
    fs::create_dir_all(&nested).expect("create non-git nested directory");
    // The marker is `model`, not `shell`: a project layer that names a host command
    // is now refused unless a trusted layer admits the checkout, which
    // `tests/host_command_trust.rs` pins. This test is about which files are
    // *visible* outside a git worktree.
    write(
        &parent.join(".zuno/zuno.json"),
        r#"{"model":"provider/model"}"#,
    );
    let home = root.path().join("home");
    let xdg_config = root.path().join("xdg-config");
    fs::create_dir_all(&home).expect("create isolated home");
    fs::create_dir_all(&xdg_config).expect("create isolated config home");
    let options = DiscoveryOptions::for_directory(
        &nested,
        Env::from_pairs([
            ("HOME", home.to_string_lossy().as_ref()),
            ("ZUNO_TEST_HOME", home.to_string_lossy().as_ref()),
            ("XDG_CONFIG_HOME", xdg_config.to_string_lossy().as_ref()),
        ]),
    )
    .with_default_username("unknown");

    assert!(options.worktree().is_none());
    let config = discover_with(&options).expect("ancestor project config is discovered");
    assert_eq!(config.model.as_deref(), Some("provider/model"));
}

#[test]
fn an_empty_zuno_root_gets_native_config_and_global_instructions() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("a genuinely fresh install gets a default config");

    let generated = temp.path().join("xdg-config/zuno/zuno.json");
    let generated_instructions = temp.path().join("xdg-config/zuno/AGENTS.md");
    assert_eq!(config.schema, None);
    assert!(generated.is_file());
    assert_eq!(
        fs::read_to_string(generated).expect("read generated config"),
        "{}\n"
    );
    let instructions =
        fs::read_to_string(&generated_instructions).expect("read generated global instructions");
    assert!(generated_instructions.is_file());
    assert!(instructions.contains("# Zuno global working rules"));
    assert!(instructions.contains("`git-workflow`"));
    assert!(instructions.contains("`worktree`"));
    assert!(instructions.contains("Never remove a dirty worktree"));
}

#[test]
fn an_existing_global_agents_file_is_never_overwritten() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let agents = temp.path().join("xdg-config/zuno/AGENTS.md");
    write(&agents, "user-owned global rules\n");

    discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("existing global instructions remain usable");

    assert_eq!(
        fs::read_to_string(agents).expect("read preserved instructions"),
        "user-owned global rules\n"
    );
}

#[test]
fn opencode_config_is_unrelated_to_zuno_discovery() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    write(
        &temp.path().join("xdg-config/opencode/opencode.json"),
        r#"{"model":"provider/opencode"}"#,
    );
    write(
        &temp.path().join("xdg-config/zuno/zuno.json"),
        r#"{"model":"provider/zuno"}"#,
    );

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("only the Zuno root participates");

    assert_eq!(config.model.as_deref(), Some("provider/zuno"));
}

#[test]
fn explicit_config_overrides_ignore_unrelated_product_config() {
    for key in ["ZUNO_CONFIG", "ZUNO_CONFIG_DIR", "ZUNO_CONFIG_CONTENT"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        write(
            &temp.path().join("xdg-config/opencode/opencode.json"),
            r#"{"model":"provider/opencode"}"#,
        );
        let override_file = temp.path().join("override/zuno.json");
        let override_dir = temp.path().join("override-dir");
        let value = match key {
            "ZUNO_CONFIG" => {
                write(&override_file, r#"{"model":"provider/override"}"#);
                override_file.display().to_string()
            }
            "ZUNO_CONFIG_DIR" => {
                write(
                    &override_dir.join("zuno.json"),
                    r#"{"model":"provider/override"}"#,
                );
                override_dir.display().to_string()
            }
            "ZUNO_CONFIG_CONTENT" => r#"{"model":"provider/override"}"#.to_owned(),
            _ => unreachable!("the override list is closed"),
        };

        let config = discover_with(&fixture_options(
            temp.path(),
            &project,
            [(key.to_owned(), value)],
        ))
        .unwrap_or_else(|error| panic!("{key} must remain authoritative: {error:?}"));

        assert_eq!(config.model.as_deref(), Some("provider/override"), "{key}");
    }
}

#[test]
fn trusted_layers_may_select_full_access_but_project_layers_may_only_narrow() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(
        &root.join("xdg-config/zuno/zuno.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    );
    write(
        &project.join(".zuno/zuno.json"),
        r#"{"sandbox":{"mode":"read-only"}}"#,
    );

    let narrowed = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect("a project may narrow a trusted full-access choice");
    assert_eq!(narrowed.sandbox_mode(), SandboxMode::ReadOnly);

    write(
        &project.join(".zuno/zuno.json"),
        r#"{"sandbox":{"mode":"danger-full-access"}}"#,
    );
    let error = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect_err("a project file must not disable confinement");
    let ConfigError::Invalid { path, issues } = error else {
        panic!("expected project sandbox validation failure");
    };
    assert_eq!(path, project.join(".zuno/zuno.json"));
    assert_eq!(issues[0].key_path, ["sandbox", "mode"]);
    assert!(issues[0].detail.contains("trusted"));
}

#[test]
fn project_layers_cannot_grant_host_network_or_external_write_roots() {
    for (body, key) in [
        (r#"{"sandbox":{"network":"allow"}}"#, "network"),
        (
            r#"{"sandbox":{"onUnavailable":"run-unconfined"}}"#,
            "onUnavailable",
        ),
        (
            r#"{"sandbox":{"writableRoots":["../shared-cache"]}}"#,
            "writableRoots",
        ),
        (r#"{"sandbox":{"mode":"workspace-write"}}"#, "mode"),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".git")).expect("worktree marker");
        let path = project.join(".zuno/zuno.json");
        write(&path, body);

        let error = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
            .expect_err("project sandbox authority must be monotonic");
        let ConfigError::Invalid {
            path: actual,
            issues,
        } = error
        else {
            panic!("expected project sandbox validation failure");
        };
        assert_eq!(actual, path);
        assert_eq!(issues[0].key_path, ["sandbox", key]);
        assert!(issues[0].detail.contains("trusted"));
    }
}

#[test]
fn trusted_unavailable_override_is_typed_and_managed_policy_can_deny_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let env_only = discover_with(&fixture_options(
        temp.path(),
        &project,
        [(
            "ZUNO_SANDBOX_ON_UNAVAILABLE".to_owned(),
            "run-unconfined".to_owned(),
        )],
    ))
    .expect("trusted environment override resolves");
    assert_eq!(
        env_only.sandbox_on_unavailable(),
        SandboxUnavailableAction::RunUnconfined
    );

    let managed = temp.path().join("managed");
    write(
        &managed.join("zuno.json"),
        r#"{"sandbox":{"onUnavailable":"deny"}}"#,
    );
    let options = fixture_options(
        temp.path(),
        &project,
        [
            (
                "ZUNO_SANDBOX_ON_UNAVAILABLE".to_owned(),
                "run-unconfined".to_owned(),
            ),
            (
                "ZUNO_TEST_MANAGED_CONFIG_DIR".to_owned(),
                managed.to_string_lossy().into_owned(),
            ),
        ],
    );

    let config = discover_with(&options).expect("managed sandbox policy resolves");

    assert_eq!(
        config.sandbox_on_unavailable(),
        SandboxUnavailableAction::Deny
    );
}

#[test]
fn project_layer_may_narrow_trusted_unavailable_fallback_to_deny() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(
        &root.join("xdg-config/zuno/zuno.json"),
        r#"{"sandbox":{"onUnavailable":"run-unconfined"}}"#,
    );
    write(
        &project.join(".zuno/zuno.json"),
        r#"{"sandbox":{"onUnavailable":"deny"}}"#,
    );

    let config = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect("project deny may narrow trusted fallback");

    assert_eq!(
        config.sandbox_on_unavailable(),
        SandboxUnavailableAction::Deny
    );
}

#[test]
fn invalid_unavailable_override_names_the_exact_configuration_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let error = discover_with(&fixture_options(
        temp.path(),
        &project,
        [(
            "ZUNO_SANDBOX_ON_UNAVAILABLE".to_owned(),
            "sometimes".to_owned(),
        )],
    ))
    .expect_err("unknown unavailable action must fail");
    let ConfigError::Invalid { path, issues } = error else {
        panic!("expected typed unavailable-action validation failure");
    };

    assert_eq!(path, Path::new("ZUNO_SANDBOX_ON_UNAVAILABLE"));
    assert_eq!(issues[0].key_path, ["sandbox", "onUnavailable"]);
}

#[test]
fn sandbox_mode_env_override_clears_confined_fields_for_danger_full_access() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let config = discover_with(&fixture_options(
        temp.path(),
        &project,
        [
            (
                "ZUNO_CONFIG_CONTENT".to_owned(),
                r#"{
                    "model":"provider/model",
                    "sandbox":{
                        "mode":"workspace-write",
                        "network":"deny",
                        "writableRoots":["../shared-cache"],
                        "protectedPaths":[".git"]
                    }
                }"#
                .to_owned(),
            ),
            (
                "ZUNO_SANDBOX_MODE".to_owned(),
                "danger-full-access".to_owned(),
            ),
        ],
    ))
    .expect("the invocation mode override must replace incompatible confined fields");

    assert_eq!(config.model.as_deref(), Some("provider/model"));
    let sandbox = config.sandbox.expect("sandbox config");
    assert_eq!(sandbox.mode, Some(SandboxMode::DangerFullAccess));
    assert_eq!(sandbox.network, None);
    assert_eq!(sandbox.writable_roots, None);
    assert_eq!(sandbox.protected_paths, None);
}

#[test]
fn sandbox_mode_env_override_clears_only_writable_roots_for_read_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let config = discover_with(&fixture_options(
        temp.path(),
        &project,
        [
            (
                "ZUNO_CONFIG_CONTENT".to_owned(),
                r#"{
                    "sandbox":{
                        "mode":"workspace-write",
                        "network":"allow",
                        "writableRoots":["../shared-cache"],
                        "protectedPaths":[".git"]
                    }
                }"#
                .to_owned(),
            ),
            ("ZUNO_SANDBOX_MODE".to_owned(), "read-only".to_owned()),
        ],
    ))
    .expect("the read-only override must remove write authority");

    let sandbox = config.sandbox.expect("sandbox config");
    assert_eq!(sandbox.mode, Some(SandboxMode::ReadOnly));
    assert_eq!(sandbox.network, Some(SandboxNetworkMode::Allow));
    assert_eq!(sandbox.writable_roots, None);
    assert_eq!(
        sandbox.protected_paths.as_deref(),
        Some([".git".to_owned()].as_slice())
    );
}

#[test]
fn discovery_instructions_keep_earlier_entries_first_and_deduplicate() {
    let first = Config {
        model: Some("provider/first".to_owned()),
        instructions: Some(vec!["shared".to_owned(), "first".to_owned()]),
        ..Config::default()
    };
    let second = Config {
        model: Some("provider/second".to_owned()),
        instructions: Some(vec!["second".to_owned(), "shared".to_owned()]),
        ..Config::default()
    };

    let merged = merge_layers([first, second]).expect("merge valid configs");

    assert_eq!(merged.model.as_deref(), Some("provider/second"));
    assert_eq!(instructions(&merged), ["shared", "first", "second"]);
}

#[test]
fn discovery_deep_merges_continuity_object_fields() {
    let first = Config::from_json_str(
        std::path::Path::new("first.json"),
        r#"{"continuity":{"history":true}}"#,
    )
    .expect("first");
    let second = Config::from_json_str(
        std::path::Path::new("second.json"),
        r#"{"continuity":{"notes":true}}"#,
    )
    .expect("second");

    assert_eq!(
        merge_layers([first, second])
            .expect("merge")
            .resolved_continuity(),
        zuno_config::schema::ResolvedContinuityConfig {
            history: true,
            notes: true,
        }
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn discovery_proptest_is_last_wins_for_scalars_and_concat_dedup_for_instructions(
        generated in prop::collection::vec((any::<u16>(), any::<u8>()), 2..24),
    ) {
        let layers = generated.iter().map(|(model, instruction)| Config {
            model: Some(format!("provider/model-{model}")),
            instructions: Some(vec![format!("instruction-{instruction}")]),
            ..Config::default()
        });
        let merged = merge_layers(layers).expect("generated configs are valid");
        let expected_model = generated
            .last()
            .map(|(model, _)| format!("provider/model-{model}"));

        prop_assert_eq!(
            merged.model.as_deref(),
            expected_model.as_deref(),
        );

        let mut seen = HashSet::new();
        let expected: Vec<String> = generated
            .iter()
            .map(|(_, instruction)| format!("instruction-{instruction}"))
            .filter(|instruction| seen.insert(instruction.clone()))
            .collect();
        prop_assert_eq!(merged.instructions.as_deref(), Some(expected.as_slice()));
    }
}

#[test]
fn discovery_walks_every_layer_in_oracle_precedence_order() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    let cwd = project.join("nested");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");

    // Every layer under the one canonical filename. The global root used to have a
    // third entry, `config.json`, which no other layer ever probed; it is gone, so
    // this list is now the same two spellings everywhere.
    let global = root.join("xdg-config/zuno");
    write(
        &global.join("zuno.json"),
        r#"{"model":"global-json","instructions":["global-json"]}"#,
    );
    write(
        &global.join("zuno.jsonc"),
        r#"{"model":"global-jsonc","instructions":["global-jsonc",],}"#,
    );

    let env_file = root.join("env/zuno.json");
    write(
        &env_file,
        r#"{"model":"env-file","instructions":["env-file"]}"#,
    );
    write(
        &project.join("zuno.json"),
        r#"{"model":"project-root","instructions":["project-root"]}"#,
    );
    write(
        &cwd.join("zuno.jsonc"),
        r#"{"model":"project-nearest","instructions":["project-nearest"]}"#,
    );
    write(
        &cwd.join(".zuno/zuno.json"),
        r#"{"model":"dot-nearest","instructions":["dot-nearest"]}"#,
    );
    write(
        &project.join(".zuno/zuno.json"),
        r#"{"model":"dot-root","instructions":["dot-root"]}"#,
    );
    write(
        &root.join("home/.zuno/zuno.json"),
        r#"{"model":"home-dot","instructions":["home-dot"]}"#,
    );
    let override_dir = root.join("override");
    write(
        &override_dir.join("zuno.jsonc"),
        r#"{"model":"config-dir","instructions":["config-dir"]}"#,
    );
    let managed_dir = root.join("managed");
    write(
        &managed_dir.join("zuno.json"),
        r#"{"model":"managed-json","instructions":["managed-json"]}"#,
    );
    write(
        &managed_dir.join("zuno.jsonc"),
        r#"{"model":"managed-jsonc","instructions":["managed-jsonc"]}"#,
    );

    let options = fixture_options(
        root,
        &cwd,
        [
            ("ZUNO_CONFIG".to_owned(), env_file.display().to_string()),
            (
                "ZUNO_CONFIG_DIR".to_owned(),
                override_dir.display().to_string(),
            ),
            (
                "ZUNO_CONFIG_CONTENT".to_owned(),
                r#"{"model":"content","instructions":["content","global-json"]}"#.to_owned(),
            ),
            (
                "ZUNO_TEST_MANAGED_CONFIG_DIR".to_owned(),
                managed_dir.display().to_string(),
            ),
        ],
    )
    .with_managed_preferences(ManagedPreferences::new(
        "mobileconfig:/Library/Managed Preferences/ai.zuno.managed.plist",
        r#"{"model":"mac-managed","instructions":["mac-managed","content"]}"#,
    ));

    let config = discover_with(&options).expect("discover all layers");

    assert_eq!(config.model.as_deref(), Some("mac-managed"));
    assert_eq!(
        instructions(&config),
        [
            "global-json",
            "global-jsonc",
            "env-file",
            "project-root",
            "project-nearest",
            "dot-nearest",
            "dot-root",
            "home-dot",
            "config-dir",
            "content",
            "managed-json",
            "managed-jsonc",
            "mac-managed",
        ]
    );
}

#[test]
fn discovery_jsonc_accepts_comments_and_trailing_commas_and_reports_bad_offset() {
    let valid = tempfile::tempdir().expect("valid tempdir");
    let valid_project = valid.path().join("project");
    fs::create_dir_all(valid_project.join(".git")).expect("worktree marker");
    let valid_path = valid_project.join(".zuno/zuno.jsonc");
    write(
        &valid_path,
        "{\n  // accepted comment\n  \"model\": \"provider/valid\",\n  \"instructions\": [\"one\",],\n}\n",
    );
    let valid_config = discover_with(&fixture_options(
        valid.path(),
        &valid_project,
        std::iter::empty(),
    ))
    .expect("valid JSONC");
    assert_eq!(valid_config.model.as_deref(), Some("provider/valid"));
    assert_eq!(instructions(&valid_config), ["one"]);

    let invalid = tempfile::tempdir().expect("invalid tempdir");
    let invalid_project = invalid.path().join("project");
    fs::create_dir_all(invalid_project.join(".git")).expect("worktree marker");
    let invalid_path = invalid_project.join(".zuno/zuno.jsonc");
    let malformed = "{\n  \"model\": ,\n}\n";
    write(&invalid_path, malformed);

    let error = discover_with(&fixture_options(
        invalid.path(),
        &invalid_project,
        std::iter::empty(),
    ))
    .expect_err("malformed JSONC must fail");
    let ConfigError::Json { path, source } = &error else {
        panic!("expected ConfigError::Json, got {error:?}");
    };
    assert_eq!(path.as_path(), invalid_path.as_path());
    let offset = json_error_byte_offset(malformed, source);
    assert_eq!(offset, malformed.find(',').expect("malformed comma"));
    assert!(offset > 0);
}

#[test]
fn discovery_does_not_inject_an_external_schema_into_file_layers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    let path = project.join("zuno.json");
    write(&path, r#"{"model":"provider/model"}"#);

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("discover config");

    assert_eq!(config.schema, None);
    assert_eq!(
        fs::read_to_string(path).expect("read config"),
        r#"{"model":"provider/model"}"#
    );
}

#[test]
fn discovery_applies_permission_after_managed_preferences_and_tools_defaults_first() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    let options = fixture_options(
        temp.path(),
        &project,
        [
            (
                "ZUNO_CONFIG_CONTENT".to_owned(),
                r#"{"tools":{"shell":false,"write":true,"history":false,"notes":true},"permission":{"rules":{"read":"ask"}}}"#
                    .to_owned(),
            ),
            (
                "ZUNO_PERMISSION".to_owned(),
                r#"{"shell":"allow","edit":"deny"}"#.to_owned(),
            ),
        ],
    )
    .with_managed_preferences(ManagedPreferences::new(
        "mobileconfig:test",
        r#"{"permission":{"rules":{"read":"deny","glob":"ask"}}}"#,
    ));

    let config = discover_with(&options).expect("discover permissions");
    let permission = config.permission.expect("permission policy").rules;
    let expected = [
        ("shell", PermissionAction::Allow),
        ("edit", PermissionAction::Deny),
        ("read", PermissionAction::Deny),
        ("glob", PermissionAction::Ask),
        ("history", PermissionAction::Deny),
        ("notes", PermissionAction::Allow),
    ];
    for (key, action) in expected {
        assert_eq!(
            permission.get(key),
            Some(&PermissionRule::Action(action)),
            "permission {key}"
        );
    }
}

#[test]
fn discovery_preserves_permission_pattern_order() {
    let mut patterns = OrderedMap::new();
    patterns.insert("*", PermissionAction::Ask);
    patterns.insert("*.secret", PermissionAction::Deny);
    patterns.insert("README.md", PermissionAction::Allow);
    let config = Config {
        permission: Some(PermissionConfig {
            mode: PermissionMode::Standard,
            rules: PermissionObject(
                [("read".to_owned(), PermissionRule::Patterns(patterns))]
                    .into_iter()
                    .collect(),
            ),
        }),
        ..Config::default()
    };

    let merged = merge_layers([config]).expect("merge permission");
    let permission = merged.permission.expect("permission").rules;
    let Some(PermissionRule::Patterns(patterns)) = permission.get("read") else {
        panic!("read patterns expected");
    };
    assert_eq!(
        patterns.keys().collect::<Vec<_>>(),
        ["*", "*.secret", "README.md"]
    );
}

// ---------------------------------------------------------------------------
// The config filename is Zuno's own, at every layer
// ---------------------------------------------------------------------------

/// The layers a config file can live in, each as `(label, relative path)`.
fn layers() -> [(&'static str, &'static str); 5] {
    [
        ("global root", "xdg-config/zuno"),
        ("project ancestor", "project"),
        ("project .zuno", "project/.zuno"),
        ("config-dir override", "override"),
        ("managed", "managed"),
    ]
}

fn layer_options(root: &Path, cwd: &Path) -> DiscoveryOptions {
    fixture_options(
        root,
        cwd,
        [
            (
                "ZUNO_CONFIG_DIR".to_owned(),
                root.join("override").display().to_string(),
            ),
            (
                "ZUNO_TEST_MANAGED_CONFIG_DIR".to_owned(),
                root.join("managed").display().to_string(),
            ),
        ],
    )
}

#[test]
fn opencode_named_files_are_ignored_at_every_zuno_layer() {
    for (label, relative) in layers() {
        for name in ["opencode.json", "opencode.jsonc"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let root = temp.path();
            let project = root.join("project");
            fs::create_dir_all(project.join(".git")).expect("worktree marker");
            fs::create_dir_all(root.join("managed")).expect("managed directory");
            write(
                &root.join(relative).join(name),
                r#"{"model":"provider/opencode"}"#,
            );

            let config = discover_with(&layer_options(root, &project))
                .unwrap_or_else(|error| panic!("{label}/{name} is unrelated: {error:?}"));
            assert_ne!(
                config.model.as_deref(),
                Some("provider/opencode"),
                "{label}/{name} must not enter Zuno's config graph"
            );
        }
    }
}

#[test]
fn a_canonically_named_config_is_read_at_every_layer() {
    for (label, relative) in layers() {
        for name in ["zuno.json", "zuno.jsonc"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let root = temp.path();
            let project = root.join("project");
            fs::create_dir_all(project.join(".git")).expect("worktree marker");
            fs::create_dir_all(root.join("managed")).expect("managed directory");
            write(
                &root.join(relative).join(name),
                r#"{"model":"provider/live"}"#,
            );

            let config = discover_with(&layer_options(root, &project))
                .unwrap_or_else(|error| panic!("{label}/{name} must be read: {error:?}"));
            assert_eq!(
                config.model.as_deref(),
                Some("provider/live"),
                "{label}/{name} was found but its setting did not take effect"
            );
        }
    }
}

#[test]
fn config_json_is_unrelated_at_global_and_project_roots() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(
        &root.join("xdg-config/zuno/config.json"),
        r#"{"model":"provider/stale"}"#,
    );

    let config = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect("a generic config filename is not Zuno input");
    assert_ne!(config.model.as_deref(), Some("provider/stale"));

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let root = elsewhere.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(&project.join("config.json"), r#"{"nonsense":true}"#);
    write(&project.join("zuno.json"), r#"{"model":"provider/live"}"#);

    let config = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect("an unrelated project config.json is not Zuno's business");
    assert_eq!(config.model.as_deref(), Some("provider/live"));
}

#[test]
fn a_zuno_file_wins_without_interpreting_an_opencode_sibling() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    let stale = project.join("opencode.json");
    write(&stale, r#"{"model":"provider/stale"}"#);
    write(&project.join("zuno.json"), r#"{"model":"provider/live"}"#);

    let config = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect("only the Zuno file is interpreted");
    assert_eq!(config.model.as_deref(), Some("provider/live"));
}
