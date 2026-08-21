use proptest::prelude::*;
use std::collections::HashSet;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use zuno_config::Config;
use zuno_config::discovery::{
    DiscoveryOptions, ManagedPreferences, discover_with, json_error_byte_offset, merge_layers,
};
use zuno_config::schema::ordered::OrderedMap;
use zuno_config::schema::permission::{PermissionAction, PermissionConfig, PermissionRule};
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

#[cfg(unix)]
#[test]
fn legacy_copy_command_executes_with_or_without_an_auth_file() {
    for auth_exists in [false, true] {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        let old_path = temp.path().join("xdg-config/opencode/opencode.json");
        let new_path = temp.path().join("xdg-config/zuno/zuno.json");
        let old_auth = temp.path().join("home/.local/share/opencode/auth.json");
        let new_auth = temp.path().join("home/.local/share/zuno/auth.json");
        let old_body = r#"{"model":"provider/legacy"}"#;
        write(&old_path, old_body);
        if auth_exists {
            write(&old_auth, r#"{"provider":{"type":"api","key":"secret"}}"#);
        }

        let result = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()));
        let error = result.expect_err("the old location requires an explicit copy");
        let ConfigError::LegacyConfig {
            old_path: actual_old,
            new_path: actual_new,
            copy_command,
        } = &error
        else {
            panic!("expected ConfigError::LegacyConfig, got {error:?}");
        };
        assert_eq!(actual_old, &old_path);
        assert_eq!(actual_new, &new_path);
        assert!(copy_command.contains("install -d -m 700"), "{copy_command}");
        assert!(copy_command.contains(&old_path.display().to_string()));
        assert!(copy_command.contains(&new_path.display().to_string()));
        assert!(copy_command.contains(&old_auth.display().to_string()));
        assert!(copy_command.contains(&new_auth.display().to_string()));
        let report = error.report();
        assert!(report.contains(&old_path.display().to_string()), "{report}");
        assert!(report.contains(&new_path.display().to_string()), "{report}");
        assert!(report.contains(copy_command), "{report}");

        let status = Command::new("sh")
            .args(["-c", copy_command])
            .status()
            .expect("execute migration command");
        assert!(status.success(), "migration command failed: {copy_command}");
        assert_eq!(
            fs::read_to_string(&new_path).expect("copied config"),
            old_body
        );
        assert_eq!(
            fs::metadata(&new_path)
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(new_auth.exists(), auth_exists);
        if auth_exists {
            assert_eq!(
                fs::read_to_string(&new_auth).expect("copied auth"),
                fs::read_to_string(&old_auth).expect("old auth")
            );
            assert_eq!(
                fs::metadata(&new_auth)
                    .expect("auth metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn an_empty_zuno_root_without_legacy_config_gets_an_empty_native_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("a genuinely fresh install gets a default config");

    let generated = temp.path().join("xdg-config/zuno/zuno.json");
    assert_eq!(config.schema, None);
    assert!(generated.is_file());
    assert_eq!(
        fs::read_to_string(generated).expect("read generated config"),
        "{}\n"
    );
}

#[test]
fn populated_zuno_config_wins_even_when_legacy_config_exists() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    write(
        &temp.path().join("xdg-config/opencode/opencode.json"),
        r#"{"model":"provider/legacy"}"#,
    );
    write(
        &temp.path().join("xdg-config/zuno/zuno.json"),
        r#"{"model":"provider/zuno"}"#,
    );

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("the populated Zuno root is authoritative");

    assert_eq!(config.model.as_deref(), Some("provider/zuno"));
}

#[test]
fn explicit_config_overrides_bypass_the_legacy_location_diagnostic() {
    for key in ["ZUNO_CONFIG", "ZUNO_CONFIG_DIR", "ZUNO_CONFIG_CONTENT"] {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        write(
            &temp.path().join("xdg-config/opencode/opencode.json"),
            r#"{"model":"provider/legacy"}"#,
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
        .unwrap_or_else(|error| panic!("{key} must bypass the diagnostic: {error:?}"));

        assert_eq!(config.model.as_deref(), Some("provider/override"), "{key}");
    }
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
                r#"{"tools":{"bash":false,"write":true},"permission":{"read":"ask"}}"#.to_owned(),
            ),
            (
                "ZUNO_PERMISSION".to_owned(),
                r#"{"bash":"allow","edit":"deny"}"#.to_owned(),
            ),
        ],
    )
    .with_managed_preferences(ManagedPreferences::new(
        "mobileconfig:test",
        r#"{"permission":{"read":"deny","glob":"ask"}}"#,
    ));

    let config = discover_with(&options).expect("discover permissions");
    let PermissionConfig::Object(permission) = config.permission.expect("permission object") else {
        panic!("permission should be normalized to an object");
    };
    let expected = [
        ("bash", PermissionAction::Allow),
        ("edit", PermissionAction::Deny),
        ("read", PermissionAction::Deny),
        ("glob", PermissionAction::Ask),
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
        permission: Some(PermissionConfig::Object(
            zuno_config::schema::permission::PermissionObject(
                [("read".to_owned(), PermissionRule::Patterns(patterns))]
                    .into_iter()
                    .collect(),
            ),
        )),
        ..Config::default()
    };

    let merged = merge_layers([config]).expect("merge permission");
    let PermissionConfig::Object(permission) = merged.permission.expect("permission") else {
        panic!("object permission expected");
    };
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
///
/// One list, used by both directions of the pair below: every layer must reject a
/// legacy-named file *and* read a canonically named one. Two separate lists is how
/// a layer comes to be covered in one direction only — the failure mode recorded
/// for the keybind scope guard, where "consumed implies reachable" was proven and
/// the converse was structurally silent.
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
fn a_legacy_named_config_is_reported_at_every_layer_never_silently_skipped() {
    for (label, relative) in layers() {
        for name in ["opencode.json", "opencode.jsonc"] {
            let temp = tempfile::tempdir().expect("tempdir");
            let root = temp.path();
            let project = root.join("project");
            fs::create_dir_all(project.join(".git")).expect("worktree marker");
            fs::create_dir_all(root.join("managed")).expect("managed directory");
            let stale = root.join(relative).join(name);
            write(&stale, r#"{"model":"provider/stale"}"#);

            let Err(error) = discover_with(&layer_options(root, &project)) else {
                panic!(
                    "a {name} in the {label} layer must be reported, not skipped: a user whose \
                     config stops being read sees no error and no effect"
                );
            };
            let ConfigError::Invalid { issues, .. } = &error else {
                panic!("expected ConfigError::Invalid for {label}/{name}, got {error:?}");
            };
            let detail = issues
                .iter()
                .map(|issue| issue.detail.clone())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                detail.contains(&stale.display().to_string()),
                "the report must name the exact file so it can be renamed without \
                 guessing which layer it was in: {detail}"
            );
            let canonical = name.replace("opencode", "zuno");
            assert!(
                detail.contains(&canonical),
                "the report must name `{canonical}`, keeping the extension, or a JSONC \
                 document is renamed to `.json` and stops parsing: {detail}"
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

/// `config.json` was accepted at the global root and nowhere else, so it is legacy
/// there and an ordinary unrelated file everywhere else. Reporting it at a
/// repository root would fail a great many checkouts over a filename Zuno has
/// never read.
#[test]
fn a_global_config_json_is_reported_while_a_project_one_is_left_alone() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(
        &root.join("xdg-config/zuno/config.json"),
        r#"{"model":"provider/stale"}"#,
    );

    let error = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect_err("a config.json in the global root is no longer read");
    assert!(error.report().contains("config.json"), "{}", error.report());

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

/// Both names present in one directory is refused as ambiguous, not resolved.
///
/// The alternative — letting the new name win silently — is what the *old* code
/// did to `config.json`, whose only effect was to be shadowed by `opencode.json`.
/// Deleting the shadowing name would have made that dead file suddenly live for
/// everyone holding both, changing behaviour with no message. Refusing is the only
/// option that cannot silently start or stop honouring a file: the user says which
/// document they meant by deleting the other.
#[test]
fn both_names_in_one_directory_are_refused_rather_than_resolved() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    let stale = project.join("opencode.json");
    write(&stale, r#"{"model":"provider/stale"}"#);
    write(&project.join("zuno.json"), r#"{"model":"provider/live"}"#);

    let error = discover_with(&fixture_options(root, &project, std::iter::empty())).expect_err(
        "with both names present the old one must still be reported: silently preferring \
         the new one would change which document is honoured without saying so",
    );
    assert!(
        error.report().contains(&stale.display().to_string()),
        "{}",
        error.report()
    );
}

/// Every legacy-named file in one run, so the user renames once instead of
/// rediscovering the same error per layer.
#[test]
fn every_offending_file_is_reported_in_one_run() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    fs::create_dir_all(root.join("managed")).expect("managed directory");
    let stale: Vec<_> = layers()
        .iter()
        .map(|(_, relative)| root.join(relative).join("opencode.json"))
        .collect();
    for path in &stale {
        write(path, r#"{"model":"provider/stale"}"#);
    }

    let error = discover_with(&layer_options(root, &project)).expect_err("all layers are stale");
    let report = error.report();
    for path in &stale {
        assert!(
            report.contains(&path.display().to_string()),
            "one run must name every file to rename; {} is missing from:\n{report}",
            path.display()
        );
    }
}

/// A bare legacy file at a repository root may belong to opencode rather than to
/// Zuno, so that message alone names the switch that stops Zuno reading project
/// config — advice that would be wrong at the global root, where the file was
/// unambiguously written for Zuno.
#[test]
fn only_the_project_ancestor_message_offers_the_project_config_switch() {
    let switch = "ZUNO_DISABLE_PROJECT_CONFIG=1";

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(&project.join("opencode.json"), "{}");
    let ancestor = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect_err("stale project config")
        .report();
    assert!(ancestor.contains(switch), "{ancestor}");

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(&root.join("xdg-config/zuno/opencode.json"), "{}");
    let global = discover_with(&fixture_options(root, &project, std::iter::empty()))
        .expect_err("stale global config")
        .report();
    assert!(
        !global.contains(switch),
        "the global config is unambiguously Zuno's, so disabling project config would \
         not fix it and naming the switch would send the reader the wrong way: {global}"
    );
}

/// Turning project config off must not turn the diagnostic on for files that layer
/// no longer reads — otherwise the documented escape hatch cannot be taken.
#[test]
fn disabling_project_config_also_silences_its_diagnostic() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let project = root.join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    write(
        &project.join("opencode.json"),
        r#"{"model":"provider/stale"}"#,
    );
    write(
        &root.join("xdg-config/zuno/zuno.json"),
        r#"{"model":"provider/live"}"#,
    );

    let config = discover_with(&fixture_options(
        root,
        &project,
        [("ZUNO_DISABLE_PROJECT_CONFIG".to_owned(), "1".to_owned())],
    ))
    .expect("a file in a layer that is switched off is not a problem to report");
    assert_eq!(config.model.as_deref(), Some("provider/live"));
}
