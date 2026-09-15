use super::*;
use crate::ValidatedWorkflow;
use crate::WorkflowCompileRequest;
use crate::WorkflowEngine;
use crate::WorkflowEngineProvider;
use crate::WorkflowError;
use crate::WorkflowFuture;
use crate::WorkflowHost;
use crate::WorkflowHostCall;
use crate::WorkflowResult;
use crate::WorkflowRun;
use crate::WorkflowRunId;
use crate::WorkflowStartRequest;
use crate::WorkflowStopReason;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn graph_workflow(nodes: &str, max_concurrent_agents: u32) -> Arc<ValidatedWorkflow> {
    graph_workflow_with_wall_time(nodes, max_concurrent_agents, None)
}

fn graph_workflow_with_wall_time(
    nodes: &str,
    max_concurrent_agents: u32,
    max_wall_time_ms: Option<u64>,
) -> Arc<ValidatedWorkflow> {
    let max_wall_time = max_wall_time_ms
        .map(|value| format!("    maxWallTimeMs: {value}\n"))
        .unwrap_or_default();
    let source = format!(
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata: {{ name: graph-run, version: "1" }}
spec:
  engine: graph/v1
  routes:
    work: {{ agentRef: worker }}
  limits:
    maxConcurrentAgents: {max_concurrent_agents}
    maxTotalAgents: 8
    maxItemsPerCall: 8
{max_wall_time}  nodes:
{nodes}
"#
    );
    Arc::new(
        crate::WorkflowDefinition::parse(
            &source,
            crate::WorkflowFormat::Yaml,
            "project://graph-run",
        )
        .unwrap(),
    )
}

async fn start_graph(
    host: Arc<dyn WorkflowHost>,
    workflow: Arc<ValidatedWorkflow>,
    args: JsonValue,
) -> Arc<dyn WorkflowRun> {
    let engine = GraphWorkflowEngine::new(host);
    let compiled = engine
        .compile(WorkflowCompileRequest {
            workflow,
            resolved_script: None,
        })
        .await
        .unwrap();
    engine
        .start(WorkflowStartRequest {
            compiled: Arc::new(compiled),
            run_id: WorkflowRunId::new("graph-run-1").unwrap(),
            parent_thread_id: "thread-1".to_string(),
            args,
        })
        .await
        .unwrap()
}

#[derive(Default)]
struct DependencyHost {
    calls: Mutex<Vec<WorkflowHostCall>>,
}

impl WorkflowHost for DependencyHost {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        _cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call.clone());
            let id = call.payload["id"].as_str().unwrap();
            Ok(json!({ "node": id }))
        })
    }
}

#[tokio::test]
async fn graph_engine_passes_dependency_outputs_and_returns_terminal_value() {
    let workflow = graph_workflow(
        r#"    - id: scan
      route: work
      input: "Inspect the repository"
    - id: build
      route: work
      needs: [scan]
      input: { prompt: "Implement the change", mode: write }
"#,
        2,
    );
    let host = Arc::new(DependencyHost::default());
    let run = start_graph(host.clone(), workflow, json!({"task": "fix it"})).await;

    assert_eq!(
        run.result().await,
        WorkflowResult {
            value: json!({"node": "build"}),
            stop_reason: WorkflowStopReason::Completed,
            error: None,
            agents_started: 2,
        }
    );
    let calls = host.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].identity.as_ref().unwrap().id, "scan");
    assert_eq!(calls[1].identity.as_ref().unwrap().id, "build");
    assert_eq!(calls[1].payload["needs"]["scan"], json!({"node": "scan"}));
    assert_eq!(calls[1].payload["args"], json!({"task": "fix it"}));
    assert!(
        calls[1].payload["prompt"]
            .as_str()
            .unwrap()
            .starts_with("Implement the change")
    );
}

#[derive(Default)]
struct ConcurrencyHost {
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl WorkflowHost for ConcurrencyHost {
    fn call<'a>(
        &'a self,
        _call: WorkflowHostCall,
        cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
                () = cancellation.cancelled() => {}
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(json!(active))
        })
    }
}

#[tokio::test]
async fn graph_engine_enforces_max_concurrent_agents() {
    let workflow = graph_workflow(
        r#"    - { id: one, route: work }
    - { id: two, route: work }
    - { id: three, route: work }
"#,
        2,
    );
    let host = Arc::new(ConcurrencyHost::default());
    let run = start_graph(host.clone(), workflow, JsonValue::Null).await;

    assert_eq!(
        run.result().await.stop_reason,
        WorkflowStopReason::Completed
    );
    assert_eq!(host.maximum.load(Ordering::SeqCst), 2);
}

struct CancellingHost {
    started: Notify,
}

