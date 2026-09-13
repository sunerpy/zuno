//! Job-scoped Skill discovery and loading. State service materializes only
//! currently active, reviewed documents through the native Skill tool.
use crate::*;
use std::sync::Arc;
use zuno_application::skill::{SkillExecutionReply, SkillExecutionRequest};
use zuno_engine::r#loop::{
    AvailableTools, DispatchRequest, DynamicContextRefresher, PreparedToolDispatch, ToolBlockKind,
    ToolDispatchResult, ToolDispatcher, TurnError,
};
use zuno_engine::state::TurnStateError;
use zuno_llm::cache::DynamicContext;
use zuno_tool::{Tool as _, ToolDefinition, ToolDynamicContextRefresh, ToolOutput};

#[derive(Clone)]
pub struct RemoteSkills {
    client: WorkerClient,
    execution: WorkerExecution,
}
impl WorkerClient {
    pub fn skills(&self, execution: &WorkerExecution) -> RemoteSkills {
        RemoteSkills {
            client: self.clone(),
            execution: execution.clone(),
        }
    }
}
impl RemoteSkills {
    async fn request(
        &self,
        request: SkillExecutionRequest,
    ) -> Result<SkillExecutionReply, TurnStateError> {
        if self.execution.boundary_started()
            || self.execution.deadline()? <= tokio::time::Instant::now()
        {
            return Err(TurnStateError::LeaseLost);
        }
        let grant = self
            .execution
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        let data = serde_json::to_vec(&request).map_err(|_| TurnStateError::InvalidData)?;
        if data.len() > 65536 {
            return Err(TurnStateError::InvalidData);
        }
        let data = self.client.post(SKILL_PATH, Some(&grant), data).await?;
        if self.execution.boundary_started()
            || self.execution.deadline()? <= tokio::time::Instant::now()
        {
            return Err(TurnStateError::LeaseLost);
        }
        serde_json::from_slice(&data).map_err(|_| TurnStateError::InvalidData)
    }
}
#[derive(Clone)]
pub struct SkillToolDispatcher {
    inner: Arc<dyn ToolDispatcher>,
    service: RemoteSkills,
    definition: ToolDefinition,
}
impl SkillToolDispatcher {
    pub fn new(inner: Arc<dyn ToolDispatcher>, service: RemoteSkills) -> Self {
        let tool = zuno_tool::Typed(zuno_tools::SkillTool::new(Arc::new(
            zuno_catalog::skill::Skills::default(),
        )));
        Self {
            inner,
            service,
            definition: tool.definition(),
        }
    }
}
#[async_trait]
impl ToolDispatcher for SkillToolDispatcher {
    fn available_tools(&self) -> AvailableTools {
        let mut value = self.inner.available_tools();
        value.definitions.push(self.definition.clone());
        value
    }
    fn concurrency_policy(&self, request: &DispatchRequest) -> zuno_tool::ToolConcurrencyPolicy {
        self.inner.concurrency_policy(request)
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.name != self.definition.id {
            return self.inner.prepare(request).await;
        }
        let execution = &self.service.execution;
        if request.session_id != execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &execution.job.principal
            || request
                .orchestration_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.turn_id != execution.job.turn_id.as_str())
            || !request
                .available_tools
                .iter()
                .any(|definition| crate::definition_matches(definition, &self.definition))
            || request.call.input_error.is_some()
            || request.interrupt.is_set()
        {
            return PreparedToolDispatch::ready(ToolDispatchResult::blocked(
                ToolOutput::text("Skill", "Skill invocation is outside this execution."),
                ToolBlockKind::Denied,
            ));
        }
        let Ok(invocation_id) = zuno_types::identity::InvocationId::new(&request.call.id) else {
            return PreparedToolDispatch::ready(ToolDispatchResult::blocked(
                ToolOutput::text("Skill", "Invalid invocation identity."),
                ToolBlockKind::InvalidArguments,
            ));
        };
        let service = self.service.clone();
        PreparedToolDispatch::new(Box::pin(async move {
            if request.interrupt.is_set() {
                return ToolDispatchResult::blocked(
                    ToolOutput::text("Skill", "The invocation was interrupted before submission."),
                    ToolBlockKind::Denied,
                );
            }
            match service
                .request(SkillExecutionRequest::Invoke {
                    invocation_id,
                    arguments: request.call.input,
                })
                .await
            {
                Ok(SkillExecutionReply::Output {
                    output,
                    is_error: false,
                }) => ToolDispatchResult::success(*output),
                Ok(SkillExecutionReply::Output {
                    output,
                    is_error: true,
                }) => ToolDispatchResult::error(*output),
                Err(TurnStateError::Forbidden | TurnStateError::LeaseLost) => {
                    ToolDispatchResult::blocked(
                        ToolOutput::text(
                            "Skill",
                            "Current execution does not authorize this Skill.",
                        ),
                        ToolBlockKind::Denied,
                    )
                }
                Err(TurnStateError::Conflict) => ToolDispatchResult::blocked(
                    ToolOutput::text(
                        "Skill",
                        "The active Skill catalog changed; discover the current source before loading.",
                    ),
                    ToolBlockKind::Conflict,
                ),
                _ => ToolDispatchResult::blocked(
                    ToolOutput::text("Skill", "The Skill state service is unavailable."),
                    ToolBlockKind::Unavailable,
                ),
            }
        }))
    }
}
pub struct SkillContextRefresher {
    pub inner: Arc<dyn DynamicContextRefresher>,
    pub service: RemoteSkills,
    pub session_id: String,
}
impl SkillContextRefresher {
    async fn append(
        &self,
        session: &str,
        base: DynamicContext,
    ) -> Result<DynamicContext, TurnError> {
        if session != self.session_id {
            return Err(TurnStateError::Forbidden.into());
        }
        let reply = self.service.request(SkillExecutionRequest::Catalog).await?;
        let SkillExecutionReply::Catalog { index, .. } = reply else {
            return Err(TurnStateError::InvalidData.into());
        };
        Ok(base.with_runtime_instruction(index))
    }
}
#[async_trait]
impl DynamicContextRefresher for SkillContextRefresher {
    async fn before_request(&self, session: &str) -> Result<Option<DynamicContext>, TurnError> {
        let base = self
            .inner
            .before_request(session)
            .await?
            .unwrap_or_default();
        self.append(session, base).await.map(Some)
    }
    async fn refresh(
        &self,
        session: &str,
        refresh: ToolDynamicContextRefresh,
    ) -> Result<DynamicContext, TurnError> {
        let base = self.inner.refresh(session, refresh).await?;
        self.append(session, base).await
    }
}
