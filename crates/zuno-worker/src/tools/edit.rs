use super::*;
use zuno_application::workspace_edit::*;
pub const WORKSPACE_EDIT: &str = "workspace_edit";
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Arguments {
    edits: Vec<WorkspaceFileEdit>,
}
pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        id:WORKSPACE_EDIT.to_owned(),display_name:"Edit workspace files".to_owned(),
        description:"Atomically create, replace or delete bounded UTF-8 files after human review. Use expected.kind=absent for creation or expected.kind=file with the SHA-256 from workspace_read. Parent directories must exist. Links and aliased files are refused. Null content deletes an existing file. Every edit requires current human approval.".to_owned(),
        parameters:zuno_tool::schema::params_schema::<Arguments>(),ui_intent:ToolUiIntent::Generic,
        history_policy:HistoryPolicy::ExactDeclaration,presentation:zuno_types::activity::InvocationPresentation::builtin(zuno_types::activity::InvocationAction::FileEdit),
    }
}
impl GatewayToolDispatcher {
    pub(super) async fn prepare_edit(
        &self,
        request: DispatchRequest,
    ) -> Result<PreparedToolDispatch, Box<ToolDispatchResult>> {
        let definition = definition();
        if request.session_id != self.execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &self.execution.job.principal
            || request.interrupt.is_set()
            || !request
                .available_tools
                .iter()
                .any(|value| crate::definition_matches(value, &definition))
            || request
                .orchestration_snapshot
                .as_ref()
                .is_some_and(|value| value.turn_id != self.execution.job.turn_id.as_str())
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The edit is outside this execution.",
            ));
        }
        let mut input = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut input);
        let args: Arguments = serde_json::from_value(input).map_err(|_| edit_invalid())?;
        if request.call.input_error.is_some() {
            return Err(edit_invalid());
        }
        let invocation = InvocationId::new(&request.call.id).map_err(|_| edit_invalid())?;
        let id = OperationId::new(format!(
            "op_{}",
            zuno_orchestration::sha256_json(&json!([
                "workspace-edit",
                self.execution.job.principal.owner(),
                self.execution.job.id,
                invocation
            ]))
        ))
        .map_err(|_| edit_invalid())?;
        let GatewayReply::Environment(environment) = self
            .call(GatewayCommand::Acquire)
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid environment response.",
            ));
        };
        let operation = WorkspaceEditOperation {
            id: id.clone(),
            invocation_id: invocation.clone(),
            environment_id: environment.spec.id.clone(),
            expected_revision: environment.revision,
            edits: args.edits,
        };
        operation.validate().map_err(|_| edit_invalid())?;
        let GatewayReply::EditPreview(admission) = self
            .call(GatewayCommand::PreviewEdit {
                operation: Box::new(operation.clone()),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(ToolBlockKind::Unavailable, "Invalid edit preview."));
        };
        if admission.operation != operation
            || admission.lease.owner != self.execution.job.principal.owner()
            || admission.lease.job_id != self.execution.job.id
            || admission.validate().is_err()
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "Edit preview does not match the invocation.",
            ));
        }
        let GatewayReply::Approval(approval) = self
            .call(GatewayCommand::PrepareEdit {
                admission: admission.clone(),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid edit approval.",
            ));
        };
        if approval.binding.job_id != self.execution.job.id
            || approval.binding.session_id != self.execution.job.session_id
            || approval.binding.turn_id != self.execution.job.turn_id
            || approval.binding.invocation_id != invocation
            || approval.binding.operation_id != id
            || approval.binding.arguments_sha256 != admission.arguments_digest()
            || approval.binding.resources_sha256 != admission.resources_digest()
            || approval.binding.effect != zuno_permission::enterprise::EffectKind::FileWrite
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "Approval does not bind the complete edit.",
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
                    let submit = dispatcher.call(GatewayCommand::SubmitEdit { admission });
                    let result = tokio::select! {
                        biased;
                        _=request.interrupt.notified()=>return edit_uncertain(&id),
                        result=submit=>result,
                    };
                    let result = match result {
                        Ok(reply) => Ok(reply),
                        Err(_) => {
                            dispatcher
                                .call(GatewayCommand::InspectEdit {
                                    operation_id: id.clone(),
                                })
                                .await
                        }
                    };
                    match result {
                        Ok(GatewayReply::EditReceipt(receipt))
                            if receipt.id == id
                                && receipt.environment_id == operation.environment_id
                                && receipt.request_digest == operation.digest()
                                && matches!(
                                    receipt.state,
                                    WorkspaceEditState::Preparing
                                        | WorkspaceEditState::Committed
                                        | WorkspaceEditState::Cancelled
                                ) =>
                        {
                            ToolDispatchOutcome::Pending(wait)
                        }
                        _ => edit_uncertain(&id),
                    }
                })))
            }
            _ => Err(blocked(
                ToolBlockKind::Denied,
                "Current human approval does not authorize this edit.",
            )),
        }
    }
}
fn edit_invalid() -> Box<ToolDispatchResult> {
    blocked(
        ToolBlockKind::InvalidArguments,
        "Provide bounded edits with logical paths and exact prior content SHA-256 or absence.",
    )
}
fn edit_uncertain(id: &OperationId) -> ToolDispatchOutcome {
    ToolDispatchOutcome::Completed(Box::new(ToolDispatchResult::error(ToolOutput::text("Workspace edit",
        format!("Operation {id} has an unconfirmed outcome; inspect its authoritative receipt before further changes.")))
        .with_uncertain_outcome(UncertainOutcome {tool:WORKSPACE_EDIT.to_owned(),applied_paths:Vec::new(),cause:zuno_error::UncertainCause::LostOutcome})))
}
