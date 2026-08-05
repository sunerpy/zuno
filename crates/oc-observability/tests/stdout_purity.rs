//! The headline guarantee: no log byte reaches stdout, and the log file has them all.
//!
//! Every test here runs the real `oc-log-probe` binary as a child process and reads
//! its stdout as bytes. That is the only way to check a property about a process's
//! file descriptor 1 — a test running inside `cargo test` shares stdout with
//! `libtest`, so an in-process assertion would be checking the wrong stream.
//!
//! Two things make these tests deterministic rather than dependent on the developer's
//! shell:
//!
//! - Both `OPENCODE_*` variables are explicitly removed from the child's environment
//!   before the case's own values are applied, so an ambient `OPENCODE_LOG_LEVEL`
//!   cannot change a result.
//! - The probe defaults to `Rotation::Never`, so the file it writes is always
//!   `opencode.log` and never depends on today's date.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Markers that appear only inside log records. Finding any of these on stdout means
/// a log byte leaked; finding them in the file means the record was written.
///
/// Chosen to be absent from the probe's protocol frames, which carry level names and
/// paths but never these strings.
const LEVEL_MARKERS: [(&str, &str); 5] = [
    ("trace", "probe-trace"),
    ("debug", "probe-debug"),
    ("info", "probe-info"),
    ("warn", "probe-warn"),
    ("error", "probe-error"),
];

/// Every marker a log record could carry, including the structured tool events and
/// the span names, so the stdout assertion is not limited to plain messages.
const LOG_ONLY_MARKERS: &[&str] = &[
    "probe-trace",
    "probe-debug",
    "probe-info",
    "probe-warn",
    "probe-error",
    "probe-provider",
    "TOOL_LIFECYCLE",
    "toolu_probe",
    "oc_log_probe",
    "elapsed_ms",
];

struct ProbeRun {
    stdout: Vec<u8>,
    stderr: String,
    log: String,
    log_path: PathBuf,
    success: bool,
    _dir: TempDir,
}

impl ProbeRun {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Every non-empty stdout line, parsed as JSON.
    ///
    /// Panics with the offending line when a line does not parse, which is the
    /// failure mode a leaked log byte produces.
    fn frames(&self) -> Vec<serde_json::Value> {
        let text = self.stdout_text();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|e| {
                    panic!(
                        "stdout line is not a valid protocol frame ({e}).\n\
                         This is what a leaked log byte looks like.\n\
                         line: {line:?}\n\
                         full stdout:\n{text}"
                    )
                })
            })
            .collect()
    }

    /// The frame whose `method` is `name`, or a panic naming what was on stdout.
    fn frame(&self, name: &str) -> serde_json::Value {
        self.frames()
            .into_iter()
            .find(|frame| frame.get("method").and_then(serde_json::Value::as_str) == Some(name))
            .unwrap_or_else(|| {
                panic!(
                    "no {name:?} frame on stdout.\nfull stdout:\n{}\nstderr:\n{}",
                    self.stdout_text(),
                    self.stderr
                )
            })
    }
}

/// Runs the probe with a fully controlled environment.
fn run_probe(env: &[(&str, &str)]) -> ProbeRun {
    let dir = TempDir::new().expect("a temp dir for the log directory");
    let log_dir = dir.path().join("log");

    let mut command = Command::new(env!("CARGO_BIN_EXE_oc-log-probe"));
    command
        .env_remove("OPENCODE_LOG_LEVEL")
        .env_remove("OPENCODE_PRINT_LOGS")
        .env_remove("OC_PROBE_ROTATION")
        .env_remove("OC_PROBE_DIRECTIVES")
        .env("OC_PROBE_LOG_DIR", &log_dir);
    for (key, value) in env {
        command.env(key, value);
    }

    let output = command.output().expect("the probe binary runs");
    let log_path = log_dir.join("opencode.log");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();

    ProbeRun {
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        log,
        log_path,
        success: output.status.success(),
        _dir: dir,
    }
}

fn assert_stdout_is_pure(run: &ProbeRun) {
    let stdout = run.stdout_text();
    for marker in LOG_ONLY_MARKERS {
        assert!(
            !stdout.contains(marker),
            "log marker {marker:?} reached stdout. In ACP or any stdio protocol this \
             is a corrupt frame and the editor disconnects.\nfull stdout:\n{stdout}"
        );
    }
    // Structural, not just substring: a leak that happened to avoid every marker
    // would still break the JSON framing.
    let frames = run.frames();
    assert!(
        frames.len() >= 5,
        "expected at least five protocol frames, got {}:\n{stdout}",
        frames.len()
    );
}

