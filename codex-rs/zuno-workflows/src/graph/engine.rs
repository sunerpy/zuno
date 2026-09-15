use super::node::spawn_node_call;
use super::node::terminal_output;
use super::node::validate_compiled_graph;
use crate::CompiledWorkflow;
use crate::WorkflowCompileRequest;
use crate::WorkflowEngine;
use crate::WorkflowEngineDescriptor;
use crate::WorkflowEngineProvider;
use crate::WorkflowError;
use crate::WorkflowFuture;
use crate::WorkflowHost;
use crate::WorkflowResult;
use crate::WorkflowRun;
use crate::WorkflowRunId;
use crate::WorkflowStartRequest;
use crate::WorkflowStopReason;
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub const GRAPH_ENGINE_REVISION: &str = "graph/v1+zuno/v1";
const STOP_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// Deterministic `graph/v1` runner over the same typed host used by script engines.
///
/// The engine owns DAG scheduling only. Agent/model/profile resolution remains a host
/// responsibility so graph documents stay user-owned and provider-neutral.
pub struct GraphWorkflowEngine {
    host: Arc<dyn WorkflowHost>,
}

impl GraphWorkflowEngine {
    pub fn new(host: Arc<dyn WorkflowHost>) -> Self {
        Self { host }
    }
}

impl WorkflowEngineProvider for GraphWorkflowEngine {
    fn descriptor(&self) -> WorkflowEngineDescriptor {
        WorkflowEngineDescriptor {
            engine: WorkflowEngine::GraphV1,
            dynamic_scripts: false,
            isolated_process: false,
            durable_replay: true,
        }
    }

    fn compile<'a>(
        &'a self,
        request: WorkflowCompileRequest,
    ) -> WorkflowFuture<'a, Result<CompiledWorkflow, WorkflowError>> {
        Box::pin(async move {
            if request.workflow.definition().spec.engine != WorkflowEngine::GraphV1 {
                return Err(graph_engine_error("graph provider only compiles graph/v1"));
            }
            if request.resolved_script.is_some() {
                return Err(graph_engine_error(
                    "graph workflows cannot compile a resolved script",
                ));
            }
            request
                .workflow
                .graph()
                .ok_or_else(|| graph_engine_error("compiled graph is unavailable"))?;
            let artifact = request.workflow.identity().digest.as_bytes().to_vec();
            Ok(CompiledWorkflow {
                workflow: request.workflow,
                engine_revision: GRAPH_ENGINE_REVISION.to_string(),
                artifact: Arc::from(artifact),
            })
        })
    }

    fn start<'a>(
        &'a self,
        request: WorkflowStartRequest,
    ) -> WorkflowFuture<'a, Result<Arc<dyn WorkflowRun>, WorkflowError>> {
        Box::pin(async move {
            validate_compiled_graph(&request)?;
            let run_id = request.run_id.clone();
            let cancellation = CancellationToken::new();
            let cancellation_reason = Arc::new(Mutex::new(None));
            let (result_tx, result_rx) = watch::channel(None);
            let task_host = Arc::clone(&self.host);
            let task_cancellation = cancellation.clone();
            let task_reason = Arc::clone(&cancellation_reason);
            tokio::spawn(async move {
                let result =
                    drive_graph_run(request, task_host, task_cancellation, task_reason).await;
                let _ = result_tx.send(Some(result));
            });
            Ok(Arc::new(GraphWorkflowRun {
                id: run_id,
                cancellation,
                cancellation_reason,
                result: result_rx,
            }) as Arc<dyn WorkflowRun>)
        })
    }
}

struct GraphWorkflowRun {
    id: WorkflowRunId,
    cancellation: CancellationToken,
    cancellation_reason: Arc<Mutex<Option<String>>>,
    result: watch::Receiver<Option<WorkflowResult>>,
}

impl WorkflowRun for GraphWorkflowRun {
    fn id(&self) -> &WorkflowRunId {
        &self.id
    }

