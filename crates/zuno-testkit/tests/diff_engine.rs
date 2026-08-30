//! The diff engine's anti-widening guard and optional research-runner diagnostics.
//!
//! A normalizer wide enough to force a pass is worse than no comparison at all.
//! The first test stops masking rules from swallowing a real difference; the
//! second pins that a missing research binary is reported with a path and remedy.

use zuno_testkit::{Normalizer, Oracle, TestkitError, diff_normalized};

/// A perturbation the harness *would* be entitled to mask must still be reported
/// when it is not in a masked position. This is the anti-widening check at the
/// integration level.
#[test]
fn a_volatile_looking_value_outside_a_masked_position_still_diverges() {
    let normalizer = Normalizer::default();
    let left = r#"{"port":4096,"host":"api.anthropic.com:443","createdAt":"2026-04-28T21:18:45Z"}"#;
    let right =
        r#"{"port":4097,"host":"api.anthropic.com:443","createdAt":"2026-08-05T09:00:00Z"}"#;
    let report = diff_normalized("left", left, "right", right, &normalizer);
    assert!(
        !report.is_identical(),
        "a configured port difference must survive normalization:\n{}",
        report.render()
    );
    assert_eq!(report.rules_fired.get("iso8601-timestamp"), Some(&2));
    assert!(!report.rules_fired.contains_key("loopback-port"));
}

/// A binary that is not there fails with a message naming the path it wanted.
#[test]
fn a_missing_comparison_binary_is_actionable() {
    let missing = "/nonexistent/zuno-testkit/oracle/bin/opencode";
    let err = Oracle::at_binary(missing).expect_err("a nonexistent binary cannot be used");
    let rendered = err.to_string();
    println!("{rendered}");
    assert!(
        matches!(err, TestkitError::BinaryNotFound { role: "oracle", .. }),
        "{err:?}"
    );
    assert!(
        rendered.contains(missing),
        "the message must name the path: {rendered}"
    );
    assert!(
        rendered.contains("remedy:"),
        "the message must be actionable: {rendered}"
    );
}