/// The acceptance criterion, in one test: every level is emitted while stdout is
/// framing JSON, stdout carries no log bytes, and the log file carries all of them.
#[test]
fn logs_at_every_level_reach_the_file_and_never_stdout() {
    let run = run_probe(&[("OC_PROBE_DIRECTIVES", "trace")]);
    assert!(
        run.success,
        "probe failed.\nstdout:\n{}\nstderr:\n{}",
        run.stdout_text(),
        run.stderr
    );

    assert_stdout_is_pure(&run);

    for (level, marker) in LEVEL_MARKERS {
        assert!(
            run.log.contains(marker),
            "the {level} record is missing from {}.\nfile contents:\n{}",
            run.log_path.display(),
            run.log
        );
    }
}

/// The failure QA scenario, stated as the property it protects: a `tracing::info!`
/// emitted while stdout frames JSON lands in the log file and not on stdout.
#[test]
fn an_info_record_in_a_stdout_framing_mode_lands_only_in_the_file() {
    let run = run_probe(&[]);
    assert!(run.success, "probe failed.\nstderr:\n{}", run.stderr);

    assert!(
        !run.stdout_text().contains("probe-info"),
        "the info record reached stdout:\n{}",
        run.stdout_text()
    );
    assert!(
        run.log.contains("probe-info"),
        "the info record is missing from the log file:\n{}",
        run.log
    );
}

/// Turning the terminal sink on must not move a single byte to stdout. This is the
/// configuration most likely to leak, because it is the one that writes to a
/// terminal at all.
#[test]
fn stdout_stays_pure_even_with_the_terminal_sink_enabled() {
    let run = run_probe(&[("OPENCODE_PRINT_LOGS", "1")]);
    assert!(run.success, "probe failed.\nstderr:\n{}", run.stderr);

    assert_stdout_is_pure(&run);
    assert!(
        run.stderr.contains("probe-info"),
        "with printing enabled the record should be on stderr:\n{}",
        run.stderr
    );
    assert!(
        run.log.contains("probe-info"),
        "printing is additive: the file sink must still receive the record:\n{}",
        run.log
    );
}

/// `packages/core/src/observability/logging.ts:68` compares with `=== "1"`, not
/// through the `truthy()` helper at `packages/core/src/flag/flag.ts:3-6` that most
/// other `OPENCODE_*` booleans use. `OPENCODE_PRINT_LOGS=true` therefore must not
/// enable printing. Matching `truthy()` here would be a silent divergence, and this
/// is the test that would catch it.
#[test]
fn print_logs_accepts_only_the_literal_one() {
    let enabled = run_probe(&[("OPENCODE_PRINT_LOGS", "1")]);
    assert!(
        enabled.success,
        "probe failed.\nstderr:\n{}",
        enabled.stderr
    );
    assert!(enabled.stderr.contains("probe-info"));

    for rejected in ["true", "TRUE", "yes", "0", "on", ""] {
        let run = run_probe(&[("OPENCODE_PRINT_LOGS", rejected)]);
        assert!(run.success, "probe failed.\nstderr:\n{}", run.stderr);
        assert!(
            !run.stderr.contains("probe-info"),
            "OPENCODE_PRINT_LOGS={rejected:?} must not enable printing; the oracle \
             compares with === \"1\".\nstderr:\n{}",
            run.stderr
        );
        assert!(
            run.log.contains("probe-info"),
            "the file sink is unconditional:\n{}",
            run.log
        );
        assert_stdout_is_pure(&run);
    }
}

