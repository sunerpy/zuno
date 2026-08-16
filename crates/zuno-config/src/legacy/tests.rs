//! One test per deprecated form, each asserting the message names both the
//! deprecated input and its replacement — because a message that names only the
//! problem leaves the author exactly as stuck as silence would.

use super::*;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

/// The single finding a fixture is built to produce.
#[track_caller]
fn only(found: Vec<Deprecation>) -> Deprecation {
    assert_eq!(
        found.len(),
        1,
        "expected exactly one finding, got {found:?}"
    );
    found.into_iter().next().expect("length was just checked")
}

#[track_caller]
fn assert_names(deprecation: &Deprecation, deprecated: &str, replacement: &str) {
    let message = deprecation.message();
    assert!(
        message.contains(deprecated),
        "message must name the deprecated input {deprecated:?}: {message}"
    );
    assert!(
        message.contains(replacement),
        "message must name the replacement {replacement:?}: {message}"
    );
}

fn config_path() -> PathBuf {
    PathBuf::from("/repo/opencode.json")
}

// ---------------------------------------------------------------------------
// 1. `mode.<name>` -> `agent.<name>` with `mode: "primary"`
// ---------------------------------------------------------------------------

#[test]
fn mode_block_names_the_agent_replacement() {
    let path = config_path();
    let found = only(inspect_config(
        &path,
        &json!({ "mode": { "build": { "model": "anthropic/claude" } } }),
    ));
    assert_eq!(found.form(), DeprecatedForm::ModeBlock);
    assert_eq!(found.pointer(), ["mode", "build"]);
    assert_eq!(found.path(), path);
    assert_names(&found, "mode.build", "agent.build");
    assert!(
        found.message().contains("mode: \"primary\""),
        "{}",
        found.message()
    );
}

#[test]
fn an_empty_mode_block_still_reports_the_replacement() {
    let path = config_path();
    let found = only(inspect_config(&path, &json!({ "mode": {} })));
    assert_eq!(found.pointer(), ["mode"]);
    assert_names(&found, "mode", "agent");
    assert!(found.message().contains("mode: \"primary\""));
}

// ---------------------------------------------------------------------------
// 2. a `{mode,modes}/` agent directory -> `agent/`
// ---------------------------------------------------------------------------

#[test]
fn mode_directory_names_the_agent_directory() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("mode")).expect("mkdir mode");
    fs::write(dir.path().join("mode/build.md"), "# build").expect("write");
    let found = only(inspect_directory(dir.path()));
    assert_eq!(found.form(), DeprecatedForm::ModeDirectory);
    assert_eq!(found.path(), dir.path().join("mode/build.md"));
    assert!(found.pointer().is_empty(), "a file has no JSON pointer");
    assert_names(&found, "mode/build.md", "agent/build.md");
}

#[test]
fn the_plural_modes_directory_is_rejected_too() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("modes")).expect("mkdir modes");
    fs::write(dir.path().join("modes/plan.md"), "# plan").expect("write");
    let found = only(inspect_directory(dir.path()));
    assert_names(&found, "modes/plan.md", "agent/plan.md");
}

#[test]
fn an_empty_mode_directory_is_not_reported() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("mode")).expect("mkdir mode");
    fs::write(dir.path().join("mode/notes.txt"), "not markdown").expect("write");
    assert_eq!(
        inspect_directory(dir.path()),
        Vec::new(),
        "the oracle's `{{mode,modes}}/*.md` glob loads nothing here, so neither does this"
    );
}

// ---------------------------------------------------------------------------
// 3. agent `tools` -> `permission`
// ---------------------------------------------------------------------------

#[test]
fn agent_tools_names_permission() {
    let path = config_path();
    let found = only(inspect_config(
        &path,
        &json!({ "agent": { "build": { "tools": { "write": false } } } }),
    ));
    assert_eq!(found.form(), DeprecatedForm::AgentTools);
    assert_eq!(found.pointer(), ["agent", "build", "tools"]);
    assert_names(&found, "agent.build.tools", "permission");
    assert!(
        found.message().contains("permission.edit"),
        "the write/edit/patch collapse is the part a translation gets wrong: {}",
        found.message()
    );
}

