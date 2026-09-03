//! Inspection and cancellation for process-owned background commands, and retrieval
//! of output a size policy withheld.
//!
//! # Why one tool covers both
//!
//! Both are the same question: "show me output that is not in the transcript". A
//! background command's output lives in the execution's ring and file; a withheld
//! result's output lives in the tool-output store. Neither was reachable, so the only
//! retrieval a model had was `shell` with `tail`, which is line-oriented, unbounded in
//! cost, and silently lossy — a truncated `tail -80` of an authoritative test summary
//! forced a needless re-run of the suite. Giving retrieval its own registered tool, a
//! server-clamped window, and one cursor convention makes the recovery path the model
//! is told to take actually exist.
//!
//! Retrieval never runs through the output policy. A window is bounded here, on the
//! server, before any bytes are read; sending it through the policy would let a large
//! window be withheld into yet another artifact, and would make this tool's
//! [`ToolEffect::ReadOnly`] claim for reads a lie about what it does.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use zuno_error::ToolError;
use zuno_paths::GeneratedDirectory;
use zuno_pty::{
    BackgroundExecutionId, BackgroundExecutionInfo, BackgroundExecutionOutput,
    BackgroundExecutionService, ReplayCursor,
};
use zuno_tool::{
    ToolContext, ToolEffect, ToolOutput, ToolOutputStore, ToolReplayPolicy, TypedTool,
};

pub const WIRE_ID: &str = "bg";

/// Metadata key carrying the execution and window facts of a background read.
pub const BACKGROUND_METADATA_KEY: &str = "background_execution";

/// Metadata key carrying the paging facts of a withheld-output read.
pub const ARTIFACT_METADATA_KEY: &str = "tool_output_artifact";

const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 120_000;

/// Bytes returned when a read does not ask for a size.
///
/// Small enough that an exploratory read is cheap, large enough to hold a test summary
/// or a stack trace whole.
pub const DEFAULT_WINDOW_BYTES: u64 = 16 * 1024;

/// The largest window any read returns, whatever it asked for.
///
/// Tied to the byte threshold at which the default output policy stops inlining output
/// at all: a retrieval window can never exceed the amount that policy would have been
/// willing to hand back in the first place. Clamped rather than refused, so a caller
/// that guesses high still gets bytes and a cursor instead of an error.
pub const MAX_WINDOW_BYTES: u64 = zuno_tool::output::DEFAULT_MAX_BYTES as u64;

