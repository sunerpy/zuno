//! Oversized output is detected and preserved. What the model is *shown* is not decided here.
//!
//! # Scope
//!
//! This suite covers detection and storage only:
//!
//! - the verdict names which threshold was crossed;
//! - the measurement reports the limits that were applied;
//! - the complete text is retrievable from the store afterwards.
//!
//! It deliberately asserts **nothing** about what a caller receives when output is
//! oversized. Refusing with the `accept_large_output` opt-in, rather than returning a
//! truncated prefix, is the output-policy layer's decision (todo 72). If both layers
//! asserted it, one of the two suites would have to be wrong about the same
//! behaviour, and no single implementation could satisfy both.

use oc_config::schema::ToolOutputConfig;
use oc_tool::output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, LimitExceeded, SizeVerdict, measure};
use oc_tool::{OutputLimits, ToolOutput, ToolOutputStore};
use std::num::NonZeroU32;

fn tiny() -> OutputLimits {
    OutputLimits {
        max_lines: 10,
        max_bytes: 4_096,
    }
}

#[test]
fn oversized_output_is_detected_and_the_full_text_is_retrievable_afterwards() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = ToolOutputStore::new(dir.path());
    let text: String = (0..500).map(|n| format!("line {n}\n")).collect();
    let mut output = ToolOutput::text("bash", &text);

    // 1. Detection reports the verdict and the limits it applied. 500 lines averaging
    //    nine bytes crosses both the ten-line and the 4096-byte budget, so the verdict
    //    has to say `Both` rather than pick whichever was noticed first.
    let measurement = output.measure(tiny());
    assert!(measurement.is_oversized());
    assert_eq!(
        measurement.verdict,
        SizeVerdict::Oversized(LimitExceeded::Both)
    );
    assert_eq!(measurement.lines, 501);
    assert_eq!(measurement.bytes, text.len());
    assert!(measurement.lines > measurement.limits.max_lines);
    assert!(measurement.bytes > measurement.limits.max_bytes);
    assert_eq!(measurement.limits, tiny());

    // 2. Storage keeps the whole thing and records where it went.
    let stored = store
        .persist("bash", "ses_detect", &output.output)
        .expect("persist");
    output.record_output_path(&stored.path);

    // 3. It is retrievable, byte for byte, from the recorded path alone.
    let path = output
        .output_paths()
        .first()
        .map(std::path::PathBuf::from)
        .expect("a recorded output path");
    assert_eq!(
        store.read("bash", &path).expect("read back"),
        text,
        "the full text has to survive whatever the policy layer decides to show"
    );
    assert_eq!(stored.bytes, text.len());
    assert_eq!(stored.lines, 501);
}

#[test]
fn a_byte_overflow_is_detected_even_when_the_line_count_fits() {
    // One enormous line: the line budget is untouched, the byte budget is not.
    let text = "x".repeat(tiny().max_bytes + 1);

    let measurement = measure(&text, tiny());

    assert_eq!(measurement.lines, 1);
    assert_eq!(
        measurement.verdict,
        SizeVerdict::Oversized(LimitExceeded::Bytes)
    );
    assert_eq!(measurement.limits.max_bytes, tiny().max_bytes);
}

#[test]
fn multibyte_output_is_measured_in_bytes_so_a_budget_cannot_be_overrun() {
    // 2000 CJK characters are 6000 UTF-8 bytes. A char-based count would call this
    // comfortably within a 4096-byte budget and store nothing.
    let text = "输出".repeat(1_000);
    assert_eq!(text.chars().count(), 2_000);
    assert_eq!(text.len(), 6_000);

    let measurement = measure(&text, tiny());

    assert!(measurement.is_oversized());
    assert_eq!(measurement.bytes, 6_000);
}

#[test]
fn detection_and_storage_leave_the_text_exactly_as_produced() {
    // A statement about this crate's own operations: neither measuring nor persisting
    // rewrites the output. It says nothing about what a caller is ultimately handed.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = ToolOutputStore::new(dir.path());
    let text = "a\n".repeat(1_000);
    let output = ToolOutput::text("bash", &text);

    let measurement = output.measure(tiny());
    let stored = store
        .persist("bash", "ses_x", &output.output)
        .expect("persist");

    assert!(measurement.is_oversized());
    assert_eq!(output.output, text, "measuring must not truncate");
    assert_eq!(
        store.read("bash", &stored.path).expect("read"),
        text,
        "persisting must not truncate"
    );
}

#[test]
fn output_within_the_limits_is_not_flagged() {
    let measurement = measure("short\noutput", tiny());

    assert_eq!(measurement.verdict, SizeVerdict::WithinLimits);
    assert!(!measurement.is_oversized());
}

#[test]
fn the_thresholds_come_from_configuration_and_default_to_the_oracles() {
    assert_eq!(OutputLimits::from_config(None).max_lines, DEFAULT_MAX_LINES);
    assert_eq!(OutputLimits::from_config(None).max_bytes, DEFAULT_MAX_BYTES);
    assert_eq!(DEFAULT_MAX_LINES, 2_000);
    assert_eq!(DEFAULT_MAX_BYTES, 51_200);

    let configured = ToolOutputConfig {
        max_lines: NonZeroU32::new(5),
        max_bytes: None,
    };
    let limits = OutputLimits::from_config(Some(&configured));

    // A five-line budget with the default byte budget: six lines is over.
    let measurement = measure(&"line\n".repeat(6), limits);
    assert_eq!(
        measurement.verdict,
        SizeVerdict::Oversized(LimitExceeded::Lines)
    );
    assert_eq!(measurement.limits.max_lines, 5);
    assert_eq!(measurement.limits.max_bytes, DEFAULT_MAX_BYTES);
}

#[test]
fn several_spills_in_one_result_are_all_retrievable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = ToolOutputStore::new(dir.path());
    let mut output = ToolOutput::text("bash", "");

    for chunk in ["first spill", "second spill"] {
        let stored = store.persist("bash", "ses_multi", chunk).expect("persist");
        output.record_output_path(&stored.path);
    }

    let recovered: Vec<String> = output
        .output_paths()
        .into_iter()
        .map(|path| {
            store
                .read("bash", std::path::Path::new(path))
                .expect("read back")
        })
        .collect();

    assert_eq!(recovered, vec!["first spill", "second spill"]);
}
