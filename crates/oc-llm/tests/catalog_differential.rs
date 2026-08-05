//! The differential: does this crate's model list equal the real binary's?
//!
//! # What is compared, and why not `--format json`
//!
//! Todo 26's acceptance criterion names `opencode models --format json`. **That
//! flag does not exist on 1.18.12.** `opencode models --help` lists exactly
//! `--verbose` and `--refresh`, and `models --format json` prints the help text
//! and exits without listing anything. The plan is wrong on this point; the
//! recorded output of `--help` is in the evidence file.
//!
//! What the criterion *means* is the model list, and `models.ts:36-47` is where it
//! comes from: one `provider/model` line per model, provider ids ordered
//! `opencode`-first then by `localeCompare`, model ids by `localeCompare` within
//! each provider. That is exactly what [`Catalog::model_lines`] produces, so the
//! comparison here is line-for-line against the real binary's stdout.
//!
//! `--verbose` is deliberately not the target. Its JSON key order differs between
//! a catalog-derived model and a config-derived one, because the oracle builds the
//! two by different code paths — a spread-merge versus an object literal. Diffing
//! it would fail on key order and say nothing about whether the right models
//! resolved.
//!
//! # No network, on either side
//!
//! Both sides read the same pinned fixture. The oracle gets it through
//! `OPENCODE_MODELS_PATH`; this crate gets it through `include_str!`. Combined with
//! `OPENCODE_DISABLE_MODELS_FETCH=1` (which `ScriptedEnv` sets by default) the
//! oracle cannot reach models.dev even if the path were ignored, so a passing run
//! proves parity rather than proving both processes fetched the same live document.
//!
//! # When the oracle is absent
//!
//! Each test skips with a printed reason rather than failing when no oracle binary
//! can be found — a machine without the real CLI installed must still be able to
//! run `cargo test`. Every skip prints, so a silently-skipping suite is visible in
//! the output rather than looking like a pass.

use std::collections::BTreeMap;

use oc_config::schema::Config;
use oc_llm::catalog::models_dev::CatalogDocument;
use oc_llm::catalog::{Catalog, ResolveInput};
use oc_testkit::{Normalizer, Oracle, ScriptedEnv, diff_normalized};

const PINNED: &str = include_str!("fixtures/models-dev-pinned.json");

/// The absolute path of the installed 1.18.12 binary, when it is where mise put it.
///
/// `Oracle::discover()` is tried first; this is a fallback because the bare
/// `opencode` shim on `PATH` fails under a cleared environment on this host.
const MISE_ORACLE: &str = "/config/.local/share/mise/installs/opencode/1.18.12/opencode";

/// An oracle, or `None` with a printed reason.
fn oracle() -> Option<Oracle> {
    match Oracle::discover() {
        Ok(found) => Some(found),
        Err(discover_error) => match Oracle::at_binary(MISE_ORACLE) {
            Ok(found) => Some(found),
            Err(fallback_error) => {
                println!(
                    "SKIP: no oracle binary available.\n  discover: {discover_error}\n  \
                     fallback {MISE_ORACLE}: {fallback_error}"
                );
                None
            }
        },
    }
}

fn document() -> CatalogDocument {
    serde_json::from_str(PINNED).expect("the pinned fixture parses")
}

/// Run `opencode models` against the pinned fixture and return its stdout plus
/// the version that produced it.
///
/// Takes the oracle by value because [`Oracle::with_env`] consumes it, so every
/// scenario constructs its own. That also guarantees no scripted temp directory is
/// shared between scenarios.
fn oracle_models(
    oracle: Oracle,
    config_json: Option<&str>,
    extra: &[(&str, &str)],
) -> Option<(String, String)> {
    let env = ScriptedEnv::new().expect("scripted env");
    let fixture = env.root().join("models-dev-pinned.json");
    std::fs::write(&fixture, PINNED).expect("write the pinned fixture");
    if let Some(config_json) = config_json {
        std::fs::write(env.project().join("opencode.json"), config_json).expect("write config");
    }
    let mut env = env
        .set("OPENCODE_MODELS_PATH", fixture.to_string_lossy())
        // Redundant with the ScriptedEnv default, restated so a change there
        // cannot quietly let this test reach the network.
        .set("OPENCODE_DISABLE_MODELS_FETCH", "1");
    for (key, value) in extra {
        env = env.set(*key, *value);
    }

    let version = oracle.reported_version().to_owned();
    match oracle.with_env(env).run(["models"]) {
        Ok(outcome) if outcome.is_success() => Some((outcome.stdout, version)),
        Ok(outcome) => {
            println!(
                "SKIP: the oracle exited {:?}\nstdout:\n{}\nstderr:\n{}",
                outcome.exit_code, outcome.stdout, outcome.stderr
            );
            None
        }
        Err(error) => {
            println!("SKIP: the oracle could not be run: {error}");
            None
        }
    }
}

