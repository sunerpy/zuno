use oc_config::Config;
use oc_config::discovery::{DiscoveryOptions, discover_with};
use oc_config::schema::permission::{PermissionConfig, PermissionRule};
use oc_paths::Env;
use oc_testkit::{ConfigFixture, Normalizer, Oracle, diff_normalized};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;

const MINIMUM_TREE_COUNT: usize = 12;

const REQUIRED_COVERAGE: &[&str] = &[
    "global-only",
    "project-only",
    "global-and-project",
    ".opencode-chain",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_PERMISSION",
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_PURE",
    "--pure",
    "jsonc-comments",
    "deep-ancestor-walk",
];

const INTENTIONAL_DIVERGENCES: &[(&str, &str)] = &[];

struct MatrixCase {
    name: &'static str,
    fixture: ConfigFixture,
    oracle_args: &'static [&'static str],
    coverage: &'static [&'static str],
}

#[derive(Deserialize)]
struct OracleDebugConfig {
    mode: Option<Value>,
    #[serde(flatten)]
    config: Config,
}

impl MatrixCase {
    fn config(
        name: &'static str,
        fixture: ConfigFixture,
        coverage: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            fixture,
            oracle_args: &["debug", "config"],
            coverage,
        }
    }

    fn pure_flag(
        name: &'static str,
        fixture: ConfigFixture,
        coverage: &'static [&'static str],
    ) -> Self {
        Self {
            name,
            fixture,
            oracle_args: &["--pure", "debug", "config"],
            coverage,
        }
    }
}

fn options(env: &oc_testkit::ScriptedEnv) -> DiscoveryOptions {
    DiscoveryOptions::new(
        env.working_dir(),
        Some(env.project()),
        Env::from_pairs(env.env_vars()),
    )
    .with_default_username("unknown")
}

fn canonical_debug_config(text: &str, _source: &Path) -> Result<String, Box<dyn Error>> {
    let debug: OracleDebugConfig = serde_json::from_str(text)?;
    if let Some(mode) = debug.mode
        && mode != serde_json::json!({})
    {
        return Err(format!("deprecated mode unexpectedly carried data: {mode}").into());
    }
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&debug.config)?
    ))
}

fn base_fixture() -> Result<ConfigFixture, Box<dyn Error>> {
    Ok(ConfigFixture::new()?.mark_worktree_root("")?)
}