const TASK_ID: &str = "taskID";
const CURSOR: &str = "cursor";
const LIMIT: &str = "limit";
const TIMEOUT: &str = "timeout";
const OUTPUT_PATH: &str = "outputPath";

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundAction {
    List,
    Output,
    Wait,
    Cancel,
    Artifact,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundParams {
    pub action: BackgroundAction,
    #[serde(default, rename = "taskID")]
    pub task_id: Option<String>,
    /// Absolute byte cursor returned by a prior `output`, `wait`, or `artifact` call.
    #[serde(default)]
    pub cursor: Option<u64>,
    /// Bytes to return in this window. Defaults to 16384 and is clamped, not refused.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Wait attention deadline in milliseconds.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// An `outputPaths` entry from a result whose output was withheld.
    #[serde(default)]
    pub output_path: Option<String>,
}

/// Model-facing view of one workspace's shared execution service.
#[derive(Clone)]
pub struct BackgroundTool {
    service: Arc<BackgroundExecutionService>,
    output_store: Option<ToolOutputStore>,
}

impl BackgroundTool {
    /// Reads a service, and the artifact store of the checkout that service belongs to.
    ///
    /// The store is derived from the service root rather than passed in because both are
    /// registered generated state of the same worktree: the service root is
    /// `<worktree>/.zuno/background/`, so recognising it names the worktree, and the
    /// artifact store is `<worktree>/.zuno/tool-output/` of that same worktree — the
    /// directory `ShellTool` and the tool registry both write to. A service rooted
    /// somewhere unregistered has no worktree to derive, and reads of withheld output
    /// then say so instead of guessing at a directory.
    #[must_use]
    pub fn new(service: Arc<BackgroundExecutionService>) -> Self {
        let output_store = GeneratedDirectory::claim(
            service.root(),
            &zuno_paths::generated::BACKGROUND_EXECUTIONS,
        )
        .map(|directory| ToolOutputStore::in_worktree(directory.worktree()));
        Self {
            service,
            output_store,
        }
    }

    /// Reads withheld output from a store the caller names.
    ///
    /// For a host that owns the store already, and for tests whose service is not rooted
    /// in a checkout.
    #[must_use]
    pub fn with_output_store(mut self, store: ToolOutputStore) -> Self {
        self.output_store = Some(store);
        self
    }

    fn owned(
        &self,
        raw: Option<String>,
        session_id: &str,
    ) -> Result<(BackgroundExecutionId, BackgroundExecutionInfo), ToolError> {
        let raw = raw.ok_or_else(|| invalid(format!("{TASK_ID} is required for this action")))?;
        let id = BackgroundExecutionId::parse(raw).map_err(failed)?;
        let info = self.service.get(&id).map_err(failed)?;
        if info.session_id != session_id {
            return Err(invalid(
                "background execution was not found for this session",
            ));
        }
        Ok((id, info))
    }

    /// One bounded window of an execution's output, from the requested cursor.
    fn window(
        &self,
        id: &BackgroundExecutionId,
        cursor: Option<u64>,
        limit: Option<u64>,
    ) -> Result<BackgroundExecutionOutput, ToolError> {
        let limit = window_bytes(limit);
        self.service
            .output(
                id,
                cursor.map_or(ReplayCursor::Full, ReplayCursor::From),
                Some(limit),
            )
            .map_err(failed)
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
         background, and page through output that was withheld for size. Reads return one \
         bounded window plus the cursor the next window starts at, so ask again with that \
         cursor instead of slicing a file with a shell command. Cancellation is a side effect \
         and this tool is never automatically replayed."
    }

    fn replay_policy(&self) -> ToolReplayPolicy {
        ToolReplayPolicy::Never
    }

    fn effect(&self, args: &Value) -> ToolEffect {
        match args.get("action").and_then(Value::as_str) {
            Some("list" | "output" | "wait" | "artifact") => ToolEffect::ReadOnly,
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
                reject_unused(&params, &[])?;
                let rows = self
                    .service
                    .list_for_session(&ctx.session_id)
                    .into_iter()
                    .map(render_info)
                    .collect::<Vec<_>>();
                render(
                    "background executions",
                    BACKGROUND_METADATA_KEY,
                    json!({ "executions": rows }),
                )
            }
            BackgroundAction::Output => {
                reject_unused(&params, &[TASK_ID, CURSOR, LIMIT])?;
                let (id, info) = self.owned(params.task_id, &ctx.session_id)?;
                let window = self.window(&id, params.cursor, params.limit)?;
                let title = format!("{}: {}", id, info.status.as_str());
                let mut fields = render_window(&window);
                fields.insert("execution".to_owned(), render_info(info));
                render(title, BACKGROUND_METADATA_KEY, Value::Object(fields))
            }
            BackgroundAction::Wait => {
                reject_unused(&params, &[TASK_ID, CURSOR, LIMIT, TIMEOUT])?;
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
                let window = self.window(&id, params.cursor, params.limit)?;
                let title = format!("{}: {}", id, waited.info.status.as_str());
                let mut fields = render_window(&window);
                fields.insert("waitTimedOut".to_owned(), Value::Bool(waited.timed_out));
                fields.insert("execution".to_owned(), render_info(waited.info));
                render(title, BACKGROUND_METADATA_KEY, Value::Object(fields))
            }
            BackgroundAction::Cancel => {
                reject_unused(&params, &[TASK_ID])?;
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
                    BACKGROUND_METADATA_KEY,
                    json!({
                        "execution": render_info(info),
                        "cancellationRequested": requested,
                    }),
                )
            }
            BackgroundAction::Artifact => {
                reject_unused(&params, &[OUTPUT_PATH, CURSOR, LIMIT])?;
                let requested = params
                    .output_path
                    .ok_or_else(|| invalid(format!("{OUTPUT_PATH} is required for this action")))?;
                let store = self.output_store.as_ref().ok_or_else(|| {
                    failed(std::io::Error::other(
                        "background executions are not rooted in a checkout, so this session's \
                         persisted tool output has no address",
                    ))
                })?;
                let from = params.cursor.unwrap_or(0);
                let limit = window_bytes(params.limit);
                let window = store.read_window(
                    WIRE_ID,
                    &ctx.session_id,
                    Path::new(&requested),
                    from,
                    limit,
                )?;
                let has_more = window.cursor < window.total;
                let facts = json!({
                    "outputPath": requested,
                    "cursor": window.cursor,
                    "hasMore": has_more,
                    "windowBytes": window.bytes.len(),
                    "totalBytes": window.total,
                });
                // The bytes are the answer, so they are the output, not a JSON string
                // field inside it: escaping every newline of a retrieved test summary
                // would hand back something harder to read than what was withheld.
                let mut body = String::from_utf8_lossy(&window.bytes).into_owned();
                if has_more {
                    body.push_str(&format!(
                        "\n[{} of {} bytes; call `{WIRE_ID}` again with `{CURSOR}: {}` for the rest]",
                        window.cursor, window.total, window.cursor
                    ));
                }
                Ok(
                    ToolOutput::text(format!("withheld output {requested}"), body)
                        .with_metadata(ARTIFACT_METADATA_KEY, facts),
                )
            }
        }
    }
}

