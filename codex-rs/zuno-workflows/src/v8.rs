use crate::CompiledWorkflow;
use crate::WorkflowCallIdentity;
use crate::WorkflowCompileRequest;
use crate::WorkflowEngine;
use crate::WorkflowEngineDescriptor;
use crate::WorkflowEngineProvider;
use crate::WorkflowError;
use crate::WorkflowFuture;
use crate::WorkflowResult;
use crate::WorkflowRun;
use crate::WorkflowRunId;
use crate::WorkflowStartRequest;
use crate::WorkflowStopReason;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionCellExecutionLimits;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

mod delegate;

use delegate::V8WorkflowDelegate;
use delegate::host_tools;
use delegate::workflow_source;

pub const V8_ENGINE_REVISION: &str = "javascript/v1+code-mode/v1";
const WAIT_YIELD_TIME_MS: u64 = 10_000;
const MAX_SCRIPT_OUTPUT_TOKENS: usize = 10_000;

/// One typed request made by a workflow script to its owning Zuno host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowHostCall {
    pub kind: WorkflowHostCallKind,
    pub payload: JsonValue,
    pub identity: Option<WorkflowCallIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowHostCallKind {
    Agent,
    Phase,
    Log,
    Checkpoint,
}

/// Host operations exposed to the restricted V8 workflow runner.
pub trait WorkflowHost: Send + Sync {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>>;
}

/// `javascript/v1` over Codex's replaceable Code Mode session provider.
pub struct V8WorkflowEngine {
    sessions: Arc<dyn CodeModeSessionProvider>,
    host: Arc<dyn WorkflowHost>,
    isolated_process: bool,
}

impl V8WorkflowEngine {
    pub fn new(
        sessions: Arc<dyn CodeModeSessionProvider>,
        host: Arc<dyn WorkflowHost>,
        isolated_process: bool,
    ) -> Self {
        Self {
            sessions,
            host,
            isolated_process,
        }
    }
}

impl WorkflowEngineProvider for V8WorkflowEngine {
    fn descriptor(&self) -> WorkflowEngineDescriptor {
        WorkflowEngineDescriptor {
            engine: WorkflowEngine::JavaScriptV1,
            dynamic_scripts: true,
            isolated_process: self.isolated_process,
            durable_replay: true,
        }
    }

