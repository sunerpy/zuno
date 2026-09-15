use anyhow::Result;
use codex_agent_extension::AgentRunner;
use codex_agent_extension::NativeCodexBackend;
use codex_agent_extension::OneShotAgentBackend;
use codex_agent_extension::OneShotAgentBackendKind;
use codex_agent_extension::OneShotAgentRequest;
use codex_agent_extension::OneShotAgentResult;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_backend_returns_authoritative_turn_completion() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("agent-response"),
            responses::ev_assistant_message("agent-message", "native answer"),
            responses::ev_completed("agent-response"),
        ]),
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let parent_thread_id = test.session_configured.session_id.into();
    let backend = NativeCodexBackend::new(
        AgentRunner::new(std::sync::Arc::downgrade(&test.thread_manager)),
        parent_thread_id,
        test.config.clone(),
    );

    let result = backend
        .run(
            OneShotAgentRequest {
                prompt: "solve the delegated task".to_string(),
                cwd: test.config.cwd.clone(),
            },
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(
        result,
        OneShotAgentResult {
            backend: OneShotAgentBackendKind::NativeCodex,
            final_answer: "native answer".to_string(),
            product_session_id: result.product_session_id.clone(),
        }
    );
    assert!(result.product_session_id.is_some());
    Ok(())
}
