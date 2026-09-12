//! Native task planning with a remote ChildTurnHost and durable pending results.

use super::*;
use serde_json::Value;
use std::collections::BTreeMap;
use zuno_agent::model_policy::ModelChoice;
use zuno_application::child::{
    ChildCommand, ChildDelivery, ChildInvocation, ChildReply, ChildWorkspaceState,
};
use zuno_engine::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchOutcome,
    ToolDispatchResult, ToolDispatcher, UncertainOutcome,
};
use zuno_error::ToolError;
use zuno_tool::{
    HistoryPolicy, InterruptHandle, PermissionAsk, PermissionAsker, PermissionOrigin, ToolContext,
    ToolDefinition, ToolOutput, ToolUiIntent,
};
use zuno_tools::task::{
    ChildTurn, ChildTurnDispatch, ChildTurnError, ChildTurnHost, ChildTurnRequest, ChildTurnState,
    DelegationLimits, DelegationTargets, FixedFacts, ReportDelivery, TaskDispatch, TaskParams,
    TaskTool,
};
use zuno_types::identity::InvocationId;

impl WorkerClient {
    async fn child_command(
        &self,
        execution: &WorkerExecution,
        command: &ChildCommand,
    ) -> Result<ChildReply, TurnStateError> {
        if execution.boundary_started() || execution.deadline()? <= tokio::time::Instant::now() {
            return Err(TurnStateError::LeaseLost);
        }
        let grant = execution
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        let body = serde_json::to_vec(command).map_err(|_| TurnStateError::InvalidData)?;
        let response = self.post(CHILD_PATH, Some(&grant), body).await?;
        serde_json::from_slice(&response).map_err(|_| TurnStateError::InvalidData)
    }
}

struct RemoteChildHost {
    client: WorkerClient,
    execution: WorkerExecution,
    invocation_id: InvocationId,
    arguments_sha256: String,
    presentation: Value,
    gateway: Option<Arc<crate::gateway::GatewayClient>>,
}

fn host_error(error: TurnStateError, admission: bool) -> ChildTurnError {
    match error {
        TurnStateError::Forbidden => ChildTurnError::Denied,
        TurnStateError::Conflict | TurnStateError::LeaseLost => ChildTurnError::Conflict,
        TurnStateError::Unavailable if admission => ChildTurnError::Uncertain,
        TurnStateError::Unavailable => ChildTurnError::Unavailable,
        _ => ChildTurnError::Host("child protocol is incompatible".to_owned()),
    }
}