fn matrix() -> Result<Vec<MatrixCase>, Box<dyn Error>> {
    Ok(vec![
        MatrixCase::config(
            "global-only",
            base_fixture()?.global(
                r#"{"model":"matrix/global","instructions":["global-only"]}"#,
            )?,
            &["global-only"],
        ),
        MatrixCase::config(
            "project-only",
            base_fixture()?.project_file(
                "",
                r#"{"model":"matrix/project","instructions":["project-only"]}"#,
            )?,
            &["project-only"],
        ),
        MatrixCase::config(
            "global-and-project",
            base_fixture()?
                .global(
                    r#"{"model":"matrix/global","small_model":"matrix/global-small","instructions":["global","shared"]}"#,
                )?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project","shared"]}"#,
                )?,
            &["global-and-project"],
        ),
        MatrixCase::config(
            "dot-opencode-chain",
            base_fixture()?
                .project_dot_opencode(
                    "",
                    r#"{"model":"matrix/dot-root","instructions":["dot-root"]}"#,
                )?
                .project_dot_opencode(
                    "team",
                    r#"{"model":"matrix/dot-team","instructions":["dot-team"]}"#,
                )?
                .project_dot_opencode(
                    "team/service",
                    r#"{"model":"matrix/dot-service","instructions":["dot-service"]}"#,
                )?
                .working_dir("team/service")?,
            &[".opencode-chain"],
        ),
        MatrixCase::config(
            "env-config-file-before-project",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .env_config_file(
                    r#"{"model":"matrix/env-file","instructions":["env-file"]}"#,
                )?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project"]}"#,
                )?,
            &["OPENCODE_CONFIG"],
        ),
        MatrixCase::config(
            "env-config-content-after-project",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project"]}"#,
                )?
                .env_config_content(
                    r#"{"model":"matrix/content","instructions":["content"]}"#,
                ),
            &["OPENCODE_CONFIG_CONTENT"],
        ),
        MatrixCase::config(
            "home-and-env-config-dir",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .home_dot_opencode(
                    r#"{"model":"matrix/home","instructions":["home"]}"#,
                )?
                .env_config_dir(
                    r#"{"model":"matrix/config-dir","instructions":["config-dir"]}"#,
                )?,
            &["OPENCODE_CONFIG_DIR"],
        ),
        MatrixCase::config(
            "permission-env-object",
            base_fixture()?
                .global(r#"{"model":"matrix/permission"}"#)?
                .env_var(
                    "OPENCODE_PERMISSION",
                    r#"{"bash":"allow","*":"ask","edit":"deny"}"#,
                ),
            &["OPENCODE_PERMISSION"],
        ),
        MatrixCase::config(
            "project-disabled-uppercase-true",
            base_fixture()?
                .global(
                    r#"{"model":"matrix/global","instructions":["global-visible"]}"#,
                )?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project-hidden"]}"#,
                )?
                .env_var("OPENCODE_DISABLE_PROJECT_CONFIG", "TRUE"),
            &["OPENCODE_DISABLE_PROJECT_CONFIG"],
        ),
        MatrixCase::config(
            "pure-env-keeps-config-layers",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project"]}"#,
                )?
                .env_var("OPENCODE_PURE", "true"),
            &["OPENCODE_PURE"],
        ),
        MatrixCase::pure_flag(
            "pure-cli-flag-keeps-config-layers",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project"]}"#,
                )?,
            &["--pure"],
        ),
        MatrixCase::config(
            "jsonc-comments-and-trailing-commas",
            base_fixture()?.global_jsonc(
                "{\n  // line comment\n  \"model\": \"matrix/jsonc\",\n  /* block comment */\n  \"instructions\": [\"jsonc\",],\n}\n",
            )?,
            &["jsonc-comments"],
        ),
        MatrixCase::config(
            "deep-ancestor-walk-with-env-file",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .env_config_file(
                    r#"{"model":"matrix/env-file","instructions":["env-file"]}"#,
                )?
                .project_file(
                    "",
                    r#"{"model":"matrix/root","instructions":["root"]}"#,
                )?
                .project_file(
                    "a",
                    r#"{"model":"matrix/a","instructions":["a"]}"#,
                )?
                .project_file_jsonc(
                    "a/b/c",
                    r#"{"model":"matrix/abc","instructions":["abc",],}"#,
                )?
                .project_file(
                    "a/b/c/d",
                    r#"{"model":"matrix/abcd","instructions":["abcd"]}"#,
                )?
                .working_dir("a/b/c/d/e/f")?,
            &["deep-ancestor-walk", "OPENCODE_CONFIG"],
        ),
        MatrixCase::config(
            "all-config-env-layers",
            base_fixture()?
                .global(r#"{"model":"matrix/global","instructions":["global"]}"#)?
                .env_config_file(
                    r#"{"model":"matrix/env-file","instructions":["env-file"]}"#,
                )?
                .project_file(
                    "",
                    r#"{"model":"matrix/project","instructions":["project"]}"#,
                )?
                .project_dot_opencode(
                    "",
                    r#"{"model":"matrix/dot","instructions":["dot"]}"#,
                )?
                .env_config_dir(
                    r#"{"model":"matrix/config-dir","instructions":["config-dir"]}"#,
                )?
                .env_config_content(
                    r#"{"model":"matrix/content","instructions":["content"]}"#,
                )
                .env_var("OPENCODE_PERMISSION", r#"{"read":"allow"}"#)
                .env_var("OPENCODE_PURE", "1"),
            &[
                "OPENCODE_CONFIG",
                "OPENCODE_CONFIG_CONTENT",
                "OPENCODE_CONFIG_DIR",
                "OPENCODE_PERMISSION",
                "OPENCODE_PURE",
            ],
        ),
    ])
}

#[test]
fn merged_config_matches_real_opencode_across_the_full_matrix() -> Result<(), Box<dyn Error>> {
    for (case, reason) in INTENTIONAL_DIVERGENCES {
        assert!(
            !reason.trim().is_empty(),
            "intentional divergence {case} needs a one-line reason"
        );
    }

    let cases = matrix()?;
    assert!(
        cases.len() >= MINIMUM_TREE_COUNT,
        "matrix has {} trees, expected at least {MINIMUM_TREE_COUNT}",
        cases.len()
    );

    let covered = cases
        .iter()
        .flat_map(|case| case.coverage.iter().copied())
        .collect::<BTreeSet<_>>();
    let missing = REQUIRED_COVERAGE
        .iter()
        .copied()
        .filter(|required| !covered.contains(required))
        .collect::<Vec<_>>();
    assert!(missing.is_empty(), "matrix coverage missing: {missing:?}");

    let tree_count = cases.len();
    let mut transcripts = Vec::with_capacity(tree_count);
    let mut failures = Vec::new();
    for case in cases {
        let rust_options = options(case.fixture.env());
        let oracle = Oracle::discover()?.with_env(case.fixture.into_env());
        let outcome = oracle.run(case.oracle_args.iter().copied())?;
        if !outcome.is_success() {
            failures.push(format!(
                "case {} failed to run oracle:\n{}",
                case.name,
                outcome.render()
            ));
            continue;
        }

        let rust = discover_with(&rust_options)?;
        let oracle_json = canonical_debug_config(&outcome.stdout, Path::new("oracle-debug.json"))?;
        let rust_json = format!("{}\n", serde_json::to_string_pretty(&rust)?);
        let report = diff_normalized(
            outcome.label(),
            &oracle_json,
            format!("oc-config differential case {}", case.name),
            &rust_json,
            &Normalizer::none(),
        );
        if report.is_identical() {
            transcripts.push(format!(
                "{}: {}\n{}",
                case.name,
                outcome.label(),
                report.render()
            ));
        } else {
            failures.push(format!("case {}:\n{}", case.name, report.render()));
        }
    }

    eprintln!(
        "config differential matrix: {tree_count} trees\n{}",
        transcripts.join("\n")
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} of {tree_count} config trees diverged:\n{}",
            failures.len(),
            failures.join("\n")
        )
        .into())
    }
}

