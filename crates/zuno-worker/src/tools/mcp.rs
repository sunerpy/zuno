use super::*;
use zuno_application::mcp::{McpOperation, McpToolBinding};
use zuno_types::activity::{
    InvocationAction, InvocationPresentation, InvocationSource, McpExposure,
};

pub(super) fn definition(binding: &McpToolBinding) -> ToolDefinition {
    ToolDefinition {
        id: binding.wire_name(),
        display_name: format!("{}/{}", binding.server.as_str(), binding.tool.as_str()),
        description: binding
            .definition
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Call this configured external MCP tool after human approval.")
            .to_owned(),
        // Exact upstream arguments are retained. No caller-controlled endpoint
        // or credential field is injected into this schema.
        parameters: binding.definition["inputSchema"].clone(),
        ui_intent: ToolUiIntent::Generic,
        history_policy: HistoryPolicy::ExactDeclaration,
        presentation: InvocationPresentation {
            action: InvocationAction::Tool,
            source: InvocationSource::Mcp {
                server: binding.server.clone(),
                tool: binding.tool.clone(),
                exposure: McpExposure::Exposed,
            },
        },
    }
}
impl GatewayToolDispatcher {
    pub(super) async fn prepare_mcp(
        &self,
        request: DispatchRequest,
        binding: McpToolBinding,
    ) -> Result<PreparedToolDispatch, Box<ToolDispatchResult>> {
        if request.session_id != self.execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &self.execution.job.principal
            || request.interrupt.is_set()
            || request.call.input_error.is_some()
            || request
                .orchestration_snapshot
                .as_ref()
                .is_some_and(|value| value.turn_id != self.execution.job.turn_id.as_str())
            || !request
                .available_tools
                .iter()
                .any(|value| crate::definition_matches(value, &definition(&binding)))
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "This MCP declaration is not authorized for this execution.",
            ));
        }
        let invocation = InvocationId::new(&request.call.id)
            .map_err(|_| blocked(ToolBlockKind::InvalidArguments, "Invalid MCP invocation."))?;
        let id = OperationId::new(format!(
            "op_{}",
            zuno_orchestration::sha256_json(&json!([
                "mcp",
                self.execution.job.principal.owner(),
                self.execution.job.id,
                invocation
            ]))
        ))
        .map_err(|_| blocked(ToolBlockKind::InvalidArguments, "Invalid MCP operation."))?;
        let GatewayReply::Environment(environment) = self
            .call(GatewayCommand::Acquire)
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid environment assignment.",
            ));
        };
        let operation = McpOperation {
            id: id.clone(),
            invocation_id: invocation.clone(),
            environment_id: environment.spec.id,
            binding,
            arguments: request.call.input.clone(),
        };
        operation.validate().map_err(|_| {
            blocked(
                ToolBlockKind::InvalidArguments,
                "MCP arguments must be a bounded JSON object.",
            )
        })?;
        let GatewayReply::Approval(approval) = self
            .call(GatewayCommand::PrepareMcp {
                operation: Box::new(operation.clone()),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(ToolBlockKind::Unavailable, "Invalid MCP approval."));
        };
        // Resource digest is verified by the gateway's immutable declaration
        // and the state owner's current profile. The Worker checks its exact
        // invocation and argument binding before accepting that approval.
        if approval.binding.job_id != self.execution.job.id
            || approval.binding.session_id != self.execution.job.session_id
            || approval.binding.turn_id != self.execution.job.turn_id
            || approval.binding.invocation_id != invocation
            || approval.binding.operation_id != id
            || approval.binding.arguments_sha256
                != zuno_orchestration::sha256_json(&operation.arguments)
            || approval.binding.effect != zuno_permission::enterprise::EffectKind::ExternalTool
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "Approval does not match the MCP invocation.",
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
                        operation_id: id.clone(),
                    },
                );
                let dispatcher = self.clone();
                Ok(PreparedToolDispatch::deferred(Box::pin(async move {
                    let submitted = dispatcher.call(GatewayCommand::SubmitMcp {
                        operation: Box::new(operation.clone()),
                    });
                    let result = tokio::select! {
                        biased;
                        _=request.interrupt.notified()=>return uncertain_mcp(&operation),
                        result=submitted=>result,
                    };
                    let reply = match result {
                        Ok(reply) => Ok(reply),
                        Err(_) => {
                            dispatcher
                                .call(GatewayCommand::InspectMcp {
                                    operation_id: id.clone(),
                                })
                                .await
                        }
                    };
                    match reply {
                        Ok(GatewayReply::Mcp(receipt))
                            if receipt.id == id && receipt.request_digest == operation.digest() =>
                        {
                            ToolDispatchOutcome::Pending(wait)
                        }
                        _ => uncertain_mcp(&operation),
                    }
                })))
            }
            _ => Err(blocked(
                ToolBlockKind::Denied,
                "Current human approval is required for this MCP call.",
            )),
        }
    }
}
fn uncertain_mcp(operation: &McpOperation) -> ToolDispatchOutcome {
    ToolDispatchOutcome::Completed(Box::new(ToolDispatchResult::error(ToolOutput::text(
        "MCP operation",format!("Operation {} has an unconfirmed outcome. Inspect the authoritative result before further effects.",operation.id),
    )).with_uncertain_outcome(UncertainOutcome {
        tool:operation.binding.wire_name(),applied_paths:Vec::new(),cause:zuno_error::UncertainCause::LostOutcome,
    })))
}
