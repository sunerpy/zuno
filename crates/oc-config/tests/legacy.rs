//! The two QA scenarios for legacy rejection, exercised through the public API.
//!
//! Test names are prefixed `legacy_` so `cargo test -p oc-config legacy` — which
//! filters on test *name*, not on target — runs them alongside the per-form unit
//! tests in `src/legacy/tests.rs`.

use oc_config::legacy::{self, DeprecatedForm};
use oc_error::ConfigError;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// The real user config this port is being validated against, checked in by the
/// discovery task. Its one difference from the live file is the `theme` key, which
/// v1.18.13 does not define and which is not a deprecated form either way.
fn real_user_config() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/user-config.json")
}

/// Happy path: rejecting the legacy forms costs a real user nothing.
#[test]
fn legacy_rejection_finds_nothing_in_the_real_user_config() {
    let source = real_user_config();
    let text = fs::read_to_string(&source).expect("the checked-in user config");
    let value: Value = serde_json::from_str(&text).expect("valid JSON");

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
