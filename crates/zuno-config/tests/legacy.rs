//! The two QA scenarios for legacy rejection, exercised through the public API.
//!
//! Test names are prefixed `legacy_` so `cargo test -p zuno-config legacy` — which
//! filters on test *name*, not on target — runs them alongside the per-form unit
//! tests in `src/legacy/tests.rs`.

use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;
use zuno_config::legacy::{self, DeprecatedForm};
use zuno_error::ConfigError;

/// The real user config this port is being validated against, checked in byte-for-byte
/// including the legacy `theme` key. It once omitted `theme`, and that omission is
/// what let the schema reject a live config while every test passed.
fn real_user_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/user-config.json")
}

/// Happy path: rejecting the legacy forms costs a real user nothing.
#[test]
fn legacy_rejection_finds_nothing_in_the_real_user_config() {
    let source = real_user_config();
    let text = fs::read_to_string(&source).expect("the checked-in user config");
    let strict = zuno_config::discovery::strip_jsonc(&text);
    let value: Value = serde_json::from_str(&strict).expect("valid JSONC");

    // A copy, so the scenario cannot be accused of touching the original.
    let dir = TempDir::new().expect("tempdir");
    let copy = dir.path().join("opencode.json");
    fs::write(&copy, &text).expect("write the copy");

    assert_eq!(
        legacy::inspect_config(&copy, &value),
        Vec::new(),
        "a config written for v1.18.13 must survive the legacy pass untouched"
    );
    legacy::check_config(&copy, &value).expect("no deprecated form");
    legacy::check_directory(dir.path()).expect("no deprecated file beside it");
    legacy::check_global_directory(dir.path()).expect("no deprecated global file");

    assert_eq!(
        fs::read_to_string(&copy).expect("still readable"),
        text,
        "the pass reports; it never rewrites"
    );
    assert_eq!(
        fs::read_to_string(&source).expect("still readable"),
        text,
        "the original is untouched"
    );
}

/// Failure path: a `mode` block is rejected with a message that is a complete
/// repair instruction.
#[test]
fn legacy_a_mode_block_is_rejected_naming_agent_build_and_primary() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("opencode.json");
    let value = json!({
        "mode": {
            "build": { "model": "anthropic/claude-sonnet-4-5", "temperature": 0.2 },
        },
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&value).expect("serialize"),
    )
    .expect("write");

    let error = legacy::check_config(&path, &value).expect_err("must be rejected");
    let ConfigError::Invalid { path: at, issues } = &error else {
        panic!("expected Invalid, got {error:?}");
    };
    assert_eq!(at, &path);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        issues[0].key_path,
        vec!["mode".to_owned(), "build".to_owned()]
    );

    let message = &issues[0].detail;
    assert!(message.contains("agent.build"), "{message}");
    assert!(message.contains("mode: \"primary\""), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");

    let found = legacy::inspect_config(&path, &value);
    assert_eq!(found[0].form(), DeprecatedForm::ModeBlock);
    assert_eq!(found[0].message(), *message);
}

/// The exemption that lets a live config load must not have swallowed a deprecated
/// form. Both directions: no exempt key is a deprecated form, and every deprecated
/// form still produces its actionable message.
#[test]
fn legacy_the_tui_exemption_rejects_nothing_the_deprecation_pass_owns() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("opencode.json");

    for key in zuno_config::LEGACY_TUI_KEYS {
        let value = json!({ *key: { "anything": true } });
        assert_eq!(
            legacy::inspect_config(&path, &value),
            Vec::new(),
            "{key} must not be a deprecated form"
        );
        zuno_config::Config::from_json_value(&path, value.clone())
            .unwrap_or_else(|e| panic!("{key} must still load: {e:?}"));
    }

    let still_rejected = [
        (
            json!({ "mode": { "build": {} } }),
            DeprecatedForm::ModeBlock,
        ),
        (json!({ "layout": "stretch" }), DeprecatedForm::Layout),
        (json!({ "autoshare": true }), DeprecatedForm::Autoshare),
        (json!({ "reference": {} }), DeprecatedForm::Reference),
        (
            json!({ "agent": { "build": { "tools": { "write": false } } } }),
            DeprecatedForm::AgentTools,
        ),
        (
            json!({ "agent": { "build": { "maxSteps": 40 } } }),
            DeprecatedForm::AgentMaxSteps,
        ),
    ];
    for (value, form) in still_rejected {
        let found = legacy::inspect_config(&path, &value);
        assert!(
            found.iter().any(|d| d.form() == form),
            "{form:?} stopped being detected in {value}"
        );
        let error = legacy::check_config(&path, &value)
            .expect_err(&format!("{form:?} must still be rejected"));
        let ConfigError::Invalid { issues, .. } = &error else {
            panic!("expected Invalid for {form:?}, got {error:?}");
        };
        assert!(
            !issues[0].key_path.is_empty(),
            "{form:?} must name its key path"
        );
    }
}

