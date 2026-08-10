//! Regression tests for the executable that source-coupled tests actually run.
//!
//! Each scenario re-enters this test binary in a child process. That gives the
//! child an isolated environment without mutating process-global variables while
//! the Rust test harness is running tests in parallel.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use oc_testkit::{Subject, subject::ENV_SUBJECT_BINARY};

const CHILD_SCENARIO: &str = "OC_TESTKIT_SUBJECT_FRESHNESS_CHILD";
const SUBJECT_PATH: &str = "OC_TESTKIT_SUBJECT_FRESHNESS_PATH";
const BUILD_COUNT: &str = "OC_TESTKIT_SUBJECT_FRESHNESS_BUILD_COUNT";
const FRESH_BYTES: &[u8] = b"fresh subject built from the current sources";

#[test]
fn an_existing_workspace_subject_is_rebuilt_before_source_coupled_tests_use_it() {
    if child_scenario() == Some("stale-workspace-subject") {
        let expected = required_path(SUBJECT_PATH);
        let subject = Subject::discover_or_build().expect("refresh the workspace subject");
        assert_eq!(subject.program(), expected);
        assert_eq!(
            std::fs::read(subject.program()).expect("read the refreshed subject"),
            FRESH_BYTES,
            "discover_or_build reused the stale candidate without asking Cargo to refresh it"
        );
        let label = subject.provenance().label();
        assert!(
            label.contains("cargo build"),
            "the subject's build provenance must be visible: {label}"
        );
        let second = Subject::discover_or_build().expect("reuse the refreshed subject");
        assert_eq!(second.program(), expected);
        assert_eq!(
            std::fs::read_to_string(required_path(BUILD_COUNT)).expect("read build count"),
            "1",
            "parallel tests must share one Cargo freshness check per process"
        );
        return;
    }

    let temp = tempfile::tempdir().expect("temporary subject fixture");
    let target = temp.path().join("target");
    let candidate = target.join("debug").join(oc_testkit::SUBJECT_BIN);
    let build_count = temp.path().join("build-count");
    std::fs::create_dir_all(candidate.parent().expect("candidate parent"))
        .expect("create candidate parent");
    std::fs::write(
        &candidate,
        b"stale subject built before the source mutation",
    )
    .expect("write stale candidate");

    let fake_cargo = compile_fake_cargo(temp.path());
    let output = child_command(
        "an_existing_workspace_subject_is_rebuilt_before_source_coupled_tests_use_it",
    )
    .env(CHILD_SCENARIO, "stale-workspace-subject")
    .env(SUBJECT_PATH, &candidate)
    .env(BUILD_COUNT, &build_count)
    .env("CARGO_TARGET_DIR", &target)
    .env("CARGO", &fake_cargo)
    .env_remove(ENV_SUBJECT_BINARY)
    .output()
    .expect("run stale-subject child");

    assert_child_passed(output);
}

#[test]
fn an_explicit_subject_override_is_honoured_and_its_source_is_reported() {
    if child_scenario() == Some("explicit-subject") {
        let expected = required_path(SUBJECT_PATH);
        let subject = Subject::discover_or_build().expect("use the explicit subject");
        assert_eq!(subject.program(), expected);
        let label = subject.provenance().label();
        assert!(
            label.contains(ENV_SUBJECT_BINARY),
            "the explicit override's source must be visible: {label}"
        );
        return;
    }

    let temp = tempfile::tempdir().expect("temporary explicit subject fixture");
    let explicit = temp.path().join("caller-owned-subject");
    std::fs::write(&explicit, b"caller-owned subject").expect("write explicit subject");

    let output =
        child_command("an_explicit_subject_override_is_honoured_and_its_source_is_reported")
            .env(CHILD_SCENARIO, "explicit-subject")
            .env(SUBJECT_PATH, &explicit)
            .env(ENV_SUBJECT_BINARY, &explicit)
            .env("CARGO", temp.path().join("cargo-must-not-run"))
            .output()
            .expect("run explicit-subject child");

    assert_child_passed(output);
}

fn child_scenario() -> Option<&'static str> {
    match std::env::var(CHILD_SCENARIO).as_deref() {
        Ok("stale-workspace-subject") => Some("stale-workspace-subject"),
        Ok("explicit-subject") => Some("explicit-subject"),
        _ => None,
    }
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} must be set")))
}

fn child_command(test_name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command.args(["--exact", test_name, "--nocapture"]);
    command
}

fn compile_fake_cargo(root: &Path) -> PathBuf {
    let source = root.join("fake-cargo.rs");
    let binary = root.join(if cfg!(windows) {
        "fake-cargo.exe"
    } else {
        "fake-cargo"
    });
    std::fs::write(
        &source,
        format!(
            "fn main() {{\n    let path = std::env::var_os({SUBJECT_PATH:?}).expect(\"subject path\");\n    let count_path = std::env::var_os({BUILD_COUNT:?}).expect(\"build count path\");\n    let count = std::fs::read_to_string(&count_path).ok().and_then(|value| value.parse::<u8>().ok()).unwrap_or(0) + 1;\n    std::fs::write(count_path, count.to_string()).expect(\"record build count\");\n    std::fs::write(path, {FRESH_BYTES:?}).expect(\"refresh subject\");\n}}\n"
        ),
    )
    .expect("write fake Cargo source");

    let status = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .status()
        .expect("compile fake Cargo");
    assert!(status.success(), "fake Cargo compilation failed: {status}");
    binary
}

fn assert_child_passed(output: Output) {
    assert!(
        output.status.success(),
        "child failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
