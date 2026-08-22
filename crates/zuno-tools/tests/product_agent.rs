use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use zuno_error::ToolError;
use zuno_tool::{
    AllowAll, InterruptHandle, NeverInterrupted, PermissionAsk, PermissionAsker, Tool, ToolContext,
    ToolReplayPolicy, ToolUiIntent, erase,
};
use zuno_tools::product_agent::{
    ProductAgentHost, ProductAgentRequest, ProductAgentTool, ProductAgentTurn,
};

const PARENT: &str = "ses_parent";

struct RecordingHost {
    requests: Mutex<Vec<ProductAgentRequest>>,
    result: Mutex<Option<Result<ProductAgentTurn, String>>>,
}

impl RecordingHost {
    fn returning(result: Result<ProductAgentTurn, String>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            result: Mutex::new(Some(result)),
        }
    }

    fn requests(&self) -> Vec<ProductAgentRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ProductAgentHost for RecordingHost {
    async fn dispatch(
        &self,
        request: ProductAgentRequest,
        _cancellation: CancellationToken,
    ) -> Result<ProductAgentTurn, String> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        self.result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("one configured result")
    }
}

#[derive(Default)]
struct RecordingPermission(Mutex<Vec<PermissionAsk>>);

#[async_trait]
impl PermissionAsker for RecordingPermission {
    async fn ask(&self, _tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ask);
        Ok(())
    }
}

struct CancellationHost;

#[async_trait]
impl ProductAgentHost for CancellationHost {
    async fn dispatch(
        &self,
        _request: ProductAgentRequest,
        cancellation: CancellationToken,
    ) -> Result<ProductAgentTurn, String> {
        cancellation.cancelled().await;
        Err("native invocation cancelled".to_owned())
    }
}

struct FirableInterrupt(CancellationToken);

#[async_trait]
impl InterruptHandle for FirableInterrupt {
    fn is_set(&self) -> bool {
        self.0.is_cancelled()
    }

    async fn notified(&self) {
        self.0.cancelled().await;
    }
}

fn context(
    permission: Arc<dyn PermissionAsker>,
    interrupt: Arc<dyn InterruptHandle>,
) -> ToolContext {
    ToolContext::new(
        PARENT,
        "msg_parent",
        "call_product",
        "build",
        permission,
        interrupt,
    )
}

fn erased(host: Arc<dyn ProductAgentHost>) -> Arc<dyn Tool> {
    erase(ProductAgentTool::new(
        "subagent_codex",
        "reviewer",
        "codex",
        host,
    ))
}

#[test]
fn each_instance_is_a_static_non_replayable_subagent_tool() {
    let tool = erased(Arc::new(RecordingHost::returning(Ok(ProductAgentTurn {
        run_id: "run_unused".to_owned(),
        job_id: None,
        output: "unused".to_owned(),
    }))));

    assert_eq!(tool.id(), "subagent_codex");
    assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
    assert_eq!(tool.ui_intent(), ToolUiIntent::Subagent);
    let definition = tool.definition();
    assert_eq!(definition.ui_intent, ToolUiIntent::Subagent);
    for field in ["prompt", "description", "background", "reportDelivery"] {
        assert!(
            definition.parameters["properties"].get(field).is_some(),
            "missing `{field}` from {}",
            definition.parameters
        );
    }
}

#[tokio::test]
async fn foreground_dispatch_carries_identity_and_uses_the_product_envelope() {
    let host = Arc::new(RecordingHost::returning(Ok(ProductAgentTurn {
        run_id: "run_1".to_owned(),
        job_id: None,
        output: "native final answer".to_owned(),
    })));
    let permission = Arc::new(RecordingPermission::default());
    let output = erased(Arc::clone(&host) as Arc<dyn ProductAgentHost>)
        .invoke(
            json!({
                "prompt":"review the patch",
                "description":"review",
                "background":false
            }),
            context(
                Arc::clone(&permission) as Arc<dyn PermissionAsker>,
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect("foreground product agent");

    assert!(
        output.output.contains("product=\"codex\""),
        "{}",
        output.output
    );
    assert!(
        output.output.contains("instance=\"reviewer\""),
        "{}",
        output.output
    );
    assert!(
        output.output.contains("native final answer"),
        "{}",
        output.output
    );
    let requests = host.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].parent_session_id, PARENT);
    assert_eq!(requests[0].instance, "reviewer");
    assert_eq!(requests[0].product, "codex");
    assert_eq!(requests[0].tool, "subagent_codex");
    assert!(!requests[0].background);
    let asks = permission
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(asks[0].permission, "task");
    assert_eq!(asks[0].patterns, ["product:reviewer"]);
}

#[tokio::test]
async fn background_dispatch_requires_and_renders_a_distinct_job_id() {
    let host = Arc::new(RecordingHost::returning(Ok(ProductAgentTurn {
        run_id: "run_2".to_owned(),
        job_id: Some("job_2".to_owned()),
        output: "started".to_owned(),
    })));
    let output = erased(Arc::clone(&host) as Arc<dyn ProductAgentHost>)
        .invoke(
            json!({
                "prompt":"inspect the workspace",
                "background":true,
                "reportDelivery":"quiet"
            }),
            context(Arc::new(AllowAll), Arc::new(NeverInterrupted)),
        )
        .await
        .expect("background product agent");

    assert!(output.output.contains("job=\"job_2\""), "{}", output.output);
    assert!(
        output.output.contains("state=\"running\""),
        "{}",
        output.output
    );
    assert!(
        output.output.contains("reportDelivery=\"quiet\""),
        "{}",
        output.output
    );
    let requests = host.requests();
    assert!(requests[0].background);
    assert_eq!(
        requests[0].report_delivery,
        zuno_tools::task::ReportDelivery::Quiet
    );
}

#[tokio::test]
async fn invalid_delivery_is_rejected_before_permission_or_dispatch() {
    let host = Arc::new(RecordingHost::returning(Ok(ProductAgentTurn {
        run_id: "run_never".to_owned(),
        job_id: None,
        output: "never".to_owned(),
    })));
    let permission = Arc::new(RecordingPermission::default());
    let error = erased(Arc::clone(&host) as Arc<dyn ProductAgentHost>)
        .invoke(
            json!({"prompt":"work","reportDelivery":"nextStep"}),
            context(
                Arc::clone(&permission) as Arc<dyn PermissionAsker>,
                Arc::new(NeverInterrupted),
            ),
        )
        .await
        .expect_err("foreground report delivery is invalid");

    assert!(error.is_model_correctable(), "{error}");
    assert!(host.requests().is_empty());
    assert!(
        permission
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test]
async fn foreground_interrupt_cancels_the_native_dispatch() {
    let interrupt = CancellationToken::new();
    let run = tokio::spawn({
        let interrupt_handle = Arc::new(FirableInterrupt(interrupt.clone()));
        async move {
            erased(Arc::new(CancellationHost))
                .invoke(
                    json!({"prompt":"wait"}),
                    context(Arc::new(AllowAll), interrupt_handle),
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    interrupt.cancel();
    let error = run
        .await
        .expect("tool task")
        .expect_err("interrupt must fail the invocation");

    let message = match &error {
        ToolError::Failed { source, .. } => source.to_string(),
        other => other.to_string(),
    };
    assert!(message.contains("native invocation cancelled"), "{message}");
}
