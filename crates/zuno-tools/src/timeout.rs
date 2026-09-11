//! Foreground attention deadlines for commands owned by the shared process service.
//!
//! The process is registered with [`zuno_pty::BackgroundExecutionService`] before
//! the shell waits. Reaching the foreground deadline therefore only detaches the
//! caller's observation; the same foreground handle keeps its task ownership,
//! hard ceiling and process-tree cancellation.

use serde_json::json;
use std::time::Duration;
use zuno_pty::BackgroundExecutionInfo;
use zuno_tool::ToolOutput;

pub const DEFAULT_FOREGROUND_TIMEOUT_MS: u64 = 120_000;
pub const MAX_FOREGROUND_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_HARD_CEILING: Duration = Duration::from_secs(30 * 60);

#[must_use]
pub fn normalize_foreground_timeout(requested_ms: Option<u64>) -> u64 {
    requested_ms
        .unwrap_or(DEFAULT_FOREGROUND_TIMEOUT_MS)
        .min(MAX_FOREGROUND_TIMEOUT_MS)
}

#[must_use]
pub fn foreground_yielded_output(
    command: String,
    timeout_ms: u64,
    execution: &BackgroundExecutionInfo,
) -> ToolOutput {
    let output = format!(
        "Command reached its foreground attention deadline after {:.1}s and is still owned \
         by this foreground task.\n\n\
         Task ID: {}\n\
         Command: {}\n\
         Output file: {}\n\
         Status file: {}\n\n\
         The command is still running; do not rerun it unless you intentionally want a second \
         copy.\n\
         Serial critical-path work stays foreground. Continue this SAME handle with `bg` \
         action=\"wait\" and taskID=\"{}\"; each observation is capped at 60 seconds. \
         An observation timeout is not command completion. Keep polling bounded and service \
         steering or interruption between observations; no observer agent is needed.\n\
         Use `bg` with action=\"output\" and taskID=\"{}\" to inspect output.\n\
         No detached callback was scheduled. If this command is a remoteObserver, its eventual \
         exit still requires an authoritative remote-state recheck by stable identifier.",
        timeout_ms as f64 / 1000.0,
        execution.id,
        command,
        execution.output_file.display(),
        execution.status_file.display(),
        execution.id,
        execution.id,
    );

    ToolOutput::text(command.clone(), output)
        .with_metadata("background", false)
        .with_metadata("background_purpose", execution.purpose.as_str())
        .with_metadata(
            "requires_authoritative_refresh",
            execution.purpose.requires_authoritative_refresh(),
        )
        .with_metadata("task_id", execution.id.as_str())
        .with_metadata("command", command)
        .with_metadata(
            "output_file",
            execution.output_file.to_string_lossy().into_owned(),
        )
        .with_metadata(
            "status_file",
            execution.status_file.to_string_lossy().into_owned(),
        )
        .with_metadata("foreground_yielded", true)
        .with_metadata("foreground_timeout_ms", json!(timeout_ms))
}

#[must_use]
pub fn background_started_output(
    command: String,
    execution: &BackgroundExecutionInfo,
) -> ToolOutput {
    let output = format!(
        "Command is running in the background{}.\n\n\
         Task ID: {}\n\
         Command: {}\n\
         Output file: {}\n\
         Status file: {}",
        execution
            .pid
            .map_or_else(String::new, |pid| format!(" with process id {pid}")),
        execution.id,
        command,
        execution.output_file.display(),
        execution.status_file.display(),
    );

    ToolOutput::text(command.clone(), output)
        .with_metadata("background", true)
        .with_metadata("background_purpose", execution.purpose.as_str())
        .with_metadata(
            "requires_authoritative_refresh",
            execution.purpose.requires_authoritative_refresh(),
        )
        .with_metadata("pid", json!(execution.pid))
        .with_metadata("task_id", execution.id.as_str())
        .with_metadata("command", command)
        .with_metadata(
            "output_file",
            execution.output_file.to_string_lossy().into_owned(),
        )
        .with_metadata(
            "status_file",
            execution.status_file.to_string_lossy().into_owned(),
        )
}
