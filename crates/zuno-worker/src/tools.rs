//! Durable gateway tools. Preparation can wait for approval; actual submission
//! starts only after the kernel's durable handoff and can yield an operation wait.

use crate::{WorkerClient, WorkerExecution, gateway::GatewayClient};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use zuno_application::{
    ApplicationError,
    authorization::ApprovalState,
    environment::{
        CommandOperation, Environment, OperationPhase,
        wire::{GatewayCommand, GatewayReply, GatewayRequest},
    },
};
use zuno_engine::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchOutcome,
    ToolDispatchResult, ToolDispatcher, UncertainOutcome,
};
use zuno_llm::cache::McpToolStatus;
use zuno_tool::{HistoryPolicy, ToolDefinition, ToolOutput, ToolUiIntent};
use zuno_types::{
    identity::{InvocationId, OperationId, WaitId},
    wait::{WaitContinuation, WaitRef, WaitTarget},
};

pub const ENVIRONMENT_COMMAND: &str = "environment_command";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CommandArguments {
    /// Executable and arguments in the assigned environment's /workspace.
    /// Invoke an explicit shell in argv when shell expansion is intended.
    argv: Vec<String>,
}

#[derive(Clone)]
pub struct GatewayToolDispatcher {
    state: WorkerClient,
    execution: WorkerExecution,
    gateway: Arc<GatewayClient>,
    definition: ToolDefinition,
}

impl GatewayToolDispatcher {
    pub fn new(
        state: WorkerClient,
        execution: WorkerExecution,
        gateway: Arc<GatewayClient>,
    ) -> Self {
        Self {
            state,
            execution,
            gateway,
            definition: Self::definition(),
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            id:ENVIRONMENT_COMMAND.to_owned(),display_name:"Environment command".to_owned(),
            description:"Run an argv command in the assigned isolated workspace. Requires current human approval. Returns only an authoritative command result; an operation may wait for completion. Shell expansion requires an explicit shell executable.".to_owned(),
            parameters:zuno_tool::schema::params_schema::<CommandArguments>(),
            ui_intent:ToolUiIntent::Generic,history_policy:HistoryPolicy::ExactDeclaration,
        }
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
        let digest = zuno_orchestration::sha256_json(&json!([
            "gateway-wait",
            self.execution.job.id,
            request.call.id,
            target,
        ]));
        WaitRef {
            id: WaitId::new(format!("wait_{digest}")).expect("derived identity"),
            turn_id: self.execution.job.turn_id.clone(),
            invocation_id: InvocationId::new(&request.call.id).expect("validated invocation"),
            arguments_sha256: zuno_orchestration::sha256_json(&request.call.input),
            target,
            continuation: WaitContinuation::CurrentTurn,
        }
    }