#[test]
fn agent_frontmatter_tools_are_rejected_with_a_definition_relative_pointer() {
    let path = PathBuf::from("/repo/.opencode/agent/build.md");
    let found = only(inspect_agent_frontmatter(
        &path,
        &json!({ "tools": { "bash": true } }),
    ));
    assert_eq!(found.pointer(), ["tools"]);
    assert_eq!(found.path(), path);
    assert_names(&found, "tools", "permission");
}

// ---------------------------------------------------------------------------
// 4. agent `maxSteps` -> `steps`
// ---------------------------------------------------------------------------

#[test]
fn agent_max_steps_names_steps() {
    let path = config_path();
    let found = only(inspect_config(
        &path,
        &json!({ "agent": { "build": { "maxSteps": 40 } } }),
    ));
    assert_eq!(found.form(), DeprecatedForm::AgentMaxSteps);
    assert_eq!(found.pointer(), ["agent", "build", "maxSteps"]);
    assert_names(&found, "agent.build.maxSteps", "`steps`");
}

// ---------------------------------------------------------------------------
// 5. `layout` -> removed
// ---------------------------------------------------------------------------

#[test]
fn layout_is_reported_as_removed() {
    let path = config_path();
    let found = only(inspect_config(&path, &json!({ "layout": "auto" })));
    assert_eq!(found.form(), DeprecatedForm::Layout);
    assert_eq!(found.pointer(), ["layout"]);
    assert_names(&found, "layout", "removed");
}

// ---------------------------------------------------------------------------
// 6. `autoshare` -> `share`
// ---------------------------------------------------------------------------

#[test]
fn autoshare_names_share() {
    let path = config_path();
    let found = only(inspect_config(&path, &json!({ "autoshare": true })));
    assert_eq!(found.form(), DeprecatedForm::Autoshare);
    assert_eq!(found.pointer(), ["autoshare"]);
    assert_names(&found, "autoshare", "`share`");
    assert!(
        found.message().contains("share: \"auto\""),
        "{}",
        found.message()
    );
}

// ---------------------------------------------------------------------------
// 7. `CONTEXT.md` -> `AGENTS.md`
// ---------------------------------------------------------------------------

#[test]
fn context_file_names_agents_md() {
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("CONTEXT.md"), "old instructions").expect("write");
    let found = only(inspect_directory(dir.path()));
    assert_eq!(found.form(), DeprecatedForm::ContextFile);
    assert_eq!(found.path(), dir.path().join("CONTEXT.md"));
    assert_names(&found, "CONTEXT.md", "AGENTS.md");
}

// ---------------------------------------------------------------------------
// 8. a global TOML `config` file -> `config.json`
// ---------------------------------------------------------------------------

#[test]
fn toml_config_names_config_json() {
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("config"), "provider = \"anthropic\"\n").expect("write");
    let found = only(inspect_global_directory(dir.path()));
    assert_eq!(found.form(), DeprecatedForm::TomlConfig);
    assert_eq!(found.path(), dir.path().join("config"));
    assert_names(&found, "`config`", "config.json");
    assert!(
        found.message().contains("TOML"),
        "the extensionless `config` is only recognizable as TOML if the message says so: {}",
        found.message()
    );
}

#[test]
fn the_toml_config_file_is_never_rewritten_or_removed() {
    let dir = TempDir::new().expect("tempdir");
    let toml = dir.path().join("config");
    fs::write(&toml, "provider = \"anthropic\"\n").expect("write");
    let error = check_global_directory(dir.path()).expect_err("must be rejected");
    assert!(matches!(error, ConfigError::Invalid { .. }));
    assert_eq!(
        fs::read_to_string(&toml).expect("still there"),
        "provider = \"anthropic\"\n",
        "the oracle migrates and unlinks; this pass reports and leaves the file alone"
    );
    assert!(
        !dir.path().join("config.json").exists(),
        "no migration output may be written"
    );
}

#[test]
fn a_project_directory_is_not_searched_for_the_toml_config_file() {
    let dir = TempDir::new().expect("tempdir");
    fs::write(dir.path().join("config"), "provider = \"anthropic\"\n").expect("write");
    assert_eq!(
        inspect_directory(dir.path()),
        Vec::new(),
        "`config/config.ts:262` looks under the global config directory only"
    );
}