    fn cancel(&self, reason: String) {
        let reason = if reason.trim().is_empty() {
            "workflow run was cancelled".to_string()
        } else {
            reason
        };
        let mut stored = lock_unpoisoned(&self.cancellation_reason);
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
                    return uncertain_result("graph workflow runner ended without a result", 0);
                }
            }
        })
    }

    fn dispose<'a>(&'a self) -> WorkflowFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            self.cancel("workflow run was disposed".to_string());
            let _ = self.result().await;
            Ok(())
        })
    }
}

#[derive(Debug)]
struct GraphExecution {
    value: JsonValue,
}

async fn drive_graph_run(
    request: WorkflowStartRequest,
    host: Arc<dyn WorkflowHost>,
    cancellation: CancellationToken,
    cancellation_reason: Arc<Mutex<Option<String>>>,
) -> WorkflowResult {
    let max_wall_time = request
        .compiled
        .workflow
        .definition()
        .spec
        .limits
        .max_wall_time_ms
        .map(Duration::from_millis);
    let task_cancellation = cancellation.clone();
    let agents_started = Arc::new(AtomicU32::new(0));
    let task_agents_started = Arc::clone(&agents_started);
    let mut execution = tokio::spawn(async move {
        execute_graph(request, host, task_cancellation, task_agents_started).await
    });

    match max_wall_time {
        Some(max_wall_time) => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    settle_stopped_execution(
                        &mut execution,
                        cancelled_result(
                            cancellation_reason_value(&cancellation_reason),
                            agents_started.load(Ordering::Relaxed),
                        ),
                        &agents_started,
                    ).await
                }
                joined = &mut execution => execution_result(joined, &agents_started),
                () = tokio::time::sleep(max_wall_time) => {
                    cancellation.cancel();
                    settle_stopped_execution(
                        &mut execution,
                        failed_result(
                            format!("graph workflow exceeded maxWallTimeMs ({})", max_wall_time.as_millis()),
                            agents_started.load(Ordering::Relaxed),
                        ),
                        &agents_started,
                    ).await
                }
            }
        }
        None => {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    settle_stopped_execution(
                        &mut execution,
                        cancelled_result(
                            cancellation_reason_value(&cancellation_reason),
                            agents_started.load(Ordering::Relaxed),
                        ),
                        &agents_started,
                    ).await
                }
                joined = &mut execution => execution_result(joined, &agents_started),
            }
        }
    }
}

async fn settle_stopped_execution(
    execution: &mut JoinHandle<Result<GraphExecution, WorkflowError>>,
    mut stopped: WorkflowResult,
    agents_started: &AtomicU32,
) -> WorkflowResult {
    stopped.agents_started = agents_started.load(Ordering::Relaxed);
    match tokio::time::timeout(STOP_SETTLE_GRACE, &mut *execution).await {
        Ok(Ok(Ok(_))) => stopped,
        Ok(Ok(Err(_))) => stopped,
        Ok(Err(error)) => uncertain_result(
            format!("graph workflow task failed while stopping: {error}"),
            stopped.agents_started,
        ),
        Err(_) => {
            execution.abort();
            let _ = execution.await;
            uncertain_result(
                "graph workflow calls did not settle after cancellation; side effects may be uncertain",
                stopped.agents_started,
            )
        }
    }
}

fn execution_result(
    joined: Result<Result<GraphExecution, WorkflowError>, tokio::task::JoinError>,
    agents_started: &AtomicU32,
) -> WorkflowResult {
    let agents_started = agents_started.load(Ordering::Relaxed);
    match joined {
        Ok(Ok(execution)) => WorkflowResult {
            value: execution.value,
            stop_reason: WorkflowStopReason::Completed,
            error: None,
            agents_started,
        },
        Ok(Err(error)) => failed_result(error.to_string(), agents_started),
        Err(error) => uncertain_result(
            format!("graph workflow task failed: {error}"),
            agents_started,
        ),
    }
}

