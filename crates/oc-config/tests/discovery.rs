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
        ("OPENCODE_TEST_HOME".to_owned(), home.display().to_string()),
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

    let global = root.join("xdg-config/opencode");
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
        &cwd.join(".opencode/opencode.json"),
        r#"{"model":"dot-nearest","instructions":["dot-nearest"]}"#,
    );
    write(
        &project.join(".opencode/opencode.json"),
        r#"{"model":"dot-root","instructions":["dot-root"]}"#,
    );
    write(
        &root.join("home/.opencode/opencode.json"),
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
            ("OPENCODE_CONFIG".to_owned(), env_file.display().to_string()),
            (
                "OPENCODE_CONFIG_DIR".to_owned(),
                override_dir.display().to_string(),
            ),
            (
                "OPENCODE_CONFIG_CONTENT".to_owned(),
                r#"{"model":"content","instructions":["content","global-json"]}"#.to_owned(),
            ),
            (
                "OPENCODE_TEST_MANAGED_CONFIG_DIR".to_owned(),
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
    let valid_path = valid_project.join(".opencode/opencode.jsonc");
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
    let invalid_path = invalid_project.join(".opencode/opencode.jsonc");
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
                "OPENCODE_PERMISSION".to_owned(),
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
