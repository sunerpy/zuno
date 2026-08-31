//! Foreground attention deadlines for commands owned by the background service.
//!
//! The process is registered with [`zuno_pty::BackgroundExecutionService`] before
//! the shell waits. Reaching the foreground deadline therefore only detaches the
//! caller; it does not move ownership, restart the command, or weaken process-tree
//! cancellation.

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
pub fn timeout_promoted_output(
    command: String,
    timeout_ms: u64,
    execution: &BackgroundExecutionInfo,
) -> ToolOutput {
    let output = format!(
        "Command exceeded the foreground timeout after {:.1}s and is continuing in background \
         (not killed).\n\n\
         Task ID: {}\n\
         Command: {}\n\
         Output file: {}\n\
         Status file: {}\n\n\
         The command is still running; do not rerun it unless you intentionally want a second \
         copy.\n\
         Use `bg` with action=\"wait\" and taskID=\"{}\" to wait for completion or the next \
         progress checkpoint.\n\
         Use `bg` with action=\"output\" and taskID=\"{}\" to inspect output.\n\
         If you expected it to finish quickly and it did not, the `timeout` parameter is in \
         MILLISECONDS; pass a larger value or omit it.",
        timeout_ms as f64 / 1000.0,
        execution.id,
        command,
        execution.output_file.display(),
        execution.status_file.display(),
        execution.id,
        execution.id,
    );

    ToolOutput::text(command.clone(), output)
        .with_metadata("background", true)
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
        .with_metadata("timeout_promoted", true)
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
