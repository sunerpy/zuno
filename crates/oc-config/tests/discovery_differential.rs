use oc_config::Config;
use oc_config::discovery::{DiscoveryOptions, discover_with};
use oc_paths::Env;
use oc_testkit::{ConfigFixture, Normalizer, Oracle, diff_normalized};
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::Path;

fn options(env: &oc_testkit::ScriptedEnv) -> DiscoveryOptions {
    DiscoveryOptions::new(
        env.working_dir(),
        Some(env.project()),
        Env::from_pairs(env.env_vars()),
    )
    .with_default_username("unknown")
}

fn canonical_debug_config(text: &str, source: &Path) -> Result<String, Box<dyn Error>> {
    let mut value: Value = serde_json::from_str(text)?;
    let object = value
        .as_object_mut()
        .ok_or("debug config output was not an object")?;
    if let Some(mode) = object.remove("mode")
        && mode != serde_json::json!({})
    {
        return Err(format!("deprecated mode unexpectedly carried data: {mode}").into());
    }
    let config = Config::from_json_value(source, value)?;
    Ok(format!("{}\n", serde_json::to_string_pretty(&config)?))
}

fn base_fixture() -> Result<ConfigFixture, Box<dyn Error>> {
    Ok(ConfigFixture::new()?.mark_worktree_root("")?)
}

#[test]
fn discovery_differential_matches_opencode_debug_config_for_ten_layered_trees()
-> Result<(), Box<dyn Error>> {
    let mut managed = base_fixture()?
        .global(r#"{"model":"provider/global","instructions":["global"]}"#)?
        .env_config_content(r#"{"model":"provider/content","instructions":["content"]}"#);
    let managed_dir = managed.env().root().join("managed");
    fs::create_dir_all(&managed_dir)?;
    fs::write(
        managed_dir.join("opencode.jsonc"),
        r#"{"model":"provider/managed","instructions":["managed",],}"#,
    )?;
    managed = managed.env_var(
        "OPENCODE_TEST_MANAGED_CONFIG_DIR",
        managed_dir.display().to_string(),
    );

    let cases = vec![
        (
            "global-json",
            base_fixture()?.global(r#"{"model":"provider/global","instructions":["global"]}"#)?,
        ),
        (
            "global-jsonc",
            base_fixture()?.global_jsonc(
                "{ // comment\n \"model\": \"provider/jsonc\", \"instructions\": [\"jsonc\",], }",
            )?,
        ),
        (
            "env-file-over-global",
            base_fixture()?
                .global(r#"{"model":"provider/global","instructions":["global"]}"#)?
                .env_config_file(r#"{"model":"provider/env-file","instructions":["env-file"]}"#)?,
        ),
        (
            "ancestor-files",
            base_fixture()?
                .project_file("", r#"{"model":"provider/root","instructions":["root"]}"#)?
                .project_file_jsonc("a", r#"{"model":"provider/a","instructions":["a",],}"#)?
                .working_dir("a/b")?,
        ),
        (
            "dot-opencode-chain",
            base_fixture()?
                .project_dot_opencode(
                    "",
                    r#"{"model":"provider/dot-root","instructions":["dot-root"]}"#,
                )?
                .project_dot_opencode(
                    "a/b",
                    r#"{"model":"provider/dot-near","instructions":["dot-near"]}"#,
                )?
                .working_dir("a/b")?,
        ),
        (
            "home-and-config-dir",
            base_fixture()?
                .home_dot_opencode(r#"{"model":"provider/home","instructions":["home"]}"#)?
                .env_config_dir(
                    r#"{"model":"provider/config-dir","instructions":["config-dir"]}"#,
                )?,
        ),
        (
            "inline-content",
            base_fixture()?
                .project_file(
                    "",
                    r#"{"model":"provider/project","instructions":["project"]}"#,
                )?
                .env_config_content(r#"{"model":"provider/content","instructions":["content"]}"#),
        ),
        ("managed-dir", managed),
        (
            "project-disabled",
            base_fixture()?
                .project_file(
                    "",
                    r#"{"model":"provider/ignored","instructions":["ignored"]}"#,
                )?
                .global(r#"{"model":"provider/global","instructions":["global"]}"#)?
                .disable_project_config(),
        ),
        (
            "instruction-dedup",
            base_fixture()?
                .global(r#"{"model":"provider/global","instructions":["shared","global"]}"#)?
                .env_config_file(r#"{"model":"provider/env","instructions":["env","shared"]}"#)?
                .project_file(
                    "",
                    r#"{"model":"provider/project","instructions":["shared","project"]}"#,
                )?
                .env_config_content(
                    r#"{"model":"provider/content","instructions":["content","env"]}"#,
                ),
        ),
    ];

    assert!(cases.len() >= 8);
    let mut transcripts = Vec::new();
    for (name, fixture) in cases {
        let rust_options = options(fixture.env());
        let rust = discover_with(&rust_options)?;
        let oracle = Oracle::installed_binary()?.with_env(fixture.into_env());
        let outcome = oracle.run(["debug", "config"])?;
        if !outcome.is_success() {
            return Err(format!("case {name} failed to run oracle:\n{}", outcome.render()).into());
        }

        let oracle_json = canonical_debug_config(&outcome.stdout, Path::new("oracle-debug.json"))?;
        let rust_json = format!("{}\n", serde_json::to_string_pretty(&rust)?);
        let report = diff_normalized(
            outcome.label(),
            &oracle_json,
            format!("oc-config discovery case {name}"),
            &rust_json,
            &Normalizer::none(),
        );
        if !report.is_identical() {
            return Err(format!("case {name}:\n{}", report.render()).into());
        }
        transcripts.push(format!("{name}: {}\n{}", outcome.label(), report.render()));
    }

    assert_eq!(transcripts.len(), 10);
    eprintln!("{}", transcripts.join("\n"));
    Ok(())
}