    fn compile<'a>(
        &'a self,
        request: WorkflowCompileRequest,
    ) -> WorkflowFuture<'a, Result<CompiledWorkflow, WorkflowError>> {
        Box::pin(async move {
            let engine = request.workflow.definition().spec.engine;
            if engine != WorkflowEngine::JavaScriptV1 {
                return Err(WorkflowError::Engine {
                    engine,
                    message: "V8 provider only compiles javascript/v1".to_string(),
                });
            }
            let script = request
                .workflow
                .definition()
                .spec
                .script
                .as_deref()
                .map(str::to_string)
                .or_else(|| request.resolved_script.as_deref().map(str::to_string))
                .ok_or_else(|| WorkflowError::Engine {
                    engine,
                    message: "resolved workflow script is unavailable".to_string(),
                })?;
            Ok(CompiledWorkflow {
                workflow: request.workflow,
                engine_revision: V8_ENGINE_REVISION.to_string(),
                artifact: Arc::from(script.into_bytes()),
            })
        })
    }

    fn start<'a>(
        &'a self,
        request: WorkflowStartRequest,
    ) -> WorkflowFuture<'a, Result<Arc<dyn WorkflowRun>, WorkflowError>> {
        Box::pin(async move {
            let engine = request.compiled.workflow.definition().spec.engine;
            if engine != WorkflowEngine::JavaScriptV1 {
                return Err(engine_error("compiled workflow is not javascript/v1"));
            }
            let limits = &request.compiled.workflow.definition().spec.limits;
            let max_wall_time = limits.max_wall_time_ms.map(Duration::from_millis);
            let heap_limit = limits
                .max_heap_size_bytes
                .map(usize::try_from)
                .transpose()
                .map_err(|_| engine_error("maxHeapSizeBytes exceeds this platform"))?;
            let routes = request
                .compiled
                .workflow
                .definition()
                .spec
                .routes
                .keys()
                .cloned()
                .collect();
            let agents_started = Arc::new(AtomicU32::new(0));
            let delegate = Arc::new(V8WorkflowDelegate {
                host: Arc::clone(&self.host),
                routes,
                agents_started: Arc::clone(&agents_started),
            });
            let session = self
                .sessions
                .create_session_with_limits(CodeModeSessionCellExecutionLimits {
                    max_yield_time_ms: limits.max_wall_time_ms,
                    max_heap_size_bytes: heap_limit,
                })
                .await
                .map_err(engine_error)?;
            let source = workflow_source(&request)?;
            let started = session
                .execute(
                    ExecuteRequest {
                        tool_call_id: request.run_id.as_str().to_string(),
                        enabled_tools: host_tools(),
                        source,
                        yield_time_ms: Some(WAIT_YIELD_TIME_MS),
                        max_output_tokens: Some(MAX_SCRIPT_OUTPUT_TOKENS),
                    },
                    delegate,
                )
                .await
                .map_err(engine_error)?;
            let cell_id = started.cell_id.clone();
            let cancellation = CancellationToken::new();
            let cancellation_reason = Arc::new(Mutex::new(None));
            let (result_tx, result_rx) = watch::channel(None);
            let task_session = Arc::clone(&session);
            let task_cancellation = cancellation.clone();
            let task_cancellation_reason = Arc::clone(&cancellation_reason);
            tokio::spawn(async move {
                let mut result = drive_run(
                    task_session.as_ref(),
                    started,
                    &cell_id,
                    task_cancellation,
                    task_cancellation_reason,
                    max_wall_time,
                )
                .await;
                result.agents_started = agents_started.load(Ordering::Relaxed);
                let _ = task_session.shutdown().await;
                let _ = result_tx.send(Some(result));
            });
            Ok(Arc::new(V8WorkflowRun {
                id: request.run_id,
                cancellation,
                cancellation_reason,
                result: result_rx,
                session,
            }) as Arc<dyn WorkflowRun>)
        })
    }
}

struct V8WorkflowRun {
    id: WorkflowRunId,
    cancellation: CancellationToken,
    cancellation_reason: Arc<Mutex<Option<String>>>,
    result: watch::Receiver<Option<WorkflowResult>>,
    session: Arc<dyn CodeModeSession>,
}

impl WorkflowRun for V8WorkflowRun {
    fn id(&self) -> &WorkflowRunId {
        &self.id
    }

    fn cancel(&self, reason: String) {
        let reason = if reason.trim().is_empty() {
            "workflow run was cancelled".to_string()
        } else {
            reason
        };
        let mut stored = self
            .cancellation_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stored.is_none() {
            *stored = Some(reason);
        }
        drop(stored);
        self.cancellation.cancel();
    }

    fn result<'a>(&'a self) -> WorkflowFuture<'a, WorkflowResult> {
        Box::pin(async move {
            let mut receiver = self.result.clone();
            loop {
                if let Some(result) = receiver.borrow().clone() {
                    return result;
                }
                if receiver.changed().await.is_err() {
                    return failed_result("workflow runner ended without a result");
                }
            }
        })
    }

    fn dispose<'a>(&'a self) -> WorkflowFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            self.cancellation.cancel();
            let _ = self.result().await;
            self.session.shutdown().await.map_err(engine_error)
        })
    }
}

