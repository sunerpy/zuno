use super::DEFAULT_DISPOSE_GRACE;
use super::OneShotAgentBackend;
use super::OneShotAgentBackendKind;
use super::OneShotAgentError;
use super::OneShotAgentFailureCategory;
use super::OneShotAgentFailureStage;
use super::OneShotAgentFuture;
use super::OneShotAgentRequest;
use super::OneShotAgentResult;
use super::validate_request;
use crate::AgentInvocation;
use crate::AgentRunner;
use codex_core::config::Config;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::W3cTraceContext;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Native Codex provider backed by the host's existing thread manager.
///
/// The config template can select a model provider, model, reasoning effort,
/// service tier, tools, and permission profile. Every run forks a fresh child
/// thread from the configured parent and never launches a second Codex CLI.
#[derive(Clone)]
pub struct NativeCodexBackend {
    runner: AgentRunner,
    parent_thread_id: ThreadId,
    config: Config,
    parent_trace: Option<W3cTraceContext>,
}

impl NativeCodexBackend {
    pub fn new(runner: AgentRunner, parent_thread_id: ThreadId, config: Config) -> Self {
        Self {
            runner,
            parent_thread_id,
            config,
            parent_trace: None,
        }
    }

    pub fn with_parent_trace(mut self, parent_trace: Option<W3cTraceContext>) -> Self {
        self.parent_trace = parent_trace;
        self
    }
}

impl OneShotAgentBackend for NativeCodexBackend {
    fn kind(&self) -> OneShotAgentBackendKind {
        OneShotAgentBackendKind::NativeCodex
    }

    fn capabilities(&self) -> crate::AgentBackendCapabilities {
        crate::AgentBackendCapabilities::native_codex()
    }

    fn run<'a>(
        &'a self,
        request: OneShotAgentRequest,
        cancellation: CancellationToken,
    ) -> OneShotAgentFuture<'a> {
        Box::pin(async move {
            validate_request(self.kind(), &request)?;
            let mut config = self.config.clone();
            config.cwd = request.cwd;
            let run = self
                .runner
                .start(
                    self.parent_thread_id,
                    AgentInvocation {
                        config,
                        prompt: request.prompt,
                        parent_trace: self.parent_trace.clone(),
                    },
                )
                .await
                .map_err(|_| {
                    OneShotAgentError::new(
                        self.kind(),
                        OneShotAgentFailureStage::Start,
                        OneShotAgentFailureCategory::ProductError,
                    )
                })?;

            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        let _ = run.thread.submit(Op::Interrupt).await;
                        let _ = timeout(DEFAULT_DISPOSE_GRACE, run.thread.shutdown_and_wait()).await;
                        return Err(OneShotAgentError::new(
                            self.kind(),
                            OneShotAgentFailureStage::Run,
                            OneShotAgentFailureCategory::Aborted,
                        ));
                    }
                    event = run.thread.next_event() => event.map_err(|_| {
                        OneShotAgentError::new(
                            self.kind(),
                            OneShotAgentFailureStage::Run,
                            OneShotAgentFailureCategory::ProductError,
                        )
                    })?,
                };

                match event.msg {
                    EventMsg::TurnComplete(completed) if completed.turn_id == run.turn_id => {
                        if completed.error.is_some() {
                            return Err(OneShotAgentError::new(
                                self.kind(),
                                OneShotAgentFailureStage::Run,
                                OneShotAgentFailureCategory::ProductError,
                            ));
                        }
                        let Some(final_answer) = completed
                            .last_agent_message
                            .filter(|message| !message.trim().is_empty())
                        else {
                            return Err(OneShotAgentError::new(
                                self.kind(),
                                OneShotAgentFailureStage::Decode,
                                OneShotAgentFailureCategory::InvalidResult,
                            ));
                        };
                        return Ok(OneShotAgentResult {
                            backend: self.kind(),
                            final_answer,
                            product_session_id: Some(run.thread_id.to_string()),
                        });
                    }
                    EventMsg::AgentMessage(message)
                        if message.phase == Some(MessagePhase::FinalAnswer) =>
                    {
                        // TurnComplete is authoritative and carries the same final text.
                    }
                    EventMsg::TurnAborted(aborted)
                        if aborted.turn_id.as_deref() == Some(run.turn_id.as_str()) =>
                    {
                        return Err(OneShotAgentError::new(
                            self.kind(),
                            OneShotAgentFailureStage::Run,
                            OneShotAgentFailureCategory::Aborted,
                        ));
                    }
                    EventMsg::Error(_) => {
                        return Err(OneShotAgentError::new(
                            self.kind(),
                            OneShotAgentFailureStage::Run,
                            OneShotAgentFailureCategory::ProductError,
                        ));
                    }
                    _ => {}
                }
            }
        })
    }
}