// ---------------------------------------------------------------------------
// 9. `reference` -> `references`
// ---------------------------------------------------------------------------

#[test]
fn reference_names_references() {
    let path = config_path();
    let found = only(inspect_config(
        &path,
        &json!({ "reference": { "docs": "./docs" } }),
    ));
    assert_eq!(found.form(), DeprecatedForm::Reference);
    assert_eq!(found.pointer(), ["reference"]);
    assert_names(&found, "`reference`", "`references`");
}

// ---------------------------------------------------------------------------
// 10. auth-prompt `condition` -> `when`
// ---------------------------------------------------------------------------

#[test]
fn auth_prompt_condition_names_when() {
    let path = PathBuf::from("/repo/.opencode/plugin/acme.json");
    let found = only(inspect_auth(
        &path,
        &json!({
            "methods": [{
                "type": "oauth",
                "prompts": [{ "type": "text", "key": "token", "condition": true }],
            }],
        }),
    ));
    assert_eq!(found.form(), DeprecatedForm::AuthPromptCondition);
    assert_eq!(
        found.pointer(),
        ["methods", "0", "prompts", "0", "condition"]
    );
    assert_names(&found, "prompts.0.condition", "`when`");
}

#[test]
fn a_prompt_using_when_is_accepted() {
    let path = PathBuf::from("/repo/.opencode/plugin/acme.json");
    assert_eq!(
        inspect_auth(
            &path,
            &json!({
                "methods": [{
                    "prompts": [{ "type": "text", "key": "token", "when": { "equals": "a" } }],
                }],
            }),
        ),
        Vec::new()
    );
}

#[test]
fn a_condition_outside_a_prompts_array_is_not_an_auth_prompt() {
    let path = PathBuf::from("/repo/.opencode/plugin/acme.json");
    assert_eq!(
        inspect_auth(&path, &json!({ "rules": [{ "condition": "always" }] })),
        Vec::new(),
        "`condition` is only deprecated on an auth prompt descriptor"
    );
}

// ---------------------------------------------------------------------------
// Modern forms cost nothing
// ---------------------------------------------------------------------------

#[test]
fn a_config_of_only_modern_forms_has_no_deprecations() {
    let path = config_path();
    let modern = json!({
        "$schema": "https://opencode.ai/config.json",
        "share": "manual",
        "references": { "docs": "./docs" },
        "tools": { "bash": true },
        "permission": { "edit": "ask" },
        "agent": {
            "build": {
                "mode": "primary",
                "steps": 40,
                "permission": { "edit": "allow" },
                "tools_note": "not the deprecated `tools` key",
            },
        },
        "instructions": ["AGENTS.md"],
    });
    assert_eq!(inspect_config(&path, &modern), Vec::new());
    assert!(check_config(&path, &modern).is_ok());
}

#[test]
fn a_modern_config_directory_has_no_deprecations() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("agent")).expect("mkdir agent");
    fs::write(dir.path().join("agent/build.md"), "# build").expect("write");
    fs::write(dir.path().join("AGENTS.md"), "instructions").expect("write");
    fs::write(dir.path().join("config.json"), "{}").expect("write");
    assert_eq!(inspect_directory(dir.path()), Vec::new());
    assert_eq!(inspect_global_directory(dir.path()), Vec::new());
    assert!(check_global_directory(dir.path()).is_ok());
}

#[test]
fn a_document_that_is_not_an_object_has_no_deprecations() {
    let path = config_path();
    assert_eq!(inspect_config(&path, &json!([1, 2, 3])), Vec::new());
    assert_eq!(inspect_agent_frontmatter(&path, &json!("text")), Vec::new());
}

// ---------------------------------------------------------------------------
// Location precision and reporting shape
// ---------------------------------------------------------------------------

#[test]
fn a_mode_entry_is_also_scanned_for_agent_level_forms() {
    let path = config_path();
    let found = inspect_config(
        &path,
        &json!({ "mode": { "build": { "maxSteps": 40, "tools": { "write": false } } } }),
    );
    let pointers: Vec<String> = found.iter().map(|f| f.found().to_owned()).collect();
    assert_eq!(
        pointers,
        vec![
            "mode.build".to_owned(),
            "mode.build.tools".to_owned(),
            "mode.build.maxSteps".to_owned(),
        ],
        "the oracle spreads a mode entry into agent verbatim, so the inner keys are deprecated too"
    );
}