/// `packages/core/src/observability/logging.ts:57-64`: the value is uppercased before
/// lookup, and anything not in the four-key map falls back to `INFO` rather than
/// failing.
#[test]
fn the_log_level_environment_variable_follows_the_oracle() {
    let cases: BTreeMap<&str, (&str, &[&str], &[&str])> = BTreeMap::from([
        (
            "DEBUG",
            ("DEBUG", &["probe-debug", "probe-info"][..], &[][..]),
        ),
        (
            "debug",
            ("DEBUG", &["probe-debug", "probe-info"][..], &[][..]),
        ),
        ("INFO", ("INFO", &["probe-info"][..], &["probe-debug"][..])),
        ("warn", ("WARN", &["probe-warn"][..], &["probe-info"][..])),
        (
            "ERROR",
            (
                "ERROR",
                &["probe-error"][..],
                &["probe-warn", "probe-info"][..],
            ),
        ),
        ("BOGUS", ("INFO", &["probe-info"][..], &["probe-debug"][..])),
        ("", ("INFO", &["probe-info"][..], &["probe-debug"][..])),
    ]);

    for (value, (resolved, present, absent)) in cases {
        let run = run_probe(&[("OPENCODE_LOG_LEVEL", value)]);
        assert!(
            run.success,
            "probe failed for {value:?}.\nstderr:\n{}",
            run.stderr
        );

        let ready = run.frame("probe/ready");
        assert_eq!(
            ready["params"]["level"].as_str(),
            Some(resolved),
            "OPENCODE_LOG_LEVEL={value:?} should resolve to {resolved}"
        );

        for marker in present {
            assert!(
                run.log.contains(marker),
                "OPENCODE_LOG_LEVEL={value:?} should record {marker}.\nfile:\n{}",
                run.log
            );
        }
        for marker in absent {
            assert!(
                !run.log.contains(marker),
                "OPENCODE_LOG_LEVEL={value:?} should filter out {marker}.\nfile:\n{}",
                run.log
            );
        }
        assert_stdout_is_pure(&run);
    }
}

/// `TRACE` is not one of the four values the oracle accepts, so the environment
/// variable must fall back to `INFO` rather than enabling it. The only way to reach
/// `TRACE` is the programmatic directive string.
#[test]
fn trace_is_reachable_only_through_programmatic_directives() {
    let via_env = run_probe(&[("OPENCODE_LOG_LEVEL", "TRACE")]);
    assert!(
        via_env.success,
        "probe failed.\nstderr:\n{}",
        via_env.stderr
    );
    assert_eq!(
        via_env.frame("probe/ready")["params"]["level"].as_str(),
        Some("INFO")
    );
    assert!(
        !via_env.log.contains("probe-trace"),
        "OPENCODE_LOG_LEVEL=TRACE must not enable trace; the oracle maps it to INFO"
    );

    let via_directives = run_probe(&[("OC_PROBE_DIRECTIVES", "trace")]);
    assert!(
        via_directives.success,
        "probe failed.\nstderr:\n{}",
        via_directives.stderr
    );
    assert!(via_directives.log.contains("probe-trace"));
}

/// Both the CLI and the test suite call `init`, so a second call has to be quiet.
/// The probe calls it twice in one process and reports what the second call did.
#[test]
fn a_second_init_installs_nothing_and_does_not_panic() {
    let run = run_probe(&[]);
    assert!(run.success, "probe failed.\nstderr:\n{}", run.stderr);

    assert_eq!(
        run.frame("probe/ready")["params"]["installed"].as_bool(),
        Some(true),
        "the first init should install the subscriber"
    );
    assert_eq!(
        run.frame("probe/second-init")["params"]["installed"].as_bool(),
        Some(false),
        "the second init must not install a second subscriber"
    );
    assert!(
        run.log.contains("probe-info"),
        "logging must still work after the second init:\n{}",
        run.log
    );
}

/// Span context is the whole reason for using `tracing` over a hand-rolled logger:
/// an event emitted deep inside a tool arrives already attributed to its session and
/// turn, with nobody passing an id down by hand.
#[test]
fn records_carry_their_enclosing_span_stack() {
    let run = run_probe(&[("OPENCODE_LOG_LEVEL", "DEBUG")]);
    assert!(run.success, "probe failed.\nstderr:\n{}", run.stderr);

    let provider_line = run
        .log
        .lines()
        .find(|line| line.contains("probe-provider"))
        .unwrap_or_else(|| panic!("no provider record in the log file:\n{}", run.log));

    for expected in [
        oc_observability::span::TURN,
        oc_observability::span::PROVIDER_REQUEST,
        "ses_probe",
        "anthropic",
    ] {
        assert!(
            provider_line.contains(expected),
            "a record inside provider_request should carry {expected:?} from its span \
             stack.\nline: {provider_line}"
        );
    }
}