#[async_trait]
impl ChildTurnHost for RemoteChildHost {
    async fn delegation_depth(&self, session: &str) -> Result<u32, ChildTurnError> {
        if session != self.execution.job.session_id.as_str() {
            return Err(ChildTurnError::Denied);
        }
        match self
            .client
            .child_command(&self.execution, &ChildCommand::Depth)
            .await
            .map_err(|error| host_error(error, false))?
        {
            ChildReply::Depth { depth } => Ok(depth),
            _ => Err(ChildTurnError::Host(
                "child depth response is incompatible".to_owned(),
            )),
        }
    }
    async fn dispatch(
        &self,
        request: ChildTurnRequest,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> Result<ChildTurnDispatch, ChildTurnError> {
        if request.parent_session_id != self.execution.job.session_id.as_str() || interrupt.is_set()
        {
            return Err(ChildTurnError::Denied);
        }
        if !request.provider_options.is_empty() || request.effort.is_some() {
            return Err(ChildTurnError::Host(
                "child model options must be installed in its immutable definition".to_owned(),
            ));
        }
        let delivery = match (request.background, request.report_delivery) {
            (false, _) => ChildDelivery::Foreground,
            (true, ReportDelivery::NextStep) => ChildDelivery::NextStep,
            (true, ReportDelivery::Quiet) => ChildDelivery::Quiet,
        };
        let invocation = ChildInvocation {
            invocation_id: self.invocation_id.clone(),
            arguments_sha256: self.arguments_sha256.clone(),
            logical_key: request.logical_key,
            prompt: request.prompt,
            description: request.description.unwrap_or_else(|| request.agent.clone()),
            delivery,
            resume_session_id: request
                .resume_session_id
                .map(zuno_types::identity::SessionId::new)
                .transpose()
                .map_err(|_| ChildTurnError::Host("invalid child session ID".to_owned()))?,
            presentation: self.presentation.clone(),
        };
        let command = ChildCommand::Dispatch {
            agent: request.agent,
            model: request.model.map(|model| model.model),
            invocation: Box::new(invocation),
        };
        let reply = self
            .client
            .child_command(&self.execution, &command)
            .await
            .map_err(|error| host_error(error, true))?;
        let ChildReply::Dispatch { mut dispatch } = reply else {
            return Err(ChildTurnError::Uncertain);
        };
        if dispatch.delivery != delivery
            || dispatch.wait.invocation_id != self.invocation_id
            || dispatch.wait.turn_id != self.execution.job.turn_id
            || dispatch.wait.arguments_sha256 != self.arguments_sha256
        {
            return Err(ChildTurnError::Uncertain);
        }
        if dispatch.workspace == ChildWorkspaceState::Pending {
            let gateway = self.gateway.as_ref().ok_or_else(|| {
                ChildTurnError::Host("child workspace gateway is not installed".to_owned())
            })?;
            let request = zuno_application::environment::wire::GatewayRequest::new(
                zuno_application::environment::wire::GatewayCommand::PrepareChildWorkspace {
                    child_job_id: dispatch.job_id.clone(),
                },
            )
            .map_err(|_| ChildTurnError::Conflict)?;
            let issued = self
                .client
                .gateway_ticket(&self.execution, &request)
                .await
                .map_err(|error| host_error(error, false))?;
            let result = gateway
                .execute(&issued, &request)
                .await
                .map_err(|error| match error {
                    zuno_application::ApplicationError::Forbidden => ChildTurnError::Denied,
                    zuno_application::ApplicationError::Conflict
                    | zuno_application::ApplicationError::LeaseLost => ChildTurnError::Conflict,
                    _ => ChildTurnError::Uncertain,
                })?;
            let zuno_application::environment::wire::GatewayReply::ChildWorkspace(receipt) = result
            else {
                return Err(ChildTurnError::Uncertain);
            };
            if receipt.child_job_id != dispatch.job_id
                || receipt.target.owner != self.execution.job.principal.owner()
                || receipt.target.spec.session_id != dispatch.session_id
            {
                return Err(ChildTurnError::Uncertain);
            }
            let ChildReply::Dispatch { dispatch: prepared } = self
                .client
                .child_command(&self.execution, &command)
                .await
                .map_err(|error| host_error(error, true))?
            else {
                return Err(ChildTurnError::Uncertain);
            };
            if prepared.job_id != dispatch.job_id
                || prepared.session_id != dispatch.session_id
                || prepared.wait != dispatch.wait
                || prepared.workspace != ChildWorkspaceState::Ready
            {
                return Err(ChildTurnError::Uncertain);
            }
            dispatch = prepared;
        }
        if delivery == ChildDelivery::Foreground {
            return Ok(ChildTurnDispatch::Pending(dispatch.wait));
        }
        Ok(ChildTurnDispatch::Ready(ChildTurn {
            session_id: dispatch.session_id.to_string(),
            job_id: Some(dispatch.job_id.to_string()),
            state: ChildTurnState::Running,
            output: "Background child was durably admitted.".to_owned(),
            report_metadata: None,
        }))
    }
}

struct DelegationPermission {
    execution: WorkerExecution,
    targets: BTreeMap<String, ChildToolTarget>,
}
#[async_trait]
impl PermissionAsker for DelegationPermission {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        if tool == "task"
            && ask.permission == "task"
            && origin.session_id() == self.execution.job.session_id.as_str()
            && origin.principal_scope() == &self.execution.job.principal
            && ask.patterns.len() == 1
            && self.targets.contains_key(&ask.patterns[0])
        {
            return Ok(());
        }
        Err(ToolError::Denied {
            tool: tool.to_owned(),
            denial: None,
        })
    }
}