#[test]
fn permission_env_object_key_order_matches_raw_oracle() -> Result<(), Box<dyn Error>> {
    let cases = vec![
        (
            "single-new-key",
            base_fixture()?.env_var("OPENCODE_PERMISSION", r#"{"edit":"allow"}"#),
            &["edit"][..],
            None,
        ),
        (
            "four-new-keys",
            base_fixture()?.env_var(
                "OPENCODE_PERMISSION",
                r#"{"read":"allow","bash":"deny","*":"ask","edit":"allow"}"#,
            ),
            &["read", "bash", "*", "edit"][..],
            None,
        ),
        (
            "existing-and-nested-keys",
            base_fixture()?
                .global(
                    r#"{"permission":{"existing":"ask","bash":{"git *":"ask","keep":"deny"},"read":"deny"}}"#,
                )?
                .env_var(
                    "OPENCODE_PERMISSION",
                    r#"{"read":"allow","bash":{"git *":"allow","*":"deny","foo":"ask"},"*":"deny","edit":"allow","deploy":"ask"}"#,
                ),
            &["existing", "bash", "read", "*", "edit", "deploy"][..],
            Some(("bash", &["git *", "keep", "*", "foo"][..])),
        ),
    ];

    for (name, fixture, expected_outer, expected_nested) in cases {
        let rust_options = options(fixture.env());
        let oracle = Oracle::discover()?.with_env(fixture.into_env());
        let outcome = oracle.run(["debug", "config"])?;
        assert!(outcome.is_success(), "{name}: {}", outcome.render());

        let rust = discover_with(&rust_options)?;
        let oracle_json = canonical_debug_config(&outcome.stdout, Path::new("oracle-debug.json"))?;
        let rust_json = format!("{}\n", serde_json::to_string_pretty(&rust)?);
        let report = diff_normalized(
            outcome.label(),
            &oracle_json,
            format!("oc-config OPENCODE_PERMISSION order case {name}"),
            &rust_json,
            &Normalizer::none(),
        );
        // remeda's mergeDeep retains existing key positions and appends new keys
        // in source order. The permission engine preserves that order, and its
        // findLast evaluation makes the last overlapping wildcard rule win.
        assert!(report.is_identical(), "{}", report.render());

        let Some(PermissionConfig::Object(permission)) = rust.permission.as_ref() else {
            return Err(format!("{name}: Rust omitted the permission object").into());
        };
        let outer = permission.iter().map(|(key, _)| key).collect::<Vec<_>>();
        assert_eq!(outer, expected_outer, "{name}: wrong outer key order");
        if let Some((permission_name, expected_patterns)) = expected_nested {
            let configured = permission
                .iter()
                .find(|(key, _)| *key == permission_name)
                .map(|(_, configured)| configured)
                .ok_or_else(|| format!("{name}: missing nested {permission_name} rule"))?;
            let PermissionRule::Patterns(patterns) = configured else {
                return Err(format!("{name}: {permission_name} was not a pattern object").into());
            };
            let nested = patterns.iter().map(|(key, _)| key).collect::<Vec<_>>();
            assert_eq!(nested, expected_patterns, "{name}: wrong nested key order");
        }
        eprintln!("{name}: {}\n{}", outcome.label(), report.render());
    }
    Ok(())
}

#[test]
fn pure_env_and_cli_flag_disable_external_plugins_without_suppressing_config()
-> Result<(), Box<dyn Error>> {
    let cases = [
        (
            "OPENCODE_PURE=true",
            base_fixture()?.env_var("OPENCODE_PURE", "true"),
            &["debug", "info"][..],
        ),
        ("--pure", base_fixture()?, &["--pure", "debug", "info"][..]),
    ];

    for (name, fixture, args) in cases {
        let oracle = Oracle::discover()?.with_env(fixture.into_env());
        let outcome = oracle.run(args.iter().copied())?;
        assert!(
            outcome.is_success(),
            "{name} failed to run oracle:\n{}",
            outcome.render()
        );
        assert!(
            outcome
                .stdout
                .contains("external plugins disabled (--pure)"),
            "{name} did not activate pure mode:\n{}",
            outcome.render()
        );
        eprintln!("{name}: {}\n{}", outcome.label(), outcome.render());
    }
    Ok(())
}