/// The tool lifecycle exists so that the four moments of a call can be joined by
/// `call_id`. This asserts all four phases land, including the `abandoned` warning
/// that makes a call which stops being tracked visible instead of an absence.
#[test]
fn the_tool_lifecycle_records_every_phase_with_its_call_id() {
    let run = run_probe(&[("OPENCODE_LOG_LEVEL", "DEBUG")]);
    assert!(run.success, "probe failed.\nstderr:\n{}", run.stderr);

    for phase in ["pending", "running", "completed", "error", "abandoned"] {
        assert!(
            run.log
                .lines()
                .any(|line| line.contains("TOOL_LIFECYCLE") && line.contains(phase)),
            "no TOOL_LIFECYCLE record for phase {phase:?}.\nfile:\n{}",
            run.log
        );
    }

    // `toolu_probe_err` appears twice — once for its pending record and once for the
    // failure — so the phase has to be part of the predicate.
    let failure = run
        .log
        .lines()
        .find(|line| line.contains("toolu_probe_err") && line.contains(r#"phase="error""#))
        .unwrap_or_else(|| panic!("no failure record in the log file:\n{}", run.log));
    for expected in ["not_found", "retryable=false", "model_correctable=true"] {
        assert!(
            failure.contains(expected),
            "a tool failure must carry {expected:?} as data, not leave a consumer to \
             parse the message.\nline: {failure}"
        );
    }
}

/// The default rotation must produce a dated file, and it must land in the directory
/// that was passed in rather than anywhere this crate resolved on its own.
#[test]
fn the_rotating_policy_writes_a_dated_file_in_the_configured_directory() {
    let dir = TempDir::new().expect("a temp dir for the log directory");
    let log_dir = dir.path().join("nested").join("log");

    let output = Command::new(env!("CARGO_BIN_EXE_oc-log-probe"))
        .env_remove("OPENCODE_LOG_LEVEL")
        .env_remove("OPENCODE_PRINT_LOGS")
        .env("OC_PROBE_LOG_DIR", &log_dir)
        .env("OC_PROBE_ROTATION", "daily")
        .output()
        .expect("the probe binary runs");
    assert!(
        output.status.success(),
        "probe failed.\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let files: Vec<String> = std::fs::read_dir(&log_dir)
        .unwrap_or_else(|e| panic!("{} was not created: {e}", log_dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        files.len(),
        1,
        "expected exactly one rolling file, got {files:?}"
    );
    let name = &files[0];
    assert!(
        name.starts_with("opencode.") && name.ends_with(".log"),
        "the rotating file name should be opencode.<date>.log, got {name:?}"
    );
    assert_ne!(
        name, "opencode.log",
        "the daily policy must add a date component"
    );

    let contents = std::fs::read_to_string(Path::new(&log_dir).join(name))
        .expect("the rolling file is readable");
    assert!(contents.contains("probe-info"));
}

/// A log directory that cannot be created has to fail loudly at startup. A process
/// that silently runs with no diagnostics is the worst outcome, because the absence
/// of logs is invisible.
#[test]
fn an_unusable_log_directory_fails_with_the_path_in_the_message() {
    let dir = TempDir::new().expect("a temp dir");
    let blocker = dir.path().join("log");
    std::fs::write(&blocker, b"not a directory").expect("write the blocking file");

    let output = Command::new(env!("CARGO_BIN_EXE_oc-log-probe"))
        .env_remove("OPENCODE_LOG_LEVEL")
        .env_remove("OPENCODE_PRINT_LOGS")
        .env("OC_PROBE_LOG_DIR", &blocker)
        .output()
        .expect("the probe binary runs");

    assert!(
        !output.status.success(),
        "a log directory that is actually a file must not be treated as usable"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&blocker.to_string_lossy().into_owned()),
        "the failure must name the directory it could not create.\nstderr:\n{stderr}"
    );

    // Even a failed startup must not have put anything on stdout beyond the frames
    // the peer itself wrote.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("stdout line is not a protocol frame ({e}): {line:?}"));
    }
}

/// The probe has to refuse to run without a log directory, or a test that forgot to
/// set one would silently assert against an empty file and pass.
#[test]
fn the_probe_refuses_to_run_without_a_log_directory() {
    let output = Command::new(env!("CARGO_BIN_EXE_oc-log-probe"))
        .env_remove("OC_PROBE_LOG_DIR")
        .output()
        .expect("the probe binary runs");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("OC_PROBE_LOG_DIR"),
        "the probe should name the variable it needs"
    );
}