/// Configuration-owned targets and model routing; dynamic names remain data.
#[derive(Clone)]
pub struct ChildToolDispatcher {
    inner: Arc<dyn ToolDispatcher>,
    pub(crate) client: WorkerClient,
    pub(crate) execution: WorkerExecution,
    targets: BTreeMap<String, ChildToolTarget>,
    maximum_depth: u32,
    definition: ToolDefinition,
    pub(crate) gateway: Option<Arc<crate::gateway::GatewayClient>>,
}
#[derive(Clone)]
pub struct ChildToolTarget {
    pub model: String,
    pub facts: zuno_tools::ModelFacts,
}
impl ChildToolDispatcher {
    pub fn new(
        inner: Arc<dyn ToolDispatcher>,
        client: WorkerClient,
        execution: WorkerExecution,
        targets: BTreeMap<String, ChildToolTarget>,
        maximum_depth: u32,
    ) -> Result<Self, TurnStateError> {
        if targets.is_empty()
            || !(1..=16).contains(&maximum_depth)
            || DelegationTargets::new(targets.keys().cloned()).is_err()
            || targets.values().any(|target| !target.model.contains('/'))
            || inner
                .available_tools()
                .definitions
                .iter()
                .any(|tool| tool.id == "task")
        {
            return Err(TurnStateError::InvalidData);
        }
        let definition = ToolDefinition {
            presentation: zuno_types::activity::InvocationPresentation::builtin(
                zuno_types::activity::InvocationAction::Agent,
            ),
            id: "task".to_owned(),
            display_name: "Delegate task".to_owned(),
            description: zuno_tools::TASK_DESCRIPTION.to_owned(),
            parameters: zuno_tool::schema::params_schema::<TaskParams>(),
            ui_intent: ToolUiIntent::Subagent,
            history_policy: HistoryPolicy::ExactDeclaration,
        };
        Ok(Self {
            inner,
            client,
            execution,
            targets,
            maximum_depth,
            definition,
            gateway: None,
        })
    }
    pub(crate) fn planner(
        &self,
        invocation: InvocationId,
        digest: String,
        args: Value,
    ) -> TaskTool {
        let host = Arc::new(RemoteChildHost {
            client: self.client.clone(),
            execution: self.execution.clone(),
            invocation_id: invocation,
            arguments_sha256: digest,
            presentation: args,
            gateway: self.gateway.clone(),
        });
        let mut facts = FixedFacts::new();
        for target in self.targets.values() {
            facts = facts.with(&target.model, target.facts.clone());
        }
        let mut tool = TaskTool::new(host, Arc::new(facts))
            .with_targets(
                DelegationTargets::new(self.targets.keys().cloned()).expect("validated targets"),
            )
            .with_limits(DelegationLimits {
                subagent_depth: self.maximum_depth,
            });
        for (agent, target) in &self.targets {
            tool = tool.with_agent_override(agent, ModelChoice::new(&target.model));
        }
        tool
    }
    pub fn with_workspace_gateway(mut self, gateway: Arc<crate::gateway::GatewayClient>) -> Self {
        self.gateway = Some(gateway);
        self
    }
}
fn blocked(kind: ToolBlockKind, text: &str) -> PreparedToolDispatch {
    PreparedToolDispatch::ready(ToolDispatchResult::blocked(
        ToolOutput::text("Task", text),
        kind,
    ))
}
#[async_trait]
impl ToolDispatcher for ChildToolDispatcher {
    fn available_tools(&self) -> AvailableTools {
        let mut tools = self.inner.available_tools();
        tools.definitions.push(self.definition.clone());
        tools
    }
    fn concurrency_policy(&self, request: &DispatchRequest) -> zuno_tool::ToolConcurrencyPolicy {
        if request.call.name == "task" {
            zuno_tool::ToolConcurrencyPolicy::IsolatedBackground
        } else {
            self.inner.concurrency_policy(request)
        }
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.name != "task" {
            return self.inner.prepare(request).await;
        }
        if request.session_id != self.execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &self.execution.job.principal
            || !request
                .available_tools
                .iter()
                .any(|tool| crate::definition_matches(tool, &self.definition))
            || request.interrupt.is_set()
        {
            return blocked(
                ToolBlockKind::Denied,
                "task does not belong to this execution",
            );
        }
        let Ok(id) = InvocationId::new(&request.call.id) else {
            return blocked(
                ToolBlockKind::InvalidArguments,
                "invalid task invocation ID",
            );
        };
        let digest = zuno_orchestration::sha256_json(&request.call.input);
        let mut args = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut args);
        let params = match serde_json::from_value::<TaskParams>(args.clone()) {
            Ok(params) if request.call.input_error.is_none() => params,
            _ => {
                return blocked(
                    ToolBlockKind::InvalidArguments,
                    "task arguments do not match its declared schema",
                );
            }
        };
        let tool = self.planner(id, digest, args);
        let mut context = ToolContext::new_scoped(
            request.session_id,
            request.message_id,
            request.call.id,
            request.agent,
            Arc::new(DelegationPermission {
                execution: self.execution.clone(),
                targets: self.targets.clone(),
            }),
            Arc::new(request.interrupt),
            request.principal_scope.as_ref().clone(),
        );
        if let Some(snapshot) = request.orchestration_snapshot {
            context = context.with_orchestration_snapshot(snapshot);
        }
        PreparedToolDispatch::deferred(Box::pin(async move {
            match tool.dispatch(params, context).await {
                Ok(TaskDispatch::Ready(output)) => {
                    ToolDispatchOutcome::Completed(Box::new(ToolDispatchResult::success(output)))
                }
                Ok(TaskDispatch::Pending(reference)) => ToolDispatchOutcome::Pending(reference),
                Err(error) => {
                    let output = ToolOutput::text("Task", zuno_error::source::describe(&error));
                    let result = match error {
                        ToolError::Denied { .. } => {
                            ToolDispatchResult::blocked(output, ToolBlockKind::Denied)
                        }
                        ToolError::InvalidArgs { .. } => {
                            ToolDispatchResult::blocked(output, ToolBlockKind::InvalidArguments)
                        }
                        ToolError::Uncertain { .. } => ToolDispatchResult::error(output)
                            .with_uncertain_outcome(UncertainOutcome {
                                tool: "task".to_owned(),
                                applied_paths: Vec::new(),
                                cause: zuno_error::UncertainCause::LostOutcome,
                            }),
                        _ => ToolDispatchResult::error(output),
                    };
                    ToolDispatchOutcome::Completed(Box::new(result))
                }
            }
        }))
    }
}
