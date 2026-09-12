//! A merge is a native side effect with a stable operation and durable waits.
use crate::{WorkerClient, WorkerExecution, gateway::GatewayClient};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use zuno_application::{
    ApplicationError,
    authorization::ApprovalState,
    environment::wire::{GatewayCommand, GatewayReply, GatewayRequest},
    workspace_merge::{MergeChoice, WorkspaceMergeState, WorkspacePath},
};
use zuno_engine::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchOutcome,
    ToolDispatchResult, ToolDispatcher, UncertainOutcome,
};
use zuno_tool::{HistoryPolicy, ToolDefinition, ToolOutput, ToolUiIntent};
use zuno_types::{
    identity::{InvocationId, JobId, OperationId, WaitId},
    wait::{WaitContinuation, WaitRef, WaitTarget},
};

pub const WIRE_ID: &str = "workspace_merge";
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Arguments {
    child_job_id: JobId,
    #[serde(default)]
    resolutions: BTreeMap<WorkspacePath, MergeChoice>,
}
#[derive(Clone)]
pub struct WorkspaceMergeDispatcher {
    inner: Arc<dyn ToolDispatcher>,
    state: WorkerClient,
    execution: WorkerExecution,
    gateway: Arc<GatewayClient>,
    definition: ToolDefinition,
}
impl WorkspaceMergeDispatcher {
    pub fn new(
        inner: Arc<dyn ToolDispatcher>,
        state: WorkerClient,
        execution: WorkerExecution,
        gateway: Arc<GatewayClient>,
    ) -> Self {
        Self {inner,state,execution,gateway,definition:ToolDefinition {
            id:WIRE_ID.to_owned(),display_name:"Merge child workspace".to_owned(),
            description:"Review and merge a completed child Job's workspace changes into this workspace. Preserves independent parent edits. Conflicts need explicit parent/child resolutions, and every applied merge requires current human approval. Returns a final authoritative merge result after a durable operation wait.".to_owned(),
            parameters:zuno_tool::schema::params_schema::<Arguments>(),ui_intent:ToolUiIntent::Generic,history_policy:HistoryPolicy::ExactDeclaration,
            presentation:zuno_types::activity::InvocationPresentation::builtin(zuno_types::activity::InvocationAction::FileEdit),
        }}
    }
    async fn call(&self, command: GatewayCommand) -> Result<GatewayReply, ApplicationError> {
        let request = GatewayRequest::new(command)?;
        let issued = self
            .state
            .gateway_ticket(&self.execution, &request)
            .await
            .map_err(|error| match error {
                zuno_engine::state::TurnStateError::Forbidden => ApplicationError::Forbidden,
                zuno_engine::state::TurnStateError::LeaseLost => ApplicationError::LeaseLost,
                _ => ApplicationError::Unavailable,
            })?;
        self.gateway.execute(&issued, &request).await
    }
    fn wait(&self, request: &DispatchRequest, target: WaitTarget) -> WaitRef {
        WaitRef {
            id: WaitId::new(format!(
                "wait_{}",
                zuno_orchestration::sha256_json(&json!([
                    "workspace-merge-wait",
                    self.execution.job.id,
                    request.call.id,
                    target
                ]))
            ))
            .expect("derived identity"),
            turn_id: self.execution.job.turn_id.clone(),
            invocation_id: InvocationId::new(&request.call.id).expect("validated invocation"),
            arguments_sha256: zuno_orchestration::sha256_json(&request.call.input),
            target,
            continuation: WaitContinuation::CurrentTurn,
        }
    }
    async fn prepare_merge(
        &self,
        request: DispatchRequest,
    ) -> Result<PreparedToolDispatch, Box<ToolDispatchResult>> {
        if request.session_id != self.execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &self.execution.job.principal
            || !request
                .available_tools
                .iter()
                .any(|definition| crate::definition_matches(definition, &self.definition))
            || request
                .orchestration_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.turn_id != self.execution.job.turn_id.as_str())
            || request.interrupt.is_set()
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The merge is outside this execution's scope.",
            ));
        }
        let invocation = InvocationId::new(&request.call.id).map_err(|_| invalid())?;
        if request.call.input_error.is_some() {
            return Err(invalid());
        }
        let mut input = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut input);
        let arguments: Arguments = serde_json::from_value(input).map_err(|_| invalid())?;
        if arguments.resolutions.len() > zuno_application::workspace_merge::MAX_MERGE_FILES
            || arguments
                .resolutions
                .values()
                .any(|choice| *choice == MergeChoice::Conflict)
        {
            return Err(invalid());
        }
        let id = OperationId::new(format!(
            "op_{}",
            zuno_orchestration::sha256_json(&json!([
                "workspace-merge",
                self.execution.job.principal.owner(),
                self.execution.job.id,
                invocation
            ]))
        ))
        .expect("derived identity");
        let GatewayReply::WorkspaceMergePreview(mut operation) = self
            .call(GatewayCommand::PreviewWorkspaceMerge {
                id: id.clone(),
                invocation_id: invocation.clone(),
                child_job_id: arguments.child_job_id.clone(),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid merge preview response.",
            ));
        };
        if operation.id != id
            || operation.invocation_id != invocation
            || operation.child_job_id != arguments.child_job_id
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The merge preview changed its invocation identity.",
            ));
        }
        for (path, choice) in arguments.resolutions {
            let Some(change) = operation
                .plan
                .changes
                .iter_mut()
                .find(|change| change.path == path)
            else {
                return Err(invalid());
            };
            change.choice = choice;
        }
        if operation
            .plan
            .changes
            .iter()
            .any(|change| change.choice == MergeChoice::Conflict)
        {
            return Err(Box::new(ToolDispatchResult::blocked(
                ToolOutput::text(
                    "Workspace merge conflicts",
                    serde_json::to_string(&operation.plan)
                        .unwrap_or_else(|_| "Merge plan unavailable".to_owned()),
                ),
                ToolBlockKind::Conflict,
            )));
        }
        if operation.plan.changes.is_empty() {
            return Ok(PreparedToolDispatch::ready(ToolDispatchResult::success(
                ToolOutput::text("Workspace merge", "The child has no changes to apply."),
            )));
        }
        let GatewayReply::Approval(approval) = self
            .call(GatewayCommand::PrepareWorkspaceMerge {
                operation: operation.clone(),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid merge approval response.",
            ));
        };
        if approval.binding.job_id != self.execution.job.id
            || approval.binding.session_id != self.execution.job.session_id
            || approval.binding.turn_id != self.execution.job.turn_id
            || approval.binding.invocation_id != invocation
            || approval.binding.operation_id != operation.id
            || approval.binding.arguments_sha256 != operation.plan.digest()
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The approval does not bind this merge plan.",
            ));
        }
        match approval.state {
            ApprovalState::Pending => Ok(PreparedToolDispatch::Pending(self.wait(
                &request,
                WaitTarget::Approval {
                    approval_id: approval.id.clone(),
                },
            ))),
            ApprovalState::Approved => {
                let wait = self.wait(
                    &request,
                    WaitTarget::Operation {
                        operation_id: operation.id.clone(),
                    },
                );
                let dispatcher = self.clone();
                Ok(PreparedToolDispatch::deferred(Box::pin(async move {
                    if request.interrupt.is_set() {
                        return ToolDispatchOutcome::Completed(blocked(
                            ToolBlockKind::Denied,
                            "The merge was interrupted before submission.",
                        ));
                    }
                    let submit = dispatcher.call(GatewayCommand::SubmitWorkspaceMerge {
                        operation: operation.clone(),
                    });
                    let result = tokio::select! {
                        biased;
                        _=request.interrupt.notified()=>return uncertain(&id),
                        result=submit=>result,
                    };
                    let result = match result {
                        Ok(reply) => Ok(reply),
                        Err(_) => {
                            dispatcher
                                .call(GatewayCommand::InspectWorkspaceMerge {
                                    operation_id: id.clone(),
                                })
                                .await
                        }
                    };
                    match result {
                        Ok(GatewayReply::WorkspaceMergeReceipt(receipt))
                            if receipt.id == id
                                && receipt.environment_id == operation.environment_id
                                && receipt.plan_digest == operation.plan.digest()
                                && matches!(
                                    receipt.state,
                                    WorkspaceMergeState::Preparing
                                        | WorkspaceMergeState::Committed
                                        | WorkspaceMergeState::Cancelled
                                ) =>
                        {
                            ToolDispatchOutcome::Pending(wait)
                        }
                        _ => uncertain(&id),
                    }
                })))
            }
            _ => Err(blocked(
                ToolBlockKind::Denied,
                "Current human approval does not authorize this merge.",
            )),
        }
    }
}
fn blocked(kind: ToolBlockKind, text: &str) -> Box<ToolDispatchResult> {
    Box::new(ToolDispatchResult::blocked(
        ToolOutput::text("Workspace merge", text),
        kind,
    ))
}
fn invalid() -> Box<ToolDispatchResult> {
    blocked(
        ToolBlockKind::InvalidArguments,
        "Provide a completed childJobId and optional parent/child path resolutions.",
    )
}
fn before_submission(error: ApplicationError) -> Box<ToolDispatchResult> {
    match error {
        ApplicationError::Forbidden => blocked(
            ToolBlockKind::Denied,
            "This child workspace is not authorized for merging.",
        ),
        ApplicationError::Conflict | ApplicationError::LeaseLost => blocked(
            ToolBlockKind::Conflict,
            "The child, workspace or execution changed before merge admission.",
        ),
        _ => blocked(
            ToolBlockKind::Unavailable,
            "The workspace merge could not be prepared.",
        ),
    }
}
fn uncertain(id: &OperationId) -> ToolDispatchOutcome {
    ToolDispatchOutcome::Completed(Box::new(ToolDispatchResult::error(ToolOutput::text("Workspace merge",
        format!("Merge {id} has an unconfirmed outcome. Inspect its authoritative receipt before further execution.")))
        .with_uncertain_outcome(UncertainOutcome {tool:WIRE_ID.to_owned(),applied_paths:Vec::new(),cause:zuno_error::UncertainCause::LostOutcome})))
}
#[async_trait]
impl ToolDispatcher for WorkspaceMergeDispatcher {
    fn available_tools(&self) -> AvailableTools {
        let mut tools = self.inner.available_tools();
        tools.definitions.push(self.definition.clone());
        tools
    }
    fn concurrency_policy(&self, request: &DispatchRequest) -> zuno_tool::ToolConcurrencyPolicy {
        if request.call.name == WIRE_ID {
            zuno_tool::ToolConcurrencyPolicy::Exclusive
        } else {
            self.inner.concurrency_policy(request)
        }
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.name != WIRE_ID {
            return self.inner.prepare(request).await;
        }
        self.prepare_merge(request)
            .await
            .unwrap_or_else(|result| PreparedToolDispatch::ready(*result))
    }
}