/// The lines this crate produces for the same inputs.
fn rust_models(config_json: Option<&str>, env: BTreeMap<String, String>) -> String {
    let config: Option<Config> =
        config_json.map(|json| serde_json::from_str(json).expect("config parses"));
    let mut input = ResolveInput::new().with_env(env);
    if let Some(config) = &config {
        input = input.with_config(config);
    }
    let lines = Catalog::resolve(&document(), &input).model_lines();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

/// Compare one scenario, byte for byte after masking nothing.
///
/// [`Normalizer::none`] on purpose: there is nothing legitimately variable in a
/// model list — no timestamps, no ids, no ports, no pids. Masking anything here
/// would be masking a real difference.
fn assert_parity(label: &str, config_json: Option<&str>, extra: &[(&str, &str)]) {
    let Some(oracle) = oracle() else {
        return;
    };
    let Some((oracle_stdout, version)) = oracle_models(oracle, config_json, extra) else {
        return;
    };
    println!("differential `{label}` ran against oracle version {version}");
    let env: BTreeMap<String, String> = extra
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    let ours = rust_models(config_json, env);
    let report = diff_normalized(
        "oc-llm::catalog",
        &ours,
        format!("opencode {version} models"),
        &oracle_stdout,
        &Normalizer::none(),
    );
    report.assert_identical();
}

#[test]
fn parity_with_nothing_configured() {
    // The baseline nobody checks and everybody should: an empty environment must
    // autoload nothing. If this crate invented a default provider, this catches it.
    assert_parity("empty environment", None, &[]);
}

#[test]
fn parity_from_a_single_env_var() {
    assert_parity("one env var", None, &[("DEEPSEEK_API_KEY", "sk-x")]);
}

#[test]
fn parity_from_several_env_vars() {
    assert_parity(
        "three env vars",
        None,
        &[
            ("DEEPSEEK_API_KEY", "sk-x"),
            ("MISTRAL_API_KEY", "sk-y"),
            ("GROQ_API_KEY", "sk-z"),
        ],
    );
}

#[test]
fn parity_from_a_bare_config_block() {
    assert_parity(
        "bare config block",
        Some(r#"{"provider":{"groq":{}}}"#),
        &[],
    );
}

#[test]
fn parity_with_config_declared_models_and_overrides() {
    assert_parity(
        "config models and overrides",
        Some(
            r#"{"provider":{"deepseek":{"models":{
                 "deepseek-chat":{"name":"DS Chat Renamed",
                                  "limit":{"context":123456,"output":999}},
                 "brand-new":{"id":"upstream-id","name":"Brand New","reasoning":true,
                              "tool_call":false,
                              "cost":{"input":1.5,"output":3.5,"cache_read":0.1,
                                      "cache_write":0.2},
                              "limit":{"context":50000,"output":5000},
                              "modalities":{"input":["text","image"],
                                            "output":["text"]}}}}}}"#,
        ),
        &[],
    );
}

#[test]
fn parity_with_a_whitelist_and_a_blacklist() {
    assert_parity(
        "whitelist and blacklist",
        Some(
            r#"{"provider":{
                 "deepseek":{"options":{"apiKey":"k"},"whitelist":["deepseek-chat"]},
                 "zhipuai":{"options":{"apiKey":"k"},"blacklist":["glm-5"]}}}"#,
        ),
        &[],
    );
}

#[test]
fn parity_with_a_provider_the_catalog_has_never_heard_of() {
    assert_parity(
        "config-only provider",
        Some(
            r#"{"provider":{"t26new":{"name":"T26 New",
                 "npm":"@ai-sdk/openai-compatible","api":"https://n.example/v1",
                 "models":{"m-one":{"name":"M One",
                                    "limit":{"context":8000,"output":800}},
                           "m-two":{}}}}}"#,
        ),
        &[],
    );
}

#[test]
fn parity_with_experimental_modes_expanded() {
    assert_parity(
        "experimental modes",
        None,
        &[("ANYAPI_API_KEY", "sk-modes")],
    );
}

#[test]
fn parity_with_disabled_providers() {
    assert_parity(
        "disabled_providers",
        Some(r#"{"disabled_providers":["deepseek"],"provider":{"deepseek":{},"groq":{}}}"#),
        &[],
    );
}

#[test]
fn parity_with_enabled_providers() {
    assert_parity(
        "enabled_providers",
        Some(
            r#"{"enabled_providers":["groq"],
                "provider":{"deepseek":{},"groq":{},"mistral":{}}}"#,
        ),
        &[],
    );
}

#[test]
fn parity_with_an_alpha_only_provider_which_must_stay_hidden() {
    assert_parity("alpha only", None, &[("INCEPTRON_API_KEY", "sk-alpha")]);
}

#[test]
fn parity_across_every_provider_in_the_fixture_at_once() {
    // The widest case: all seven declared, so ordering, filtering and mode
    // expansion all have to be right simultaneously.
    assert_parity(
        "all seven providers",
        Some(
            r#"{"provider":{"anyapi":{},"deepseek":{},"groq":{},"impossibl":{},
                            "inceptron":{},"mistral":{},"zhipuai":{}}}"#,
        ),
        &[],
    );
}

#[test]
fn the_differential_harness_can_actually_see_a_difference() {
    // A differential that cannot fail is decoration. This asserts the comparison
    // is load-bearing by feeding it a list with one model removed.
    let ours = "deepseek/deepseek-chat\ndeepseek/deepseek-reasoner\n";
    let theirs = "deepseek/deepseek-chat\n";
    let report = diff_normalized("ours", ours, "theirs", theirs, &Normalizer::none());
    assert!(
        !report.is_identical(),
        "a missing model must survive normalization"
    );
    assert_eq!(report.divergence_count(), 1);

    // And an ordering difference, which is the failure the collation port exists
    // to prevent.
    let reordered = "deepseek/deepseek-reasoner\ndeepseek/deepseek-chat\n";
    let report = diff_normalized("ours", ours, "theirs", reordered, &Normalizer::none());
    assert!(
        !report.is_identical(),
        "a reordered list must survive normalization"
    );
}