/// The `variant`-without-`model` rejection, through the real discovery path.
///
/// Written as a config *file* read by `discover_with` rather than by calling
/// `inspect_agent` directly, because the detector was already correct and unreachable:
/// `check_config` had no production caller, so an agent's `tools`, `maxSteps` or a
/// dangling `variant` was swept into provider options and silently ignored. A test
/// that called the detector would have passed throughout that period.
#[test]
fn legacy_a_variant_with_no_model_is_rejected_by_name_when_a_config_is_discovered() {
    let dir = TempDir::new().expect("tempdir");
    let config_root = dir.path().join("config");
    fs::create_dir_all(config_root.join("zuno")).expect("create the config root");
    let path = config_root.join("zuno/zuno.json");
    fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "agent": { "loose": { "description": "no model", "variant": "high" } }
        }))
        .expect("serialise the config"),
    )
    .expect("write the config");

    let project = dir.path().join("project");
    fs::create_dir_all(&project).expect("create the project directory");
    let env =
        zuno_paths::Env::empty().with("XDG_CONFIG_HOME", config_root.to_str().expect("utf-8"));
    let error = zuno_config::discovery::discover_with(
        &zuno_config::discovery::DiscoveryOptions::new(&project, None::<PathBuf>, env),
    )
    .expect_err("a variant with no model must be rejected");

    let report = error.report();
    assert!(
        report.contains("agent.loose.variant"),
        "the message must name the offending field, or the author is told only that \
         something is wrong: {report}"
    );
    assert!(
        report.contains("add `model`, or delete `variant`"),
        "naming the field is not enough; the message must say what to write instead: \
         {report}"
    );
    assert!(
        report.contains(path.to_str().expect("utf-8")),
        "the message must name the file, because the error's own path is the scanned \
         root: {report}"
    );
}

/// An agent that declares both keys is the supported form and must load.
#[test]
fn legacy_a_variant_beside_a_model_is_accepted() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("zuno.json");
    let value = json!({
        "agent": { "tight": { "model": "stub/reasoner", "variant": "high" } }
    });
    fs::write(&path, serde_json::to_string(&value).expect("serialise")).expect("write");

    assert_eq!(
        legacy::inspect_config(&path, &value),
        Vec::new(),
        "rejecting the supported form would make the feature unusable"
    );
}

/// The deprecated agent keys the schema does *not* refuse must still be rejected.
///
/// `tools` and `maxSteps` land in the agent schema's flattened `extra` bag and are
/// swept into `options`, so nothing downstream would ever complain about them. This
/// asserts the discovery path is where they are caught.
#[test]
fn legacy_a_swept_agent_key_is_rejected_when_a_config_is_discovered() {
    for (key, value) in [("maxSteps", json!(4)), ("tools", json!({"read": true}))] {
        let dir = TempDir::new().expect("tempdir");
        let config_root = dir.path().join("config");
        fs::create_dir_all(config_root.join("zuno")).expect("create the config root");
        let path = config_root.join("zuno/zuno.json");
        fs::write(
            &path,
            serde_json::to_string(&json!({ "agent": { "build": { key: value } } }))
                .expect("serialise"),
        )
        .expect("write");

        let project = dir.path().join("project");
        fs::create_dir_all(&project).expect("create the project directory");
        let env =
            zuno_paths::Env::empty().with("XDG_CONFIG_HOME", config_root.to_str().expect("utf-8"));
        let report = zuno_config::discovery::discover_with(
            &zuno_config::discovery::DiscoveryOptions::new(&project, None::<PathBuf>, env),
        )
        .expect_err("a deprecated agent key must be rejected")
        .report();

        assert!(
            report.contains(&format!("agent.build.{key}")),
            "`{key}` is swept into provider options rather than refused by the schema, \
             so without the deprecation pass it becomes an API argument: {report}"
        );
    }
}

/// Every form must be reachable from the pass that production actually runs.
#[test]
fn legacy_the_deprecation_pass_runs_before_the_strict_parse() {
    let discovery =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/discovery.rs"))
            .expect("discovery.rs is readable");

    let check_at = discovery
        .find("crate::legacy::check_config(path, &document)")
        .expect(
            "`parse_layer` no longer runs the deprecation pass, so every form this module \
             detects is accepted and silently ignored — the detector had no production \
             caller at all before this call site existed",
        );
    let parse_at = discovery
        .find("Config::from_json_str(path, &strict)")
        .expect("the strict parse moved; this test's anchor needs updating");

    assert!(
        check_at < parse_at,
        "the deprecation pass must precede the strict parse, or `deny_unknown_fields` \
         reports `unrecognized key` first and the author never learns what to write instead"
    );
}
