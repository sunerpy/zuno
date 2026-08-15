use oc_config::discovery::{
    DiscoveryOptions, ManagedPreferences, discover_with, json_error_byte_offset, merge_layers,
};
use oc_config::schema::ordered::OrderedMap;
use oc_config::schema::permission::{PermissionAction, PermissionConfig, PermissionRule};
use oc_config::{Config, DEFAULT_SCHEMA};
use oc_error::ConfigError;
use oc_paths::Env;
use proptest::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

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
fn legacy_config_with_an_empty_zuno_root_returns_an_actionable_typed_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let old_path = temp.path().join("xdg-config/opencode/opencode.json");
    let new_path = temp.path().join("xdg-config/zuno/opencode.json");
    write(&old_path, r#"{"model":"provider/legacy"}"#);

    let result = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()));

    assert!(
        result.is_err(),
        "an old config must not be hidden by generating a fresh Zuno config"
    );
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
    let report = error.report();
    assert!(report.contains(&old_path.display().to_string()), "{report}");
    assert!(report.contains(&new_path.display().to_string()), "{report}");
    assert!(report.contains(copy_command), "{report}");
}

#[test]
fn an_empty_zuno_root_without_legacy_config_still_gets_the_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("a genuinely fresh install gets a default config");

    let generated = temp.path().join("xdg-config/zuno/opencode.jsonc");
    assert_eq!(config.schema.as_deref(), Some(DEFAULT_SCHEMA));
    assert!(generated.is_file());
    assert!(
        fs::read_to_string(generated)
            .expect("read generated config")
            .contains(DEFAULT_SCHEMA)
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
        &temp.path().join("xdg-config/zuno/opencode.json"),
        r#"{"model":"provider/zuno"}"#,
    );

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("the populated Zuno root is authoritative");

    assert_eq!(config.model.as_deref(), Some("provider/zuno"));
}

#[test]
fn explicit_config_overrides_bypass_the_legacy_location_diagnostic() {
    for key in [
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "OPENCODE_CONFIG_CONTENT",
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("project");
        write(
            &temp.path().join("xdg-config/opencode/opencode.json"),
            r#"{"model":"provider/legacy"}"#,
        );
        let override_file = temp.path().join("override/opencode.json");
        let override_dir = temp.path().join("override-dir");
        let value = match key {
            "OPENCODE_CONFIG" => {
                write(&override_file, r#"{"model":"provider/override"}"#);
                override_file.display().to_string()
            }
            "OPENCODE_CONFIG_DIR" => {
                write(
                    &override_dir.join("opencode.json"),
                    r#"{"model":"provider/override"}"#,
                );
                override_dir.display().to_string()
            }
            "OPENCODE_CONFIG_CONTENT" => r#"{"model":"provider/override"}"#.to_owned(),
            _ => unreachable!("the override list is closed"),
        };

        let config = discover_with(&fixture_options(
            temp.path(),
            &project,
            [(oc_paths::env::accepted_env_name(key).to_owned(), value)],
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

    let global = root.join("xdg-config/zuno");
    write(
        &global.join("config.json"),
        r#"{"model":"global-config","instructions":["global-config"]}"#,
    );
    write(
        &global.join("opencode.json"),
        r#"{"model":"global-json","instructions":["global-json"]}"#,
    );
    write(
        &global.join("opencode.jsonc"),
        r#"{"model":"global-jsonc","instructions":["global-jsonc",],}"#,
    );

    let env_file = root.join("env/opencode.json");
    write(
        &env_file,
        r#"{"model":"env-file","instructions":["env-file"]}"#,
    );
    write(
        &project.join("opencode.json"),
        r#"{"model":"project-root","instructions":["project-root"]}"#,
    );
    write(
        &cwd.join("opencode.jsonc"),
        r#"{"model":"project-nearest","instructions":["project-nearest"]}"#,
    );
    write(
        &cwd.join(".zuno/opencode.json"),
        r#"{"model":"dot-nearest","instructions":["dot-nearest"]}"#,
    );
    write(
        &project.join(".zuno/opencode.json"),
        r#"{"model":"dot-root","instructions":["dot-root"]}"#,
    );
    write(
        &root.join("home/.zuno/opencode.json"),
        r#"{"model":"home-dot","instructions":["home-dot"]}"#,
    );
    let override_dir = root.join("override");
    write(
        &override_dir.join("opencode.jsonc"),
        r#"{"model":"config-dir","instructions":["config-dir"]}"#,
    );
    let managed_dir = root.join("managed");
    write(
        &managed_dir.join("opencode.json"),
        r#"{"model":"managed-json","instructions":["managed-json"]}"#,
    );
    write(
        &managed_dir.join("opencode.jsonc"),
        r#"{"model":"managed-jsonc","instructions":["managed-jsonc"]}"#,
    );

    let options = fixture_options(
        root,
        &cwd,
        [
            ("ZUNO_CONFIG".to_owned(), env_file.display().to_string()),
            (
                "OPENCODE_CONFIG_DIR".to_owned(),
                override_dir.display().to_string(),
            ),
            (
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                r#"{"model":"content","instructions":["content","global-json"]}"#.to_owned(),
            ),
            (
                "ZUNO_TEST_MANAGED_CONFIG_DIR".to_owned(),
                managed_dir.display().to_string(),
            ),
        ],
    )
    .with_managed_preferences(ManagedPreferences::new(
        "mobileconfig:/Library/Managed Preferences/ai.opencode.managed.plist",
        r#"{"model":"mac-managed","instructions":["mac-managed","content"]}"#,
    ));

    let config = discover_with(&options).expect("discover all layers");

    assert_eq!(config.model.as_deref(), Some("mac-managed"));
    assert_eq!(
        instructions(&config),
        [
            "global-config",
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
    let valid_path = valid_project.join(".zuno/opencode.jsonc");
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
    let invalid_path = invalid_project.join(".zuno/opencode.jsonc");
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
fn discovery_injects_the_default_schema_into_file_layers() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(project.join(".git")).expect("worktree marker");
    let path = project.join("opencode.json");
    write(&path, r#"{"model":"provider/model"}"#);

    let config = discover_with(&fixture_options(temp.path(), &project, std::iter::empty()))
        .expect("discover config");

    assert_eq!(config.schema.as_deref(), Some(DEFAULT_SCHEMA));
    assert!(
        fs::read_to_string(path)
            .expect("read injected file")
            .contains(DEFAULT_SCHEMA)
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
                "OPENCODE_CONFIG_CONTENT".to_owned(),
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
            oc_config::schema::permission::PermissionObject(
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
