use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn script_yaml(script: &str) -> String {
    format!(
        r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata:
  name: user-flow
  version: 1.0.0
  description: User-owned workflow
spec:
  engine: javascript/v1
  routes:
    review:
      agentRef: reviewer
  limits:
    maxConcurrentAgents: 2
    maxTotalAgents: 5
    maxItemsPerCall: 10
  script: |
    {script}
"#
    )
}

#[test]
fn parses_dynamic_yaml_and_binds_exact_source_identity() {
    let source = script_yaml("return await agent(args.task, { id: 'review', route: 'review' });");
    let workflow =
        WorkflowDefinition::parse(&source, WorkflowFormat::Yaml, "user://flows/user-flow")
            .expect("valid workflow");
    assert_eq!(
        workflow.definition().spec.engine,
        WorkflowEngine::JavaScriptV1
    );
    assert_eq!(workflow.identity().source, "user://flows/user-flow");
    assert_eq!(workflow.identity().name, "user-flow");
    assert_eq!(workflow.identity().digest.len(), 64);
    assert_eq!(workflow.graph(), None);
}

#[test]
fn optional_repository_template_is_user_owned_and_model_neutral() {
    let source = include_str!("../../../examples/zuno-workflows/design-review.yaml");
    let workflow =
        WorkflowDefinition::parse(source, WorkflowFormat::Yaml, "example://design-review")
            .expect("valid optional workflow template");
    let definition = workflow.definition();
    assert_eq!(definition.metadata.name, "design-review");
    assert_eq!(definition.spec.engine, WorkflowEngine::GraphV1);
    assert_eq!(definition.spec.routes["design"].agent_ref, "claude-code");
    assert_eq!(
        definition.spec.routes["review"]
            .execution_profile
            .as_deref(),
        Some("review-agent")
    );
    assert!(!source.contains("gpt-"));
    assert!(!source.to_ascii_lowercase().contains("opus"));
    assert!(!source.contains("kiro-local"));
}

#[test]
fn content_digest_changes_with_exact_source_bytes() {
    let source = script_yaml("return args;");
    let first = WorkflowDefinition::parse(&source, WorkflowFormat::Yaml, "user://flow").unwrap();
    let second =
        WorkflowDefinition::parse(&(source + "\n"), WorkflowFormat::Yaml, "user://flow").unwrap();
    assert_ne!(first.identity().digest, second.identity().digest);
}

#[test]
fn compiles_graph_dependencies_and_routes() {
    let source = r#"{
      "apiVersion":"zuno.workflow/v1","kind":"Workflow",
      "metadata":{"name":"graph","version":"1"},
      "spec":{"engine":"graph/v1","routes":{"scan":{"agentRef":"explorer"},"build":{"agentRef":"builder"}},
      "nodes":[{"id":"scan","route":"scan"},{"id":"build","route":"build","needs":["scan"]}]}
    }"#;
    let workflow =
        WorkflowDefinition::parse(source, WorkflowFormat::Json, "project://graph").unwrap();
    let graph = workflow.graph().expect("compiled graph");
    assert_eq!(graph.stages(), &[vec![0], vec![1]]);
    assert_eq!(graph.dependencies(1), Some([0].as_slice()));
    assert_eq!(graph.terminal_nodes(), &[1]);
}

#[test]
fn rejects_engine_program_mismatches_and_unsafe_paths() {
    let both = script_yaml("return args;")
        .replace("  script: |", "  scriptFile: ../escape.js\n  script: |");
    assert!(matches!(
        WorkflowDefinition::parse(&both, WorkflowFormat::Yaml, "user://flow"),
        Err(WorkflowError::InvalidProgram { .. })
    ));

    let unsafe_path = script_yaml("return args;").replace(
        "  script: |\n    return args;",
        "  scriptFile: ../escape.js",
    );
    assert_eq!(
        WorkflowDefinition::parse(&unsafe_path, WorkflowFormat::Yaml, "user://flow"),
        Err(WorkflowError::UnsafeScriptPath("../escape.js".to_string()))
    );
}