impl WorkflowHost for CancellingHost {
    fn call<'a>(
        &'a self,
        _call: WorkflowHostCall,
        cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            self.started.notify_one();
            cancellation.cancelled().await;
            Err(WorkflowError::Engine {
                engine: WorkflowEngine::GraphV1,
                message: "host observed cancellation".to_string(),
            })
        })
    }
}

#[tokio::test]
async fn graph_engine_propagates_cancellation_and_preserves_reason() {
    let workflow = graph_workflow("    - { id: work, route: work }\n", 1);
    let host = Arc::new(CancellingHost {
        started: Notify::new(),
    });
    let run = start_graph(host.clone(), workflow, JsonValue::Null).await;
    host.started.notified().await;
    run.cancel("stopped by user".to_string());

    let result = run.result().await;
    assert_eq!(result.stop_reason, WorkflowStopReason::Cancelled);
    assert_eq!(result.error.as_deref(), Some("stopped by user"));
}

#[tokio::test]
async fn graph_engine_returns_all_terminal_values_by_node_id() {
    let workflow = graph_workflow(
        r#"    - { id: left, route: work }
    - { id: right, route: work }
"#,
        2,
    );
    let host = Arc::new(DependencyHost::default());
    let run = start_graph(host, workflow, JsonValue::Null).await;

    assert_eq!(
        run.result().await.value,
        json!({"left": {"node": "left"}, "right": {"node": "right"}})
    );
}

struct FailingHost {
    calls: Mutex<Vec<String>>,
}

impl WorkflowHost for FailingHost {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        _cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(call.payload["id"].as_str().unwrap().to_string());
            Err(WorkflowError::Engine {
                engine: WorkflowEngine::GraphV1,
                message: "node failed".to_string(),
            })
        })
    }
}

#[tokio::test]
async fn graph_engine_does_not_dispatch_dependants_after_failure() {
    let workflow = graph_workflow(
        r#"    - { id: scan, route: work }
    - { id: build, route: work, needs: [scan] }
"#,
        2,
    );
    let host = Arc::new(FailingHost {
        calls: Mutex::new(Vec::new()),
    });
    let run = start_graph(host.clone(), workflow, JsonValue::Null).await;

    let result = run.result().await;
    assert_eq!(result.stop_reason, WorkflowStopReason::Failed);
    assert!(result.error.unwrap().contains("node failed"));
    assert_eq!(*host.calls.lock().unwrap(), vec!["scan"]);
}

#[tokio::test]
async fn graph_engine_enforces_total_wall_time() {
    let workflow = graph_workflow_with_wall_time("    - { id: work, route: work }\n", 1, Some(10));
    let host = Arc::new(CancellingHost {
        started: Notify::new(),
    });
    let run = start_graph(host, workflow, JsonValue::Null).await;

    let result = tokio::time::timeout(Duration::from_secs(1), run.result())
        .await
        .unwrap();
    assert_eq!(result.stop_reason, WorkflowStopReason::Failed);
    assert!(result.error.unwrap().contains("maxWallTimeMs (10)"));
}

#[test]
fn graph_engine_descriptor_is_static_and_replayable() {
    let engine = GraphWorkflowEngine::new(Arc::new(DependencyHost::default()));
    assert_eq!(
        engine.descriptor(),
        crate::WorkflowEngineDescriptor {
            engine: WorkflowEngine::GraphV1,
            dynamic_scripts: false,
            isolated_process: false,
            durable_replay: true,
        }
    );
}

struct WorkConservingHost {
    slow_started: Notify,
    release_slow: Notify,
    dependant_started: Notify,
}

impl WorkflowHost for WorkConservingHost {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        _cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            match call.payload["id"].as_str() {
                Some("slow") => {
                    self.slow_started.notify_one();
                    self.release_slow.notified().await;
                }
                Some("after-fast") => self.dependant_started.notify_one(),
                _ => {}
            }
            Ok(json!({"node": call.payload["id"]}))
        })
    }
}

#[tokio::test]
async fn graph_engine_refills_a_slot_without_waiting_for_the_whole_topological_stage() {
    let workflow = graph_workflow(
        r#"    - { id: slow, route: work }
    - { id: fast, route: work }
    - { id: after-fast, route: work, needs: [fast] }
"#,
        2,
    );
    let host = Arc::new(WorkConservingHost {
        slow_started: Notify::new(),
        release_slow: Notify::new(),
        dependant_started: Notify::new(),
    });
    let run = start_graph(host.clone(), workflow, JsonValue::Null).await;
    host.slow_started.notified().await;

    tokio::time::timeout(Duration::from_secs(1), host.dependant_started.notified())
        .await
        .expect("dependent work should refill the fast task's slot while slow is still running");
    host.release_slow.notify_one();

    let result = run.result().await;
    assert_eq!(result.stop_reason, WorkflowStopReason::Completed);
    assert_eq!(result.agents_started, 3);
}
