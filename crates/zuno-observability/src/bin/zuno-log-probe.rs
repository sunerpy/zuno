//! A stdout-framing peer, used only to prove that no log byte reaches stdout.
//!
//! # Why a real binary
//!
//! The guarantee under test is about a *process's* file descriptor 1, so it can only
//! be checked by running a process and capturing its stdout as bytes. Two
//! alternatives were rejected:
//!
//! - Re-executing the integration test binary in a probe mode. `libtest` writes its
//!   own progress lines ("running 1 test", "test result: ok") to stdout, so the
//!   captured bytes would be a mix of protocol frames and harness chatter and the
//!   assertion would have to allow-list the chatter — which is exactly the kind of
//!   loophole that lets a real leak through.
//! - Redirecting the current process's fd 1 with `dup2`. That needs `unsafe`, which
//!   this workspace forbids.
//!
//! # What it does
//!
//! Stands in for the Agent Client Protocol: writes newline-delimited JSON frames to
//! stdout, initializes logging from its own environment, and emits a record at every
//! `tracing` level plus a full tool lifecycle. If the library ever routed a record to
//! stdout, it would land between these frames and the test's JSON parse would fail.
//!
//! This file is the **only** place in the crate permitted to touch stdout, and
//! `tests/no_stdout_in_library.rs` names it as the single exemption.
//!
//! # Environment
//!
//! | Variable | Meaning |
//! | :------- | :------ |
//! | `ZUNO_PROBE_LOG_DIR` | required; the log directory to write to |
//! | `ZUNO_PROBE_ROTATION` | optional; `daily`, `hourly` or `never` (default `never`, so the test knows the file name) |
//! | `ZUNO_PROBE_DIRECTIVES` | optional; raw filter directives, the only way to reach `TRACE` |
//! | `ZUNO_LOG_LEVEL` | read by the library under test |
//! | `ZUNO_PRINT_LOGS` | read by the library under test |
//!
//! The `ZUNO_PROBE_*` names configure this fixture, never the product: no crate
//! outside this file reads one. They are also outside the `ZUNO_*` namespace the
//! oracle's flag surface occupies, so neither half of the table can be mistaken for
//! the other.

use std::io::Write;
use std::process::ExitCode;
use zuno_error::ToolError;
use zuno_observability::tool::ToolLifecycle;
use zuno_observability::{LogConfig, Rotation, span};

/// Escapes the subset of JSON string syntax a filesystem path can contain.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Writes one newline-delimited JSON frame to stdout, as a stdio protocol peer does.
///
/// This is the deliberate stdout write the whole test exists to protect. It is
/// flushed immediately so that ordering against any leaked log byte is preserved.
fn frame(body: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{body}");
    let _ = stdout.flush();
}

fn rotation_from_env() -> Rotation {
    match std::env::var("ZUNO_PROBE_ROTATION")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "daily" => Rotation::Daily,
        "hourly" => Rotation::Hourly,
        // `Never` by default: a single `zuno.log` is what the test asserts on, so
        // the file name must not depend on today's date.
        _ => Rotation::Never,
    }
}

fn main() -> ExitCode {
    let Ok(dir) = std::env::var("ZUNO_PROBE_LOG_DIR") else {
        eprintln!("ZUNO_PROBE_LOG_DIR is required");
        return ExitCode::FAILURE;
    };

    frame(r#"{"jsonrpc":"2.0","method":"probe/start","params":{"stage":"pre-init"}}"#);

    let mut config = LogConfig::from_env(&dir).with_rotation(rotation_from_env());
    if let Ok(directives) = std::env::var("ZUNO_PROBE_DIRECTIVES") {
        config = config.with_directives(directives);
    }
    let handle = match zuno_observability::init(config) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("logging init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    frame(&format!(
        r#"{{"jsonrpc":"2.0","method":"probe/ready","params":{{"level":"{level}","print_logs":{print_logs},"installed":{installed},"dir":"{dir}"}}}}"#,
        level = handle.level().as_str(),
        print_logs = handle.print_logs(),
        installed = handle.installed(),
        dir = escape(handle.dir().to_string_lossy().as_ref()),
    ));

    // A second `init` must be quiet and must not install anything. Proving that from
    // a real process means the CLI and the test suite can both call it.
    let second = zuno_observability::init(LogConfig::from_env(&dir));
    let second_installed = match &second {
        Ok(handle) => handle.installed(),
        Err(e) => {
            eprintln!("second logging init failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    frame(&format!(
        r#"{{"jsonrpc":"2.0","method":"probe/second-init","params":{{"installed":{second_installed}}}}}"#
    ));

    // Every level, inside the nested spans later waves will use, so the file records
    // carry a span stack and the stdout stream carries none of it.
    let turn_span = span::turn("ses_probe", 0, "build");
    span::record_turn_model(&turn_span, "anthropic", "claude-sonnet-4-5");
    let turn_entered = turn_span.enter();

    tracing::trace!(marker = "probe-trace", "probe emitted at trace");
    tracing::debug!(marker = "probe-debug", "probe emitted at debug");
    tracing::info!(marker = "probe-info", "probe emitted at info");
    tracing::warn!(marker = "probe-warn", "probe emitted at warn");
    tracing::error!(marker = "probe-error", "probe emitted at error");

    {
        let request_span = span::provider_request("anthropic", "claude-sonnet-4-5", 1, true);
        let _request_entered = request_span.enter();
        tracing::info!(
            marker = "probe-provider",
            "probe emitted inside provider_request"
        );
        span::record_provider_response(&request_span, 200, Some("req_probe"));
    }

    {
        let tool_span = span::tool_call("bash", "toolu_probe");
        let _tool_entered = tool_span.enter();
        let mut call = ToolLifecycle::pending("bash", "toolu_probe");
        call.running();
        call.completed();

        let failing = ToolLifecycle::pending("bash", "toolu_probe_err");
        failing.failed(&ToolError::NotFound {
            tool: "bash".to_owned(),
        });

        // Never terminated on purpose: its `Drop` emits `phase=abandoned`.
        let _abandoned = ToolLifecycle::pending("bash", "toolu_probe_abandoned");
    }

    drop(turn_entered);

    frame(r#"{"jsonrpc":"2.0","method":"probe/emitted","params":{"levels":5}}"#);

    let dropped = handle.dropped_lines().unwrap_or_default();
    frame(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"dropped_lines":{dropped}}}}}"#
    ));

    // Dropping both handles flushes the writer thread before the process exits, so
    // the test can read a complete file.
    drop(second);
    drop(handle);
    ExitCode::SUCCESS
}