/// Rejects a parameter that has no meaning for the action it was sent with.
///
/// Named rather than positional: five booleans at a call site said nothing about which
/// parameter each one governed, and the action arms are where the answer has to be
/// readable.
fn reject_unused(params: &BackgroundParams, allowed: &[&str]) -> Result<(), ToolError> {
    for (name, present) in [
        (TASK_ID, params.task_id.is_some()),
        (CURSOR, params.cursor.is_some()),
        (LIMIT, params.limit.is_some()),
        (TIMEOUT, params.timeout.is_some()),
        (OUTPUT_PATH, params.output_path.is_some()),
    ] {
        if present && !allowed.contains(&name) {
            return Err(invalid(format!("{name} is not valid for this action")));
        }
    }
    Ok(())
}

/// The window size to read: the caller's, clamped, or the default.
///
/// Clamped here, before anything is read, so no caller — including one that asked for
/// the whole file — can turn a read into an unbounded transfer.
fn window_bytes(requested: Option<u64>) -> usize {
    let bytes = requested
        .unwrap_or(DEFAULT_WINDOW_BYTES)
        .clamp(1, MAX_WINDOW_BYTES);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

fn render_window(window: &BackgroundExecutionOutput) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "output".to_owned(),
        Value::String(String::from_utf8_lossy(&window.bytes).into_owned()),
    );
    fields.insert("cursor".to_owned(), json!(window.cursor));
    fields.insert(
        "hasMore".to_owned(),
        Value::Bool(window.cursor < window.total_written),
    );
    fields.insert("fromDisk".to_owned(), Value::Bool(window.from_disk));
    fields.insert("retainedFrom".to_owned(), json!(window.retained_from));
    fields.insert("totalWritten".to_owned(), json!(window.total_written));
    fields.insert("discarded".to_owned(), json!(window.discarded));
    fields.insert("outputFile".to_owned(), json!(window.output_file));
    fields
}

fn render_info(info: BackgroundExecutionInfo) -> Value {
    json!({
        "taskID": info.id.as_str(),
        "sessionID": info.session_id,
        "title": info.title,
        "command": info.command,
        "purpose": info.purpose.as_str(),
        "requiresAuthoritativeRefresh": info.purpose.requires_authoritative_refresh(),
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

fn render(
    title: impl Into<String>,
    key: &'static str,
    value: Value,
) -> Result<ToolOutput, ToolError> {
    let output = serde_json::to_string_pretty(&value).map_err(failed)?;
    Ok(ToolOutput::text(title, output).with_metadata(key, value))
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: WIRE_ID.to_owned(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message.into(),
        )),
    }
}

fn failed(error: impl std::error::Error + Send + Sync + 'static) -> ToolError {
    ToolError::Failed {
        tool: WIRE_ID.to_owned(),
        source: Box::new(error),
    }
}
