//! Inspection and cancellation for process-owned background commands.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use zuno_error::ToolError;
use zuno_pty::{
    BackgroundExecutionId, BackgroundExecutionInfo, BackgroundExecutionService, ReplayCursor,
};
use zuno_tool::{ToolContext, ToolEffect, ToolOutput, ToolReplayPolicy, TypedTool};

pub const WIRE_ID: &str = "bg";
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundAction {
    List,
    Output,
    Wait,
    Cancel,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundParams {
    pub action: BackgroundAction,
    #[serde(default, rename = "taskID")]
    pub task_id: Option<String>,
    /// Absolute output cursor returned by a prior `output` or `wait`.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// Wait attention deadline in milliseconds.
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Model-facing view of one workspace's shared execution service.
#[derive(Clone)]
pub struct BackgroundTool {
    service: Arc<BackgroundExecutionService>,
}

impl BackgroundTool {
    #[must_use]
    pub fn new(service: Arc<BackgroundExecutionService>) -> Self {
        Self { service }
    }

    fn owned(
        &self,
        raw: Option<String>,
        session_id: &str,
    ) -> Result<(BackgroundExecutionId, BackgroundExecutionInfo), ToolError> {
        let raw = raw.ok_or_else(|| invalid("taskID is required for this action"))?;
        let id = BackgroundExecutionId::parse(raw).map_err(failed)?;
        let info = self.service.get(&id).map_err(failed)?;
        if info.session_id != session_id {
            return Err(invalid(
                "background execution was not found for this session",
            ));
        }
        Ok((id, info))
    }
}

#[async_trait]
impl TypedTool for BackgroundTool {
    type Params = BackgroundParams;

    fn id(&self) -> &str {
        WIRE_ID
    }

    fn description(&self) -> &str {
        "List, inspect, wait for, or cancel shell commands that are already running in the \
         background. Cancellation is a side effect and this tool is never automatically replayed."
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    fn effect(&self, args: &Value) -> ToolEffect {
        match args.get("action").and_then(Value::as_str) {
            Some("list" | "output" | "wait") => ToolEffect::ReadOnly,
            Some("cancel") | None | Some(_) => ToolEffect::SideEffecting,
        }
    }

    async fn run(
        &self,
        params: BackgroundParams,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        match params.action {
            BackgroundAction::List => {
                reject_unused(&params, false, false, false)?;
                let rows = self
                    .service
                    .list_for_session(&ctx.session_id)
                    .into_iter()
                    .map(render_info)
                    .collect::<Vec<_>>();
                render("background executions", json!({ "executions": rows }))
            }
            BackgroundAction::Output => {
                reject_unused(&params, true, true, false)?;
                let (id, info) = self.owned(params.task_id, &ctx.session_id)?;
                let output = self
                    .service
                    .output(
                        &id,
                        params.cursor.map_or(ReplayCursor::Full, ReplayCursor::From),
                    )
                    .map_err(failed)?;
                render(
                    format!("{}: {}", id, info.status.as_str()),
                    json!({
                        "execution": render_info(info),
                        "output": String::from_utf8_lossy(&output.bytes),
                        "cursor": output.cursor,
                        "retainedFrom": output.retained_from,
                        "totalWritten": output.total_written,
                        "discarded": output.discarded,
                        "outputFile": output.output_file,
                    }),
                )
            }
            BackgroundAction::Wait => {
                reject_unused(&params, true, true, true)?;
                let (id, _) = self.owned(params.task_id, &ctx.session_id)?;
                if params.timeout == Some(0) {
                    return Err(invalid("timeout must be a positive number"));
                }
                let timeout = Duration::from_millis(
                    params.timeout.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS),
                );
                let waited = self
                    .service
                    .wait(&id, Some(timeout))
                    .await
                    .map_err(failed)?;
                let output = self
                    .service
                    .output(
                        &id,
                        params.cursor.map_or(ReplayCursor::Full, ReplayCursor::From),
                    )
                    .map_err(failed)?;
                render(
                    format!("{}: {}", id, waited.info.status.as_str()),
                    json!({
                        "execution": render_info(waited.info),
                        "waitTimedOut": waited.timed_out,
                        "output": String::from_utf8_lossy(&output.bytes),
                        "cursor": output.cursor,
                        "retainedFrom": output.retained_from,
                        "totalWritten": output.total_written,
                        "discarded": output.discarded,
                        "outputFile": output.output_file,
                    }),
                )
            }
            BackgroundAction::Cancel => {
                reject_unused(&params, true, false, false)?;
                let (id, _) = self.owned(params.task_id, &ctx.session_id)?;
                let requested = self.service.cancel(&id).map_err(failed)?;
                let info = self.service.get(&id).map_err(failed)?;
                render(
                    format!(
                        "{}: {}",
                        id,
                        if requested {
                            "cancellation requested"
                        } else {
                            info.status.as_str()
                        }
                    ),
                    json!({
                        "execution": render_info(info),
                        "cancellationRequested": requested,
                    }),
                )
            }
        }
    }
}

fn reject_unused(
    params: &BackgroundParams,
    task_id: bool,
    cursor: bool,
    timeout: bool,
) -> Result<(), ToolError> {
    if !task_id && params.task_id.is_some() {
        return Err(invalid("taskID is not valid for this action"));
    }
    if !cursor && params.cursor.is_some() {
        return Err(invalid("cursor is not valid for this action"));
    }
    if !timeout && params.timeout.is_some() {
        return Err(invalid("timeout is not valid for this action"));
    }
    Ok(())
}

fn render_info(info: BackgroundExecutionInfo) -> Value {
    json!({
        "taskID": info.id.as_str(),
        "sessionID": info.session_id,
        "title": info.title,
        "command": info.command,
        "cwd": info.cwd,
        "status": info.status.as_str(),
        "pid": info.pid,
        "exitCode": info.exit_code,
        "timedOut": info.timed_out,
        "error": info.error,
        "timeCreated": info.time_created,
        "timeUpdated": info.time_updated,
        "timeCompleted": info.time_completed,
        "outputFile": info.output_file,
        "statusFile": info.status_file,
    })
}

fn render(title: impl Into<String>, value: Value) -> Result<ToolOutput, ToolError> {
    let output = serde_json::to_string_pretty(&value).map_err(failed)?;
    Ok(ToolOutput::text(title, output).with_metadata("background_execution", value))
}

fn invalid(message: &'static str) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        )),
    }
}

fn failed(error: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(error),
    }
}
