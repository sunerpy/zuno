//! Native workflow planning with a persistent data-owner coordinator. The Worker
//! prepares bounded workspaces, then returns the original invocation's wait.

use super::*;
use crate::child::ChildToolDispatcher;
use tokio_util::sync::CancellationToken;
use zuno_application::{
    child::{ChildDelivery, ChildDispatch, ChildInvocation, ChildWorkspaceState},
    workflow::{WorkflowCommand, WorkflowDispatch, WorkflowInvocation},
};
use zuno_engine::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchOutcome,
    ToolDispatchResult, ToolDispatcher, UncertainOutcome,
};
use zuno_error::ToolError;
use zuno_orchestration::WorkflowTemplateDescriptor;
use zuno_tool::{
    HistoryPolicy, PermissionAsk, PermissionAsker, PermissionOrigin, ToolContext, ToolDefinition,
    ToolOutput, ToolUiIntent,
};
use zuno_tools::{
    orchestration_dispatch::OrchestrationDispatch,
    task::ReportDelivery,
    workflow::{WorkflowHost, WorkflowParams, WorkflowRequest, WorkflowTool, WorkflowTurn},
};
use zuno_types::identity::InvocationId;

impl WorkerClient {
    async fn workflow_command(
        &self,
        execution: &WorkerExecution,
        command: WorkflowCommand,
    ) -> Result<WorkflowDispatch, TurnStateError> {
        if execution.boundary_started() || execution.deadline()? <= tokio::time::Instant::now() {
            return Err(TurnStateError::LeaseLost);
        }
        let grant = execution
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        let body = serde_json::to_vec(&command).map_err(|_| TurnStateError::InvalidData)?;
        let response = self.post(WORKFLOW_PATH, Some(&grant), body).await?;
        serde_json::from_slice(&response).map_err(|_| TurnStateError::InvalidData)
    }