#[test]
fn every_finding_becomes_one_issue_keeping_its_own_pointer() {
    let path = config_path();
    let error = check_config(
        &path,
        &json!({
            "layout": "auto",
            "autoshare": true,
            "reference": {},
            "agent": { "build": { "maxSteps": 1 } },
        }),
    )
    .expect_err("must be rejected");
    let ConfigError::Invalid { path: at, issues } = error else {
        panic!("expected Invalid");
    };
    assert_eq!(at, path);
    let pointers: Vec<String> = issues.iter().map(|i| i.key_path.join(".")).collect();
    assert_eq!(
        pointers,
        vec![
            "layout".to_owned(),
            "autoshare".to_owned(),
            "reference".to_owned(),
            "agent.build.maxSteps".to_owned(),
        ]
    );
    for issue in &issues {
        assert!(
            issue.detail.contains("/repo/opencode.json"),
            "every issue names its own file: {}",
            issue.detail
        );
    }
}

#[test]
fn a_directory_finding_names_the_offending_file_not_just_the_scanned_root() {
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("mode")).expect("mkdir mode");
    fs::write(dir.path().join("mode/build.md"), "# build").expect("write");
    let error = check_directory(dir.path()).expect_err("must be rejected");
    let ConfigError::Invalid { path: at, issues } = error else {
        panic!("expected Invalid");
    };
    assert_eq!(at, dir.path(), "the scanned root");
    let file = dir.path().join("mode/build.md");
    assert!(
        issues[0].detail.contains(&file.display().to_string()),
        "the issue must name the exact file: {}",
        issues[0].detail
    );
}

#[test]
fn a_single_finding_can_become_an_error_on_its_own() {
    let path = config_path();
    let found = only(inspect_config(&path, &json!({ "autoshare": false })));
    let message = found.message();
    let error = found.into_error();
    let ConfigError::Invalid { path: at, issues } = error else {
        panic!("expected Invalid");
    };
    assert_eq!(at, path);
    assert_eq!(issues, vec![ConfigIssue::new(["autoshare"], message)]);
}

#[test]
fn the_legacy_pass_is_what_makes_the_schemas_rejection_actionable() {
    let path = config_path();
    let value = json!({ "mode": { "build": {} } });

    let schema_error = crate::Config::from_json_value(&path, value.clone())
        .expect_err("the schema does not name `mode`");
    let ConfigError::Invalid { issues, .. } = &schema_error else {
        panic!("expected Invalid");
    };
    assert_eq!(issues[0].detail, "unrecognized key");
    assert!(
        !issues[0].detail.contains("agent"),
        "correct, but it does not say what to write instead"
    );

    let legacy_error = check_config(&path, &value).expect_err("must be rejected");
    let ConfigError::Invalid { issues, .. } = &legacy_error else {
        panic!("expected Invalid");
    };
    assert!(
        issues[0].detail.contains("agent.build"),
        "running this pass first is the whole point: {}",
        issues[0].detail
    );
}

#[test]
fn all_ten_deprecated_forms_are_reachable() {
    let path = config_path();
    let dir = TempDir::new().expect("tempdir");
    fs::create_dir(dir.path().join("modes")).expect("mkdir modes");
    fs::write(dir.path().join("modes/plan.md"), "# plan").expect("write");
    fs::write(dir.path().join("CONTEXT.md"), "old").expect("write");
    fs::write(dir.path().join("config"), "model = \"x\"\n").expect("write");

    let mut forms: Vec<DeprecatedForm> = inspect_config(
        &path,
        &json!({
            "mode": { "build": {} },
            "layout": "auto",
            "autoshare": true,
            "reference": {},
            "agent": { "build": { "tools": {}, "maxSteps": 1 } },
        }),
    )
    .into_iter()
    .chain(inspect_auth(
        &path,
        &json!({ "prompts": [{ "condition": true }] }),
    ))
    .chain(inspect_global_directory(dir.path()))
    .map(|found| found.form())
    .collect();
    forms.sort_by_key(|form| format!("{form:?}"));
    forms.dedup();
    assert_eq!(forms.len(), 10, "ten forms, all reachable: {forms:?}");
}
