use oc_config::Config;
use oc_config::discovery::{DiscoveryOptions, discover_with};
use oc_config::schema::permission::{PermissionConfig, PermissionRule};
use oc_paths::Env;
use oc_testkit::{ConfigFixture, DivergenceList, Normalizer, Oracle, diff_normalized, divergence};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};

const MINIMUM_TREE_COUNT: usize = 12;

/// The drift-checked capture case derived from the path criterion 2 names.
const REAL_USER_CAPTURE_CASE: &str = "real-user-global-config-capture";
const LIVE_USER_CONFIG: &str = "/config/.config/opencode/opencode.json";

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
    "real-user-config-capture",
    "legacy-tui-keys",
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
        // A committed capture of the live global file, byte-for-byte at test time,
        // legacy `theme` included. The named drift test below makes a changed or
        // absent live file an explicit failure rather than letting this reproducible
        // fixture silently masquerade as the current machine state.
        MatrixCase::config(
            REAL_USER_CAPTURE_CASE,
            base_fixture()?
                .global(&real_user_config())?
                .env_var("OPENCODE_PURE", "1"),
            &["real-user-config-capture", "legacy-tui-keys"],
        ),
    ])
}

fn real_user_config_capture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/user-config.json")
}

fn real_user_config() -> String {
    let path = real_user_config_capture_path();
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn real_user_config_capture_matches_live_file_byte_for_byte() {
    let capture_path = real_user_config_capture_path();
    let live_path = Path::new(LIVE_USER_CONFIG);
    let capture = std::fs::read(&capture_path)
        .unwrap_or_else(|error| panic!("read capture {}: {error}", capture_path.display()));
    let live = std::fs::read(live_path).unwrap_or_else(|error| {
        panic!(
            "read live config {}: {error}; this gate must fail visibly when the machine-specific \
             criterion-2 input is unavailable",
            live_path.display()
        )
    });

    if capture != live {
        let first_difference = capture
            .iter()
            .zip(&live)
            .position(|(captured, current)| captured != current)
            .unwrap_or_else(|| capture.len().min(live.len()));
        panic!(
            "committed real-config capture {} drifted from live config {}: capture={} bytes, \
             live={} bytes, first difference at byte {}. Refresh the capture deliberately and \
             re-run the differential; never call a stale copy the live file.",
            capture_path.display(),
            live_path.display(),
            capture.len(),
            live.len(),
            first_difference
        );
    }
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
        let rust = discover_with(&rust_options)?;
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

/// Success criterion 2's narrowing, pinned so widening it fails here.
///
/// The plan owner narrowed criterion 2 on 2026-08-09 to require byte-identical
/// merged configuration in **pure mode**. A narrowing no test enforces is a
/// waiver, so all three halves of the narrowing are asserted:
///
/// 1. the drift-checked real-user capture the byte-exact matrix runs is *scoped*
///    to pure mode, so silently reinterpreting the criterion as non-pure fails;
/// 2. the excluded non-pure difference is declared in `docs/divergences.toml`
///    with **both** measured tree sizes, so a reader learns what was excluded and
///    a future difference outside those two trees is undeclared rather than
///    absorbed;
/// 3. this port really does still leave `agent` and `command` empty without pure
///    mode — if plugin-tree synthesis ever lands, the declaration describes a
///    difference that no longer exists and must be revisited.
#[test]
fn criterion_2_is_narrowed_to_pure_mode_and_the_non_pure_plugin_trees_are_declared()
-> Result<(), Box<dyn Error>> {
    let cases = matrix()?;
    let real_user = cases
        .iter()
        .find(|case| case.name == REAL_USER_CAPTURE_CASE)
        .ok_or_else(|| {
            format!(
                "the byte-exact matrix no longer contains a case named {REAL_USER_CAPTURE_CASE}; \
                 criterion 2 names the user's own config specifically, so its drift-checked \
                 capture cannot be removed"
            )
        })?;
    assert_eq!(
        real_user
            .fixture
            .env()
            .env_vars()
            .get("OPENCODE_PURE")
            .map(String::as_str),
        Some("1"),
        "criterion 2 was narrowed to pure mode, so case {REAL_USER_CAPTURE_CASE} must force \
         OPENCODE_PURE=1. Without it this test would be asserting a scope the criterion no \
         longer claims, and the declared non-pure divergence below would be covering a \
         comparison that is once again required."
    );
    assert_eq!(
        real_user.oracle_args,
        &["debug", "config"],
        "the narrowed comparison is still `debug config`; a different oracle command would not \
         be the byte-identical merged configuration criterion 2 names"
    );

    let list = DivergenceList::load()?;
    let entry = list
        .find(divergence::NON_PURE_PLUGIN_TREES_ID)
        .ok_or_else(|| {
            format!(
                "{} does not declare {:?}. Criterion 2's non-pure comparison is out of scope \
                 ONLY because that exclusion is declared; without the entry the narrowing is a \
                 waiver.",
                list.path().display(),
                divergence::NON_PURE_PLUGIN_TREES_ID
            )
        })?;
    for (tree, measured) in [
        ("agent", divergence::NON_PURE_AGENT_TREE_BYTES),
        ("command", divergence::NON_PURE_COMMAND_TREE_BYTES),
    ] {
        assert!(
            entry.reason.contains(&measured.to_string()),
            "the {:?} entry must state the measured {tree}-tree size ({measured} bytes). A \
             declaration without the number cannot be contradicted by a later measurement, \
             which is the whole difference between a declared divergence and a shrug.",
            divergence::NON_PURE_PLUGIN_TREES_ID
        );
    }
    assert!(
        entry.surface.contains("OPENCODE_PURE"),
        "the {:?} entry must name the mode it is scoped to; an entry that does not say \
         \"without OPENCODE_PURE\" would read as an unconditional difference",
        divergence::NON_PURE_PLUGIN_TREES_ID
    );

    let non_pure = base_fixture()?.global(&real_user_config())?;
    let merged = discover_with(&options(non_pure.env()))?;
    let agent_entries = merged
        .agent
        .as_ref()
        .map_or(0, |agent| agent.iter().count());
    let command_entries = merged
        .command
        .as_ref()
        .map_or(0, |command| command.iter().count());
    assert_eq!(
        (agent_entries, command_entries),
        (0, 0),
        "without pure mode this port still contributes no plugin-generated agent or command \
         entries, which is exactly what {:?} declares. It now produces {agent_entries} agent and \
         {command_entries} command entries, so the declared difference no longer describes \
         reality — re-measure the oracle and update the entry rather than deleting this \
         assertion.",
        divergence::NON_PURE_PLUGIN_TREES_ID
    );
    eprintln!(
        "criterion 2: case {REAL_USER_CAPTURE_CASE} scoped to OPENCODE_PURE=1; non-pure exclusion \
         declared as {} with {}-byte agent and {}-byte command trees measured on the oracle",
        divergence::NON_PURE_PLUGIN_TREES_ID,
        divergence::NON_PURE_AGENT_TREE_BYTES,
        divergence::NON_PURE_COMMAND_TREE_BYTES
    );
    Ok(())
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
        let rust = discover_with(&rust_options)?;
        let oracle = Oracle::discover()?.with_env(fixture.into_env());
        let outcome = oracle.run(["debug", "config"])?;
        assert!(outcome.is_success(), "{name}: {}", outcome.render());

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