    async fn workflow_workspace(
        &self,
        execution: &WorkerExecution,
        gateway: &crate::gateway::GatewayClient,
        child: &ChildDispatch,
    ) -> Result<(), TurnStateError> {
        if child.workspace != ChildWorkspaceState::Pending {
            return Ok(());
        }
        let request = zuno_application::environment::wire::GatewayRequest::new(
            zuno_application::environment::wire::GatewayCommand::PrepareChildWorkspace {
                child_job_id: child.job_id.clone(),
            },
        )
        .map_err(|_| TurnStateError::InvalidData)?;
        let issued = self.gateway_ticket(execution, &request).await?;
        let result = gateway
            .execute(&issued, &request)
            .await
            .map_err(|error| match error {
                zuno_application::ApplicationError::Forbidden => TurnStateError::Forbidden,
                zuno_application::ApplicationError::Conflict
                | zuno_application::ApplicationError::LeaseLost => TurnStateError::Conflict,
                _ => TurnStateError::Unavailable,
            })?;
        let zuno_application::environment::wire::GatewayReply::ChildWorkspace(receipt) = result
        else {
            return Err(TurnStateError::InvalidData);
        };
        if receipt.child_job_id != child.job_id
            || receipt.target.spec.session_id != child.session_id
            || receipt.target.owner != execution.job.principal.owner()
        {
            return Err(TurnStateError::InvalidData);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum GroupKind {
    Workflow,
    Council,
}

impl WorkerClient {
    async fn prepare_group_command(
        &self,
        execution: &WorkerExecution,
        job: zuno_types::identity::JobId,
        kind: GroupKind,
    ) -> Result<WorkflowDispatch, TurnStateError> {
        match kind {
            GroupKind::Workflow => {
                self.workflow_command(execution, WorkflowCommand::Prepare { job_id: job })
                    .await
            }
            GroupKind::Council => {
                self.council_command(
                    execution,
                    zuno_application::council::CouncilCommand::Prepare { job_id: job },
                )
                .await
            }
        }
    }

    /// Workflow and Council share workspace preparation and immutable reply
    /// validation. All execution still belongs to the native child Job runtime.
    pub(crate) async fn prepare_group(
        &self,
        execution: &WorkerExecution,
        gateway: &crate::gateway::GatewayClient,
        first: WorkflowDispatch,
        nodes: &[String],
        kind: GroupKind,
        cancellation: &CancellationToken,
    ) -> Result<WorkflowDispatch, TurnStateError> {
        self.workflow_workspace(execution, gateway, &first.group)
            .await?;
        let validate = |reply: &WorkflowDispatch| {
            if reply.run_id != first.run_id
                || reply.group.job_id != first.group.job_id
                || reply.group.session_id != first.group.session_id
                || reply.group.wait != first.group.wait
                || reply.group.delivery != first.group.delivery
                || reply.nodes.len() != nodes.len()
                || reply
                    .nodes
                    .iter()
                    .zip(nodes)
                    .any(|(actual, expected)| &actual.node_id != expected)
            {
                Err(TurnStateError::InvalidData)
            } else {
                Ok(())
            }
        };
        let mut prepared = self
            .prepare_group_command(execution, first.group.job_id.clone(), kind)
            .await?;
        validate(&prepared)?;
        for node in &prepared.nodes {
            if cancellation.is_cancelled() {
                return Err(TurnStateError::LeaseLost);
            }
            self.workflow_workspace(execution, gateway, &node.child)
                .await?;
        }
        if !prepared.prepared {
            let confirmed = self
                .prepare_group_command(execution, first.group.job_id.clone(), kind)
                .await?;
            validate(&confirmed)?;
            if confirmed.nodes.iter().zip(&prepared.nodes).any(|(a, b)| {
                a.child.job_id != b.child.job_id || a.child.session_id != b.child.session_id
            }) {
                return Err(TurnStateError::InvalidData);
            }
            prepared = confirmed;
        }
        if !prepared.prepared
            || prepared
                .nodes
                .iter()
                .any(|node| node.child.workspace == ChildWorkspaceState::Pending)
        {
            return Err(TurnStateError::InvalidData);
        }
        Ok(prepared)
    }
}

fn host_error(error: TurnStateError) -> ToolError {
    match error {
        TurnStateError::Forbidden => ToolError::Denied {
            tool: "workflow".to_owned(),
            denial: None,
        },
        _ => ToolError::Uncertain {
            tool: "workflow".to_owned(),
            applied_paths: Vec::new(),
            source: Box::new(std::io::Error::other(format!(
                "workflow admission must be reconciled: {error}"
            ))),
        },
    }
}
struct RemoteWorkflowHost {
    client: WorkerClient,
    execution: WorkerExecution,
    gateway: Arc<crate::gateway::GatewayClient>,
    invocation: InvocationId,
    digest: String,
    params: WorkflowParams,
    presentation: serde_json::Value,
}
impl RemoteWorkflowHost {
    fn validate(&self, reply: &WorkflowDispatch, expected: ChildDelivery) -> Result<(), ToolError> {
        if reply.group.delivery != expected
            || reply.group.wait.invocation_id != self.invocation
            || reply.group.wait.turn_id != self.execution.job.turn_id
            || reply.group.wait.arguments_sha256 != self.digest
            || reply.nodes.len() > 64
        {
            return Err(host_error(TurnStateError::InvalidData));
        }
        Ok(())
    }
}
#[async_trait]
impl WorkflowHost for RemoteWorkflowHost {
    async fn dispatch(
        &self,
        request: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<OrchestrationDispatch<WorkflowTurn>, ToolError> {
        if request.parent_session_id != self.execution.job.session_id.as_str()
            || request.workflow != self.params.workflow.trim()
            || cancellation.is_cancelled()
        {
            return Err(host_error(TurnStateError::Forbidden));
        }
        let delivery = match (request.background, request.report_delivery) {
            (false, _) => ChildDelivery::Foreground,
            (true, ReportDelivery::NextStep) => ChildDelivery::NextStep,
            (true, ReportDelivery::Quiet) => ChildDelivery::Quiet,
        };
        let first = self
            .client
            .workflow_command(
                &self.execution,
                WorkflowCommand::Dispatch {
                    invocation: Box::new(WorkflowInvocation {
                        template: request.workflow.clone(),
                        root: ChildInvocation {
                            invocation_id: self.invocation.clone(),
                            arguments_sha256: self.digest.clone(),
                            logical_key: format!("workflow:v1:{}", self.digest),
                            prompt: self.params.prompt.clone(),
                            description: self
                                .params
                                .description
                                .clone()
                                .unwrap_or_else(|| format!("{} workflow", request.workflow)),
                            delivery,
                            resume_session_id: None,
                            presentation: self.presentation.clone(),
                        },
                    }),
                },
            )
            .await
            .map_err(host_error)?;
        self.validate(&first, delivery)?;
        let prepared = self
            .client
            .prepare_group(
                &self.execution,
                &self.gateway,
                first,
                &request
                    .nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .collect::<Vec<_>>(),
                GroupKind::Workflow,
                &cancellation,
            )
            .await
            .map_err(host_error)?;
        if delivery == ChildDelivery::Foreground {
            Ok(OrchestrationDispatch::Pending(prepared.group.wait))
        } else {
            Ok(OrchestrationDispatch::Ready(WorkflowTurn {
                run_id: prepared.run_id.to_string(),
                job_id: Some(prepared.group.job_id.to_string()),
                output:
                    "Workflow was durably admitted under its fixed dependency and capacity limits."
                        .to_owned(),
            }))
        }
    }
}

struct WorkflowPermission {
    execution: WorkerExecution,
    templates: Vec<String>,
}
#[async_trait]
impl PermissionAsker for WorkflowPermission {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        if tool == "workflow"
            && ask.permission == "task"
            && origin.session_id() == self.execution.job.session_id.as_str()
            && origin.principal_scope() == &self.execution.job.principal
            && ask.patterns.len() == 1
            && self
                .templates
                .iter()
                .any(|name| ask.patterns[0] == format!("workflow:{name}"))
        {
            Ok(())
        } else {
            Err(host_error(TurnStateError::Forbidden))
        }
    }
}

#[derive(Clone)]
pub struct WorkflowToolDispatcher {
    child: ChildToolDispatcher,
    templates: Vec<WorkflowTemplateDescriptor>,
    definition: ToolDefinition,
}
impl WorkflowToolDispatcher {
    pub fn new(
        child: ChildToolDispatcher,
        templates: Vec<WorkflowTemplateDescriptor>,
    ) -> Result<Self, TurnStateError> {
        if templates.is_empty()
            || child.gateway.is_none()
            || child
                .available_tools()
                .definitions
                .iter()
                .any(|tool| tool.id == "workflow")
        {
            return Err(TurnStateError::InvalidData);
        }
        for template in &templates {
            zuno_engine::workflow::WorkflowGraph::new(
                template
                    .nodes
                    .iter()
                    .map(|node| (node.id.clone(), node.depends_on.clone())),
                template.max_parallel,
            )
            .map_err(|_| TurnStateError::InvalidData)?;
        }
        let definition = ToolDefinition {
            id: "workflow".to_owned(),
            display_name: "Run workflow".to_owned(),
            description: format!(
                "Instantiate a fixed workflow. Graph, agents and capacity are configuration-owned. Available templates: {}.",
                templates
                    .iter()
                    .map(|template| template.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            parameters: zuno_tool::schema::params_schema::<WorkflowParams>(),
            ui_intent: ToolUiIntent::Subagent,
            history_policy: HistoryPolicy::ExactDeclaration,
            presentation: zuno_types::activity::InvocationPresentation::builtin(
                zuno_types::activity::InvocationAction::Workflow,
            ),
        };
        Ok(Self {
            child,
            templates,
            definition,
        })
    }
}
fn blocked(kind: ToolBlockKind, message: &str) -> PreparedToolDispatch {
    PreparedToolDispatch::ready(ToolDispatchResult::blocked(
        ToolOutput::text("Workflow", message),
        kind,
    ))
}
#[async_trait]
impl ToolDispatcher for WorkflowToolDispatcher {
    fn available_tools(&self) -> AvailableTools {
        let mut tools = self.child.available_tools();
        tools.definitions.push(self.definition.clone());
        tools
    }
    fn concurrency_policy(&self, request: &DispatchRequest) -> zuno_tool::ToolConcurrencyPolicy {
        if request.call.name == "workflow" {
            zuno_tool::ToolConcurrencyPolicy::IsolatedBackground
        } else {
            self.child.concurrency_policy(request)
        }
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.name != "workflow" {
            return self.child.prepare(request).await;
        }
        let execution = &self.child.execution;
        if request.session_id != execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &execution.job.principal
            || !request
                .available_tools
                .iter()
                .any(|tool| crate::definition_matches(tool, &self.definition))
            || request.interrupt.is_set()
        {
            return blocked(
                ToolBlockKind::Denied,
                "workflow does not belong to this execution",
            );
        }
        let Ok(invocation) = InvocationId::new(&request.call.id) else {
            return blocked(ToolBlockKind::InvalidArguments, "invalid workflow call ID");
        };
        let digest = zuno_orchestration::sha256_json(&request.call.input);
        let mut args = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut args);
        let params = match serde_json::from_value::<WorkflowParams>(args.clone()) {
            Ok(params) if request.call.input_error.is_none() => params,
            _ => {
                return blocked(
                    ToolBlockKind::InvalidArguments,
                    "workflow arguments do not match the schema",
                );
            }
        };
        let host = Arc::new(RemoteWorkflowHost {
            client: self.child.client.clone(),
            execution: execution.clone(),
            gateway: self.child.gateway.clone().expect("validated gateway"),
            invocation: invocation.clone(),
            digest: digest.clone(),
            params: params.clone(),
            presentation: args.clone(),
        });
        let tool = match WorkflowTool::new(
            self.templates.clone(),
            self.child.planner(invocation, digest, args),
            host,
        ) {
            Ok(tool) => tool,
            Err(_) => {
                return blocked(
                    ToolBlockKind::Denied,
                    "workflow definition is not executable",
                );
            }
        };
        let mut context = ToolContext::new_scoped(
            request.session_id,
            request.message_id,
            request.call.id,
            request.agent,
            Arc::new(WorkflowPermission {
                execution: execution.clone(),
                templates: self
                    .templates
                    .iter()
                    .map(|template| template.name.clone())
                    .collect(),
            }),
            Arc::new(request.interrupt),
            request.principal_scope.as_ref().clone(),
        );
        if let Some(snapshot) = request.orchestration_snapshot {
            context = context.with_orchestration_snapshot(snapshot);
        }
        PreparedToolDispatch::deferred(Box::pin(async move {
            match tool.dispatch(params, context).await {
                Ok(OrchestrationDispatch::Pending(wait)) => ToolDispatchOutcome::Pending(wait),
                Ok(OrchestrationDispatch::Ready(output)) => {
                    ToolDispatchOutcome::Completed(Box::new(ToolDispatchResult::success(output)))
                }
                Err(error) => {
                    let output = ToolOutput::text("Workflow", zuno_error::source::describe(&error));
                    let result = match error {
                        ToolError::Denied { .. } => {
                            ToolDispatchResult::blocked(output, ToolBlockKind::Denied)
                        }
                        ToolError::InvalidArgs { .. } => {
                            ToolDispatchResult::blocked(output, ToolBlockKind::InvalidArguments)
                        }
                        ToolError::Uncertain { .. } => ToolDispatchResult::error(output)
                            .with_uncertain_outcome(UncertainOutcome {
                                tool: "workflow".to_owned(),
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