    async fn prepare_command(
        &self,
        request: DispatchRequest,
    ) -> Result<PreparedToolDispatch, Box<ToolDispatchResult>> {
        if request.call.name != ENVIRONMENT_COMMAND
            || request.session_id != self.execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &self.execution.job.principal
            || !request
                .available_tools
                .iter()
                .any(|definition| definition == &self.definition)
            || request
                .orchestration_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.turn_id != self.execution.job.turn_id.as_str())
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The invocation is outside this execution's scope.",
            ));
        }
        if request.interrupt.is_set() {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The invocation was interrupted before submission.",
            ));
        }
        let invocation = InvocationId::new(&request.call.id).map_err(|_| invalid())?;
        if request.call.input_error.is_some() {
            return Err(invalid());
        }
        let mut input = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut input);
        let arguments: CommandArguments = serde_json::from_value(input).map_err(|_| invalid())?;
        CommandOperation::validate_arguments(&arguments.argv).map_err(|_| invalid())?;
        let id = OperationId::new(format!(
            "op_{}",
            zuno_orchestration::sha256_json(&json!([
                "environment-command",
                self.execution.job.principal.owner(),
                self.execution.job.id,
                invocation,
            ]))
        ))
        .expect("derived identity");
        let GatewayReply::Environment(environment) = self
            .call(GatewayCommand::Acquire)
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "The environment returned an invalid response.",
            ));
        };
        if environment.owner != self.execution.job.principal.owner()
            || environment.spec.session_id != self.execution.job.session_id
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The environment does not belong to this invocation.",
            ));
        }
        let operation = CommandOperation {
            id,
            invocation_id: invocation,
            environment_id: environment.spec.id.clone(),
            expected_revision: environment.revision,
            argv: arguments.argv,
        };
        let GatewayReply::Approval(approval) = self
            .call(GatewayCommand::PrepareCommand {
                operation: operation.clone(),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "The approval service returned an invalid response.",
            ));
        };
        if approval.binding.job_id != self.execution.job.id
            || approval.binding.session_id != self.execution.job.session_id
            || approval.binding.turn_id != self.execution.job.turn_id
            || approval.binding.invocation_id != operation.invocation_id
            || approval.binding.operation_id != operation.id
            || approval.binding.arguments_sha256
                != zuno_orchestration::sha256_json(&json!(operation.argv))
            || approval.binding.resources_sha256 != resource_digest(&environment)
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The approval does not match this command and workspace.",
            ));
        }
        match approval.state {
            ApprovalState::Pending => Ok(PreparedToolDispatch::Pending(self.wait(
                &request,
                WaitTarget::Approval {
                    approval_id: approval.id.clone(),
                },
            ))),
            ApprovalState::Approved | ApprovalState::Automatic => {
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
                            "The invocation was interrupted before submission.",
                        ));
                    }
                    let submit = dispatcher.call(GatewayCommand::SubmitCommand {
                        operation: operation.clone(),
                    });
                    let result = tokio::select! {
                        biased;
                        _=request.interrupt.notified()=>return uncertain(&operation),
                        result=submit=>result,
                    };
                    let result = match result {
                        Ok(reply) => Ok(reply),
                        Err(_) => {
                            dispatcher
                                .call(GatewayCommand::Inspect {
                                    operation_id: operation.id.clone(),
                                })
                                .await
                        }
                    };
                    match result {
                        Ok(GatewayReply::Operation(receipt))
                            if receipt.id == operation.id
                                && receipt.environment_id == operation.environment_id
                                && matches!(
                                    receipt.phase,
                                    OperationPhase::Running
                                        | OperationPhase::Completed
                                        | OperationPhase::Cancelled
                                ) =>
                        {
                            ToolDispatchOutcome::Pending(wait)
                        }
                        _ => uncertain(&operation),
                    }
                })))
            }
            ApprovalState::Rejected | ApprovalState::Expired | ApprovalState::Invalidated => {
                Err(blocked(
                    ToolBlockKind::Denied,
                    "Current human approval does not authorize this command.",
                ))
            }
        }
    }
}

fn resource_digest(environment: &Environment) -> String {
    zuno_orchestration::sha256_json(&json!([
        environment.owner,
        environment.spec,
        environment.revision
    ]))
}
fn blocked(kind: ToolBlockKind, message: &str) -> Box<ToolDispatchResult> {
    Box::new(ToolDispatchResult::blocked(
        ToolOutput::text("Environment command", message),
        kind,
    ))
}
fn invalid() -> Box<ToolDispatchResult> {
    blocked(
        ToolBlockKind::InvalidArguments,
        "Provide a bounded argv array with a nonempty executable.",
    )
}
fn before_submission(error: ApplicationError) -> Box<ToolDispatchResult> {
    match error {
        ApplicationError::Forbidden => blocked(
            ToolBlockKind::Denied,
            "The environment operation is not authorized.",
        ),
        ApplicationError::Conflict | ApplicationError::LeaseLost => blocked(
            ToolBlockKind::Conflict,
            "The execution or workspace version changed before submission.",
        ),
        _ => blocked(
            ToolBlockKind::Unavailable,
            "The environment could not prepare this invocation.",
        ),
    }
}
fn uncertain(operation: &CommandOperation) -> ToolDispatchOutcome {
    let output = ToolOutput::text(
        "Environment command",
        format!(
            "Operation {} has an unconfirmed outcome. Inspect its authoritative receipt before further execution.",
            operation.id,
        ),
    );
    ToolDispatchOutcome::Completed(Box::new(
        ToolDispatchResult::error(output).with_uncertain_outcome(UncertainOutcome {
            tool: ENVIRONMENT_COMMAND.to_owned(),
            applied_paths: Vec::new(),
            cause: zuno_error::UncertainCause::LostOutcome,
        }),
    ))
}
#[async_trait]
impl ToolDispatcher for GatewayToolDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(vec![self.definition.clone()], McpToolStatus::Ready)
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        self.prepare_command(request)
            .await
            .unwrap_or_else(|result| PreparedToolDispatch::ready(*result))
    }
}