async fn drive_run(
    session: &dyn CodeModeSession,
    started: codex_code_mode::StartedCell,
    cell_id: &CellId,
    cancellation: CancellationToken,
    cancellation_reason: Arc<Mutex<Option<String>>>,
    max_wall_time: Option<Duration>,
) -> WorkflowResult {
    let result = wait_for_result(session, started, cell_id);
    tokio::pin!(result);
    match max_wall_time {
        Some(max_wall_time) => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => terminate_run(
                    session,
                    cell_id,
                    WorkflowStopReason::Cancelled,
                    cancellation_reason_value(&cancellation_reason),
                ).await,
                result = &mut result => result,
                () = tokio::time::sleep(max_wall_time) => terminate_run(
                    session,
                    cell_id,
                    WorkflowStopReason::Failed,
                    format!("javascript workflow exceeded maxWallTimeMs ({})", max_wall_time.as_millis()),
                ).await,
            }
        }
        None => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => terminate_run(
                    session,
                    cell_id,
                    WorkflowStopReason::Cancelled,
                    cancellation_reason_value(&cancellation_reason),
                ).await,
                result = &mut result => result,
            }
        }
    }
}

async fn wait_for_result(
    session: &dyn CodeModeSession,
    started: codex_code_mode::StartedCell,
    cell_id: &CellId,
) -> WorkflowResult {
    let mut text = String::new();
    let mut response = match started.initial_response().await {
        Ok(response) => response,
        Err(message) => return uncertain_result(message),
    };
    loop {
        match response {
            RuntimeResponse::Yielded { content_items, .. } => {
                if let Err(message) = append_text(&mut text, content_items) {
                    return failed_result(message);
                }
                response = match session
                    .wait(WaitRequest {
                        cell_id: cell_id.clone(),
                        yield_time_ms: WAIT_YIELD_TIME_MS,
                    })
                    .await
                {
                    Ok(WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response)) => {
                        response
                    }
                    Err(message) => return uncertain_result(message),
                };
            }
            RuntimeResponse::Terminated { content_items, .. } => {
                let _ = append_text(&mut text, content_items);
                return WorkflowResult {
                    value: JsonValue::Null,
                    stop_reason: WorkflowStopReason::Cancelled,
                    error: Some("javascript workflow host terminated the run".to_string()),
                    agents_started: 0,
                };
            }
            RuntimeResponse::Result {
                content_items,
                error_text,
                ..
            } => {
                if let Err(message) = append_text(&mut text, content_items) {
                    return failed_result(message);
                }
                if let Some(error) = error_text {
                    return failed_result(error);
                }
                return match serde_json::from_str(&text) {
                    Ok(value) => WorkflowResult {
                        value,
                        stop_reason: WorkflowStopReason::Completed,
                        error: None,
                        agents_started: 0,
                    },
                    Err(error) => {
                        failed_result(format!("workflow script returned invalid JSON: {error}"))
                    }
                };
            }
        }
    }
}

async fn terminate_run(
    session: &dyn CodeModeSession,
    cell_id: &CellId,
    stop_reason: WorkflowStopReason,
    message: String,
) -> WorkflowResult {
    match session.terminate(cell_id.clone()).await {
        Ok(_) => WorkflowResult {
            value: JsonValue::Null,
            stop_reason,
            error: Some(message),
            agents_started: 0,
        },
        Err(terminate_error) => uncertain_result(format!(
            "{message}; code-mode termination failed: {terminate_error}"
        )),
    }
}

fn cancellation_reason_value(reason: &Mutex<Option<String>>) -> String {
    reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| "workflow run was cancelled".to_string())
}

fn append_text(
    output: &mut String,
    items: Vec<FunctionCallOutputContentItem>,
) -> Result<(), String> {
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => output.push_str(&text),
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::InputAudio { .. } => {
                return Err("workflow runner returned non-text content".to_string());
            }
        }
    }
    Ok(())
}

pub(super) fn engine_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::Engine {
        engine: WorkflowEngine::JavaScriptV1,
        message: message.into(),
    }
}

fn failed_result(message: impl Into<String>) -> WorkflowResult {
    WorkflowResult {
        value: JsonValue::Null,
        stop_reason: WorkflowStopReason::Failed,
        error: Some(message.into()),
        agents_started: 0,
    }
}

fn uncertain_result(message: impl Into<String>) -> WorkflowResult {
    WorkflowResult {
        value: JsonValue::Null,
        stop_reason: WorkflowStopReason::Uncertain,
        error: Some(message.into()),
        agents_started: 0,
    }
}
