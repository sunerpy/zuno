//! What the guard's own exit status says about the payload it supervised.
//!
//! A consumer only sees the guard's `ExitStatus`. It must be able to tell apart
//! four things without parsing stderr: the payload exited with a code, the
//! payload was killed by a signal, the payload could not be started at all, and
//! the guard itself failed. Collapsing any of those into `exit 1` turns an
//! unknown outcome into an authoritative "the command failed".
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, ExitStatus, Stdio};

fn fixture() -> &'static str {
    env!("CARGO_BIN_EXE_zuno-process-fixture")
}

fn guarded(program: &str, arguments: &[&str]) -> ExitStatus {
    zuno_process::activate_guard_executable(fixture()).expect("activate fixture guard");
    let (program, arguments) = zuno_process::guarded_argv(program, arguments);
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("the guard process runs")
}

#[test]
fn a_payload_exit_code_passes_through_unchanged() {
    let status = guarded("sh", &["-c", "exit 7"]);
    assert_eq!(status.code(), Some(7), "guard status: {status}");
}

#[test]
fn a_payload_killed_by_a_signal_is_reported_as_signal_death() {
    let status = guarded("sh", &["-c", "kill -KILL $$"]);
    assert_eq!(
        status.signal(),
        Some(9),
        "signal death must stay signal death instead of becoming exit 1: {status}"
    );
    assert_eq!(status.code(), None, "guard status: {status}");
}

#[test]
fn a_payload_that_cannot_be_found_is_reported_as_127() {
    // 127 is the shell convention for "command not found"; the crate publishes it
    // as `GUARD_NOT_FOUND_EXIT_CODE`, which `guard_exit_classification_is_stable`
    // pins to this literal.
    let status = guarded("/nonexistent/zuno-guard-exit-test-binary", &[]);
    assert_eq!(
        status.code(),
        Some(127),
        "a missing program is the shell convention 127, not a generic failure: {status}"
    );
}

#[test]
fn a_guard_protocol_failure_is_reported_as_the_guard_failure_code() {
    // A malformed guard argv (no `--` separator) never reaches the payload. 125 is
    // the wrapper-utility convention shared with `timeout`, `env`, and `nice`; the
    // crate publishes it as `GUARD_FAILURE_EXIT_CODE`.
    let status = Command::new(fixture())
        .args([
            zuno_process::GUARD_MARKER,
            "supervise",
            &std::process::id().to_string(),
            "sh",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the guard process runs");
    assert_eq!(
        status.code(),
        Some(125),
        "a guard failure is infrastructure, not a command result: {status}"
    );
}

#[test]
fn guard_exit_classification_is_stable() {
    use zuno_process::{
        GUARD_FAILURE_EXIT_CODE, GUARD_NOT_EXECUTABLE_EXIT_CODE, GUARD_NOT_FOUND_EXIT_CODE,
        GuardExit,
    };

    // The literals the behavioral tests above assert against are these constants.
    assert_eq!(GUARD_FAILURE_EXIT_CODE, 125);
    assert_eq!(GUARD_NOT_EXECUTABLE_EXIT_CODE, 126);
    assert_eq!(GUARD_NOT_FOUND_EXIT_CODE, 127);

    let exited = |code: u8| ExitStatus::from_raw(i32::from(code) << 8);
    assert_eq!(GuardExit::classify(&exited(0)), GuardExit::Exited(0));
    assert_eq!(GuardExit::classify(&exited(7)), GuardExit::Exited(7));
    assert_eq!(
        GuardExit::classify(&ExitStatus::from_raw(9)),
        GuardExit::Signaled(9)
    );
    assert_eq!(
        GuardExit::classify(&exited(GUARD_FAILURE_EXIT_CODE)),
        GuardExit::GuardFailed
    );
    assert_eq!(
        GuardExit::classify(&exited(GUARD_NOT_EXECUTABLE_EXIT_CODE)),
        GuardExit::NotExecutable
    );
    assert_eq!(
        GuardExit::classify(&exited(GUARD_NOT_FOUND_EXIT_CODE)),
        GuardExit::NotFound
    );
}
