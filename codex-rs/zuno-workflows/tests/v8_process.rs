use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zuno_workflows::V8WorkflowEngine;
use zuno_workflows::WorkflowCompileRequest;
use zuno_workflows::WorkflowDefinition;
use zuno_workflows::WorkflowEngineProvider;
use zuno_workflows::WorkflowError;
use zuno_workflows::WorkflowFormat;
use zuno_workflows::WorkflowFuture;
use zuno_workflows::WorkflowHost;
use zuno_workflows::WorkflowHostCall;
use zuno_workflows::WorkflowHostCallKind;
use zuno_workflows::WorkflowRunId;
use zuno_workflows::WorkflowStartRequest;
use zuno_workflows::WorkflowStopReason;

struct EchoHost;

impl WorkflowHost for EchoHost {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        _cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            Ok(match call.kind {
                WorkflowHostCallKind::Agent => {
                    json!({"backend":"real-code-mode-host","accepted":true})
                }
                WorkflowHostCallKind::Phase
                | WorkflowHostCallKind::Log
                | WorkflowHostCallKind::Checkpoint => JsonValue::Null,
            })
        })
    }
}

#[tokio::test]
async fn installed_code_mode_host_executes_a_dynamic_workflow() {
    let Some(program) = std::env::var_os("ZUNO_CODE_MODE_HOST").map(PathBuf::from) else {
        eprintln!("skipping: set ZUNO_CODE_MODE_HOST to run the process-level V8 workflow test");
        return;
    };
    let source = r#"apiVersion: zuno.workflow/v1
kind: Workflow
metadata: { name: process-v8, version: "1" }
spec:
  engine: javascript/v1
  routes: { review: { agentRef: reviewer } }
  script: |
    await phase("review");
    return await agent(args.task, { id: "review", route: "review" });
"#;
    let workflow = Arc::new(
        WorkflowDefinition::parse(source, WorkflowFormat::Yaml, "test://process-v8").unwrap(),
    );
    let engine = V8WorkflowEngine::new(
        Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
            program,
        )),
        Arc::new(EchoHost),
        true,
    );
    let compiled = engine
        .compile(WorkflowCompileRequest {
            workflow,
            resolved_script: None,
        })
        .await
        .unwrap();
    let run = engine
        .start(WorkflowStartRequest {
            compiled: Arc::new(compiled),
            run_id: WorkflowRunId::new("process-run").unwrap(),
            parent_thread_id: "parent-thread".to_string(),
            args: json!({"task":"review the implementation"}),
        })
        .await
        .unwrap();
    let result = run.result().await;
    assert_eq!(result.stop_reason, WorkflowStopReason::Completed);
    assert_eq!(
        result.value,
        json!({"backend":"real-code-mode-host","accepted":true})
    );
    run.dispose().await.unwrap();
}