async fn execute_graph(
    request: WorkflowStartRequest,
    host: Arc<dyn WorkflowHost>,
    cancellation: CancellationToken,
    agents_started: Arc<AtomicU32>,
) -> Result<GraphExecution, WorkflowError> {
    let workflow = &request.compiled.workflow;
    let definition = workflow.definition();
    let graph = workflow
        .graph()
        .ok_or_else(|| graph_engine_error("compiled graph is unavailable"))?;
    let nodes = &definition.spec.nodes;
    let max_concurrent = definition.spec.limits.max_concurrent_agents as usize;
    let mut results = vec![None; nodes.len()];
    let mut remaining_dependencies = (0..nodes.len())
        .map(|index| graph.dependencies(index).map_or(0, <[usize]>::len))
        .collect::<Vec<_>>();
    let mut dependants = vec![Vec::new(); nodes.len()];
    for index in 0..nodes.len() {
        for dependency in graph.dependencies(index).unwrap_or_default() {
            dependants[*dependency].push(index);
        }
    }
    let mut ready = remaining_dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, remaining)| (*remaining == 0).then_some(index))
        .collect::<BTreeSet<_>>();
    let call_cancellation = cancellation.child_token();
    let mut calls = JoinSet::new();
    let mut completed = 0usize;

    while completed < nodes.len() {
        while calls.len() < max_concurrent {
            let Some(index) = ready.pop_first() else {
                break;
            };
            spawn_node_call(
                &mut calls,
                Arc::clone(&host),
                &request.args,
                nodes,
                graph,
                &results,
                index,
                call_cancellation.clone(),
            )?;
            agents_started.fetch_add(1, Ordering::Relaxed);
        }
        if calls.is_empty() {
            return Err(graph_engine_error(
                "graph scheduler has unfinished nodes but no runnable work",
            ));
        }

        let first = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                call_cancellation.cancel();
                while calls.join_next().await.is_some() {}
                return Err(graph_engine_error("graph workflow was cancelled"));
            }
            joined = calls.join_next() => joined,
        };
        let Some(first) = first else {
            return Err(graph_engine_error("graph scheduler lost its running calls"));
        };
        let mut batch = vec![first];
        while let Some(joined) = calls.try_join_next() {
            batch.push(joined);
        }

        let mut first_error = None;
        let mut successful = Vec::new();
        for joined in batch {
            match joined {
                Ok((index, Ok(value))) => successful.push((index, value)),
                Ok((_index, Err(error))) if first_error.is_none() => first_error = Some(error),
                Ok((_index, Err(_))) => {}
                Err(error) if first_error.is_none() => {
                    first_error = Some(graph_engine_error(format!(
                        "graph node task failed and its outcome may be uncertain: {error}"
                    )));
                }
                Err(_) => {}
            }
        }
        successful.sort_by_key(|(index, _)| *index);
        for (index, value) in successful {
            results[index] = Some(value);
            completed += 1;
            for dependant in &dependants[index] {
                remaining_dependencies[*dependant] -= 1;
                if remaining_dependencies[*dependant] == 0 {
                    ready.insert(*dependant);
                }
            }
        }
        if let Some(error) = first_error {
            call_cancellation.cancel();
            while calls.join_next().await.is_some() {}
            return Err(error);
        }
    }

    Ok(GraphExecution {
        value: terminal_output(graph, nodes, &results)?,
    })
}

pub(super) fn graph_engine_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::Engine {
        engine: WorkflowEngine::GraphV1,
        message: message.into(),
    }
}

fn failed_result(message: impl Into<String>, agents_started: u32) -> WorkflowResult {
    WorkflowResult {
        value: JsonValue::Null,
        stop_reason: WorkflowStopReason::Failed,
        error: Some(message.into()),
        agents_started,
    }
}

fn cancelled_result(message: String, agents_started: u32) -> WorkflowResult {
    WorkflowResult {
        value: JsonValue::Null,
        stop_reason: WorkflowStopReason::Cancelled,
        error: Some(message),
        agents_started,
    }
}

fn uncertain_result(message: impl Into<String>, agents_started: u32) -> WorkflowResult {
    WorkflowResult {
        value: JsonValue::Null,
        stop_reason: WorkflowStopReason::Uncertain,
        error: Some(message.into()),
        agents_started,
    }
}

fn cancellation_reason_value(reason: &Mutex<Option<String>>) -> String {
    lock_unpoisoned(reason)
        .clone()
        .unwrap_or_else(|| "workflow run was cancelled".to_string())
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