#[test]
fn rejects_unknown_routes_cycles_and_unsafe_limits() {
    let unknown = r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata: { name: graph, version: "1" }
spec:
  engine: graph/v1
  nodes: [{ id: scan, route: absent }]
"#;
    assert!(matches!(
        WorkflowDefinition::parse(unknown, WorkflowFormat::Yaml, "project://graph"),
        Err(WorkflowError::UnknownRoute { .. })
    ));

    let cycle = r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata: { name: graph, version: "1" }
spec:
  engine: graph/v1
  routes: { work: { agentRef: worker } }
  nodes:
    - { id: first, route: work, needs: [second] }
    - { id: second, route: work, needs: [first] }
"#;
    assert_eq!(
        WorkflowDefinition::parse(cycle, WorkflowFormat::Yaml, "project://graph"),
        Err(WorkflowError::DependencyCycle)
    );

    let unsafe_limits =
        script_yaml("return args;").replace("maxConcurrentAgents: 2", "maxConcurrentAgents: 6");
    assert!(matches!(
        WorkflowDefinition::parse(&unsafe_limits, WorkflowFormat::Yaml, "user://flow"),
        Err(WorkflowError::ParallelismExceedsTotal { .. })
    ));
}

#[test]
fn workflow_call_digest_is_key_order_independent_and_fails_closed_on_drift() {
    let recorded =
        WorkflowCallIdentity::new("design", &json!({"b": 2, "a": {"y": 2, "x": 1}})).unwrap();
    recorded
        .verify_replay(&json!({"a": {"x": 1, "y": 2}, "b": 2}))
        .unwrap();
    assert!(matches!(
        recorded.verify_replay(&json!({"a": {"x": 9, "y": 2}, "b": 2})),
        Err(WorkflowError::ReplayDiverged { .. })
    ));
}

#[test]
fn engine_provider_contract_is_object_safe() {
    fn accepts_provider(_: &dyn WorkflowEngineProvider) {}
    let _ = accepts_provider;
}

#[derive(Default)]
struct RecordingHost {
    calls: std::sync::Mutex<Vec<WorkflowHostCall>>,
}

impl WorkflowHost for RecordingHost {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> WorkflowFuture<'a, Result<serde_json::Value, WorkflowError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call.clone());
            Ok(match call.kind {
                WorkflowHostCallKind::Agent => json!({"answer": "reviewed"}),
                WorkflowHostCallKind::Phase
                | WorkflowHostCallKind::Log
                | WorkflowHostCallKind::Checkpoint => serde_json::Value::Null,
            })
        })
    }
}

struct FakeV8Provider;

struct FakeV8Session {
    delegate: std::sync::Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
}

impl codex_code_mode::CodeModeSession for FakeV8Session {
    fn execute<'a>(
        &'a self,
        request: codex_code_mode::ExecuteRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell> {
        let delegate = self.delegate.clone();
        Box::pin(async move {
            let missing = request.source.contains("route: 'missing'");
            let route = if missing { "missing" } else { "review" };
            let invocation = codex_code_mode::CodeModeNestedToolCall {
                cell_id: codex_code_mode::CellId::new("cell-1".to_string()),
                runtime_tool_call_id: "call-1".to_string(),
                tool_name: codex_protocol::ToolName::plain("workflow_agent"),
                tool_kind: codex_code_mode::CodeModeToolKind::Function,
                input: Some(json!({"id":"review","route":route,"prompt":"review"})),
            };
            Ok(codex_code_mode::StartedCell::from_future(
                codex_code_mode::CellId::new("cell-1".to_string()),
                async move {
                    match delegate
                        .invoke_tool(invocation, tokio_util::sync::CancellationToken::new())
                        .await
                    {
                        Ok(value) => Ok(codex_code_mode::RuntimeResponse::Result {
                            cell_id: codex_code_mode::CellId::new("cell-1".to_string()),
                            content_items: vec![
                                codex_code_mode::FunctionCallOutputContentItem::InputText {
                                    text: serde_json::to_string(&value).unwrap(),
                                },
                            ],
                            error_text: None,
                            code_mode_host_duration: None,
                        }),
                        Err(error) => Ok(codex_code_mode::RuntimeResponse::Result {
                            cell_id: codex_code_mode::CellId::new("cell-1".to_string()),
                            content_items: Vec::new(),
                            error_text: Some(error),
                            code_mode_host_duration: None,
                        }),
                    }
                },
            ))
        })
    }

