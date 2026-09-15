use super::backend_factory::WorkflowAgentBackendContext;
use super::backend_factory::WorkflowAgentBackendFactories;
use super::binding::PreparedWorkflowAgentBinding;
use super::binding::WorkflowAgentBinding;
use super::binding::WorkflowAgentBindingRequest;
use crate::config_manager::ConfigManager;
use codex_agent_extension::AgentBackendFactoryError;
use codex_agent_extension::OneShotAgentError;
use codex_agent_extension::OneShotAgentFailureCategory;
use codex_agent_extension::OneShotAgentFailureStage;
use codex_agent_extension::OneShotAgentRequest;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_protocol::ThreadId;
use serde_json::Value as JsonValue;
use serde_json::json;
use std::fmt;
use std::sync::Weak;
use tokio_util::sync::CancellationToken;
use zuno_workflows::WorkflowFuture;
use zuno_workflows::WorkflowRunId;

/// Product-neutral request accepted by a configured workflow agent route.
///
/// The app server owns durable admission and replay. A dispatcher only performs
/// the newly admitted call and must honor cancellation. Implementations can use
/// in-process Codex children, Claude Code, ACP, or another installed backend.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct WorkflowAgentDispatchRequest {
    pub(crate) run_id: WorkflowRunId,
    pub(crate) call_id: String,
    pub(crate) parent_thread_id: String,
    pub(crate) route: String,
    pub(crate) agent_ref: String,
    pub(crate) execution_profile: Option<String>,
    pub(crate) prompt: String,
    pub(crate) options: JsonValue,
}

/// Typed AgentBackend outcome. Transport loss after dispatch is `Uncertain`,
/// never a generic failure that a caller may mechanically replay.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowAgentDispatchError {
    Failed(String),
    Cancelled(String),
    Uncertain(String),
}

impl fmt::Display for WorkflowAgentDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(message) | Self::Cancelled(message) | Self::Uncertain(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for WorkflowAgentDispatchError {}

pub(crate) trait WorkflowAgentDispatcher: Send + Sync {
    /// Resolve one route without starting product work. The result is persisted
    /// before workflow admission and verified again immediately before dispatch.
    fn prepare<'a>(
        &'a self,
        request: WorkflowAgentBindingRequest,
    ) -> WorkflowFuture<'a, Result<PreparedWorkflowAgentBinding, WorkflowAgentDispatchError>>;

    fn dispatch<'a>(
        &'a self,
        prepared: PreparedWorkflowAgentBinding,
        request: WorkflowAgentDispatchRequest,
        cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowAgentDispatchError>>;
}

/// Built-in generic Agent backends. The route still belongs to the workflow;
/// this dispatcher only resolves its backend and optional Profile v2 config.
/// Plugins may replace the dispatcher through `new_with_agent_dispatcher`.
pub(super) struct NativeWorkflowAgentDispatcher {
    backend_factories: WorkflowAgentBackendFactories,
    config_manager: ConfigManager,
    thread_manager: Weak<ThreadManager>,
}

impl NativeWorkflowAgentDispatcher {
    pub(super) fn new(thread_manager: Weak<ThreadManager>, config_manager: ConfigManager) -> Self {
        Self {
            backend_factories: WorkflowAgentBackendFactories::with_native_providers(),
            config_manager,
            thread_manager,
        }
    }

    async fn parent_and_config(
        &self,
        request: &WorkflowAgentBindingRequest,
    ) -> Result<(ThreadId, Config), WorkflowAgentDispatchError> {
        let parent_thread_id =
            ThreadId::from_string(&request.parent_thread_id).map_err(|error| {
                WorkflowAgentDispatchError::Failed(format!(
                    "workflow parent thread id is invalid: {error}"
                ))
            })?;
        let thread_manager = self.thread_manager.upgrade().ok_or_else(|| {
            WorkflowAgentDispatchError::Failed("workflow thread manager is unavailable".to_string())
        })?;
        let parent = thread_manager
            .get_thread(parent_thread_id)
            .await
            .map_err(|_| {
                WorkflowAgentDispatchError::Failed(format!(
                    "workflow parent thread `{parent_thread_id}` is not loaded"
                ))
            })?;
        let snapshot = parent.config_snapshot().await;
        let cwd = snapshot.cwd().clone();
        let mut config = match request.execution_profile.as_deref() {
            Some(profile) => {
                self.config_manager
                    .load_execution_profile(profile, cwd.to_path_buf())
                    .await
            }
            None => {
                self.config_manager
                    .load_for_cwd(
                        /*request_overrides*/ None,
                        ConfigOverrides {
                            model: Some(snapshot.model.clone()),
                            cwd: Some(cwd.to_path_buf()),
                            approval_policy: Some(snapshot.approval_policy),
                            approvals_reviewer: Some(snapshot.approvals_reviewer),
                            permission_profile: Some(snapshot.permission_profile.clone()),
                            model_provider: Some(snapshot.model_provider_id.clone()),
                            service_tier: Some(snapshot.service_tier.clone()),
                            workspace_roots: Some(snapshot.workspace_roots.clone()),
                            ..ConfigOverrides::default()
                        },
                        Some(cwd.to_path_buf()),
                    )
                    .await
            }
        }
        .map_err(|error| {
            WorkflowAgentDispatchError::Failed(format!(
                "workflow execution profile could not be loaded: {error}"
            ))
        })?;
        if request.execution_profile.is_none() {
            // This is an already validated selection inherited from the parent
            // thread; ConfigOverrides does not expose reasoning effort.
            config.model_reasoning_effort = snapshot.reasoning_effort;
        }
        Ok((parent_thread_id, config))
    }

    async fn prepare_binding(
        &self,
        request: &WorkflowAgentBindingRequest,
    ) -> Result<PreparedWorkflowAgentBinding, WorkflowAgentDispatchError> {
        let (parent_thread_id, config) = self.parent_and_config(request).await?;
        let thread_manager = self.thread_manager.upgrade().ok_or_else(|| {
            WorkflowAgentDispatchError::Failed("workflow thread manager is unavailable".to_string())
        })?;
        let plugin_backends = thread_manager
            .plugins_manager()
            .plugins_for_config_fresh(&config.plugins_config_input())
            .await
            .effective_plugin_agent_backends();
        let factory = self
            .backend_factories
            .resolve(&request.agent_ref, &config, plugin_backends)
            .map_err(|error| {
                agent_backend_factory_error(AgentBackendFactoryError::Registry(error))
            })?;
        let binding = WorkflowAgentBinding::resolved(
            request,
            factory.descriptor(),
            factory.plugin(),
            factory.environment_digest(),
            &config,
        )
        .map_err(WorkflowAgentDispatchError::Failed)?;
        let cwd = config.cwd.clone();
        let context = WorkflowAgentBackendContext {
            thread_manager,
            parent_thread_id,
            config,
        };
        let backend = factory
            .build(&context)
            .map_err(agent_backend_factory_error)?;
        Ok(PreparedWorkflowAgentBinding::new(binding, backend, cwd))
    }

    async fn dispatch_inner(
        &self,
        prepared: PreparedWorkflowAgentBinding,
        request: WorkflowAgentDispatchRequest,
        cancellation: CancellationToken,
    ) -> Result<JsonValue, WorkflowAgentDispatchError> {
        let result = prepared
            .backend()
            .run(
                OneShotAgentRequest {
                    prompt: request.prompt,
                    cwd: prepared.cwd().clone(),
                },
                cancellation,
            )
            .await
            .map_err(agent_backend_error)?;

        Ok(json!({
            "answer": result.final_answer,
            "backend": result.backend.to_string(),
            "sessionId": result.product_session_id,
        }))
    }
}

fn agent_backend_factory_error(error: AgentBackendFactoryError) -> WorkflowAgentDispatchError {
    match error {
        AgentBackendFactoryError::Backend(error) => agent_backend_error(error),
        AgentBackendFactoryError::Registry(_)
        | AgentBackendFactoryError::ContractMismatch { .. } => {
            WorkflowAgentDispatchError::Failed(error.to_string())
        }
    }
}

impl WorkflowAgentDispatcher for NativeWorkflowAgentDispatcher {
    fn prepare<'a>(
        &'a self,
        request: WorkflowAgentBindingRequest,
    ) -> WorkflowFuture<'a, Result<PreparedWorkflowAgentBinding, WorkflowAgentDispatchError>> {
        Box::pin(async move { self.prepare_binding(&request).await })
    }

    fn dispatch<'a>(
        &'a self,
        prepared: PreparedWorkflowAgentBinding,
        request: WorkflowAgentDispatchRequest,
        cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowAgentDispatchError>> {
        Box::pin(self.dispatch_inner(prepared, request, cancellation))
    }
}

fn agent_backend_error(error: OneShotAgentError) -> WorkflowAgentDispatchError {
    let message = error.to_string();
    if error.category == OneShotAgentFailureCategory::Aborted {
        return WorkflowAgentDispatchError::Cancelled(message);
    }
    match error.stage {
        OneShotAgentFailureStage::Validate => WorkflowAgentDispatchError::Failed(message),
        OneShotAgentFailureStage::Start
        | OneShotAgentFailureStage::Run
        | OneShotAgentFailureStage::Decode
        | OneShotAgentFailureStage::Teardown => WorkflowAgentDispatchError::Uncertain(message),
    }
}