    fn wait<'a>(
        &'a self,
        request: codex_code_mode::WaitRequest,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async move {
            Ok(codex_code_mode::WaitOutcome::MissingCell(
                codex_code_mode::RuntimeResponse::Terminated {
                    cell_id: request.cell_id,
                    content_items: Vec::new(),
                    code_mode_host_duration: None,
                },
            ))
        })
    }

    fn terminate<'a>(
        &'a self,
        cell_id: codex_code_mode::CellId,
    ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome> {
        Box::pin(async move {
            Ok(codex_code_mode::WaitOutcome::LiveCell(
                codex_code_mode::RuntimeResponse::Terminated {
                    cell_id,
                    content_items: Vec::new(),
                    code_mode_host_duration: None,
                },
            ))
        })
    }

    fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

impl codex_code_mode::CodeModeSessionProvider for FakeV8Provider {
    fn create_session<'a>(
        &'a self,
        delegate: std::sync::Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            Ok(std::sync::Arc::new(FakeV8Session { delegate })
                as std::sync::Arc<dyn codex_code_mode::CodeModeSession>)
        })
    }

    fn create_session_with_limits<'a>(
        &'a self,
        delegate: std::sync::Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
        _limits: codex_code_mode::CodeModeSessionCellExecutionLimits,
    ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
        self.create_session(delegate)
    }
}

async fn run_v8_script(script: &str) -> (WorkflowResult, std::sync::Arc<RecordingHost>) {
    let source = script_yaml(script);
    let workflow = std::sync::Arc::new(
        WorkflowDefinition::parse(&source, WorkflowFormat::Yaml, "user://flow").unwrap(),
    );
    let host = std::sync::Arc::new(RecordingHost::default());
    let engine = V8WorkflowEngine::new(std::sync::Arc::new(FakeV8Provider), host.clone(), false);
    let compiled = engine
        .compile(WorkflowCompileRequest {
            workflow,
            resolved_script: None,
        })
        .await
        .unwrap();
    let run = engine
        .start(WorkflowStartRequest {
            compiled: std::sync::Arc::new(compiled),
            run_id: WorkflowRunId::new("run-1").unwrap(),
            parent_thread_id: "thread-1".to_string(),
            args: json!({"task": "review"}),
        })
        .await
        .unwrap();
    (run.result().await, host)
}

#[tokio::test]
async fn v8_engine_executes_restricted_script_and_calls_host_agent() {
    let (result, host) =
        run_v8_script("return await agent(args.task, { id: 'review', route: 'review' });").await;
    assert_eq!(
        result,
        WorkflowResult {
            value: json!({"answer": "reviewed"}),
            stop_reason: WorkflowStopReason::Completed,
            error: None,
            agents_started: 1,
        }
    );
    let calls = host.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].kind, WorkflowHostCallKind::Agent);
    assert_eq!(calls[0].identity.as_ref().unwrap().id, "review");
}

#[tokio::test]
async fn v8_engine_rejects_unknown_routes_without_dispatch() {
    let (result, host) =
        run_v8_script("return await agent(args.task, { id: 'review', route: 'missing' });").await;
    assert_eq!(result.stop_reason, WorkflowStopReason::Failed);
    assert!(result.error.unwrap().contains("unknown route"));
    assert!(host.calls.lock().unwrap().is_empty());
}
