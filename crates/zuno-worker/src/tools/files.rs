use super::*;
use zuno_application::{
    workspace_files::{WorkspaceFileOperation, WorkspaceFileQuery, WorkspaceFileResult},
    workspace_merge::WorkspacePath,
};
use zuno_types::activity::{Counter, InvocationAction, InvocationPresentation};

pub const WORKSPACE_READ: &str = "workspace_read";
pub const WORKSPACE_LIST: &str = "workspace_list";
pub const WORKSPACE_SEARCH: &str = "workspace_search";

fn bytes() -> u32 {
    65536
}
fn entries() -> u32 {
    100
}
fn matches() -> u32 {
    50
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadArguments {
    path: WorkspacePath,
    #[serde(default)]
    offset: Counter,
    #[serde(default = "bytes")]
    maximum_bytes: u32,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListArguments {
    #[serde(default = "WorkspacePath::root")]
    path: WorkspacePath,
    after: Option<WorkspacePath>,
    #[serde(default = "entries")]
    limit: u32,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchArguments {
    #[serde(default = "WorkspacePath::root")]
    path: WorkspacePath,
    text: String,
    #[serde(default = "matches")]
    limit: u32,
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    [
        (WORKSPACE_READ,"Read workspace file","Read a bounded UTF-8 byte window from a logical workspace path. Offset is a decimal byte counter. Binary files and links return metadata; links are not followed. The server applies current approval policy.",InvocationAction::FileRead,zuno_tool::schema::params_schema::<ReadArguments>()),
        (WORKSPACE_LIST,"List workspace directory","List one logical workspace directory with a bounded page and after cursor. The server applies current approval policy.",InvocationAction::FileList,zuno_tool::schema::params_schema::<ListArguments>()),
        (WORKSPACE_SEARCH,"Search workspace text","Search for literal case-sensitive text in the logical workspace. Returns bounded line matches and reports skipped binary/large files. The server applies current approval policy.",InvocationAction::FileSearch,zuno_tool::schema::params_schema::<SearchArguments>()),
    ].into_iter().map(|(id,name,description,action,parameters)|ToolDefinition {
        id:id.to_owned(),display_name:name.to_owned(),description:description.to_owned(),
        parameters,ui_intent:ToolUiIntent::Generic,history_policy:HistoryPolicy::ExactDeclaration,
        presentation:InvocationPresentation::builtin(action),
    }).collect()
}

impl GatewayToolDispatcher {
    pub(super) async fn prepare_files(
        &self,
        request: DispatchRequest,
    ) -> Result<PreparedToolDispatch, Box<ToolDispatchResult>> {
        let definition = definitions()
            .into_iter()
            .find(|definition| definition.id == request.call.name)
            .ok_or_else(|| {
                blocked(
                    ToolBlockKind::Denied,
                    "This file operation is not installed.",
                )
            })?;
        if request.session_id != self.execution.job.session_id.as_str()
            || request.principal_scope.as_ref() != &self.execution.job.principal
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
                "The file invocation is outside this execution.",
            ));
        }
        if request.interrupt.is_set() {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The file invocation was interrupted.",
            ));
        }
        if request.call.input_error.is_some() {
            return Err(file_invalid());
        }
        let mut input = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut input);
        let query = match request.call.name.as_str() {
            WORKSPACE_READ => {
                let value: ReadArguments =
                    serde_json::from_value(input).map_err(|_| file_invalid())?;
                WorkspaceFileQuery::Read {
                    path: value.path,
                    offset: value.offset,
                    maximum_bytes: value.maximum_bytes,
                }
            }
            WORKSPACE_LIST => {
                let value: ListArguments =
                    serde_json::from_value(input).map_err(|_| file_invalid())?;
                WorkspaceFileQuery::List {
                    path: value.path,
                    after: value.after,
                    limit: value.limit,
                }
            }
            WORKSPACE_SEARCH => {
                let value: SearchArguments =
                    serde_json::from_value(input).map_err(|_| file_invalid())?;
                WorkspaceFileQuery::Search {
                    path: value.path,
                    text: value.text,
                    limit: value.limit,
                }
            }
            _ => return Err(file_invalid()),
        };
        query.validate().map_err(|_| file_invalid())?;
        let invocation = InvocationId::new(&request.call.id).map_err(|_| file_invalid())?;
        let id = OperationId::new(format!(
            "op_{}",
            zuno_orchestration::sha256_json(&json!([
                "workspace-files",
                self.execution.job.principal.owner(),
                self.execution.job.id,
                invocation
            ]))
        ))
        .map_err(|_| file_invalid())?;
        let GatewayReply::Environment(environment) = self
            .call(GatewayCommand::Acquire)
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid workspace response.",
            ));
        };
        if environment.owner != self.execution.job.principal.owner()
            || environment.spec.session_id != self.execution.job.session_id
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The workspace does not belong to this invocation.",
            ));
        }
        let operation = WorkspaceFileOperation {
            id,
            invocation_id: invocation,
            environment_id: environment.spec.id.clone(),
            expected_revision: environment.revision,
            query,
        };
        let GatewayReply::Approval(approval) = self
            .call(GatewayCommand::PrepareFiles {
                operation: operation.clone(),
            })
            .await
            .map_err(before_submission)?
        else {
            return Err(blocked(
                ToolBlockKind::Unavailable,
                "Invalid workspace approval response.",
            ));
        };
        if approval.binding.job_id != self.execution.job.id
            || approval.binding.session_id != self.execution.job.session_id
            || approval.binding.turn_id != self.execution.job.turn_id
            || approval.binding.invocation_id != operation.invocation_id
            || approval.binding.operation_id != operation.id
            || approval.binding.effect != operation.query.effect()
            || approval.binding.arguments_sha256
                != zuno_orchestration::sha256_json(&json!(operation.query))
            || approval.binding.resources_sha256 != resource_digest(&environment)
        {
            return Err(blocked(
                ToolBlockKind::Denied,
                "The file approval does not match its query.",
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
                let dispatcher = self.clone();
                Ok(PreparedToolDispatch::deferred(Box::pin(async move {
                    let result = tokio::select! {
                        biased;
                        _=request.interrupt.notified()=>return ToolDispatchOutcome::Completed(blocked(ToolBlockKind::Denied,"File query interrupted.")),
                        result=dispatcher.call(GatewayCommand::QueryFiles{operation:operation.clone()})=>result,
                    };
                    match result {
                        Ok(GatewayReply::Files(receipt))
                            if receipt.validate_for(&operation).is_ok() =>
                        {
                            let text = match &receipt.result {
                                WorkspaceFileResult::Read {
                                    text: Some(text), ..
                                } => text.clone(),
                                _ => serde_json::to_string(&receipt.result).unwrap_or_default(),
                            };
                            let output = ToolOutput::text(&definition.display_name, text)
                                .with_metadata("workspaceFile", json!(receipt));
                            ToolDispatchOutcome::Completed(Box::new(ToolDispatchResult::success(
                                output,
                            )))
                        }
                        Err(error) => ToolDispatchOutcome::Completed(before_submission(error)),
                        _ => ToolDispatchOutcome::Completed(blocked(
                            ToolBlockKind::Unavailable,
                            "Invalid file query response.",
                        )),
                    }
                })))
            }
            _ => Err(blocked(
                ToolBlockKind::Denied,
                "Current approval does not authorize this file query.",
            )),
        }
    }
}
fn file_invalid() -> Box<ToolDispatchResult> {
    blocked(
        ToolBlockKind::InvalidArguments,
        "Use a logical workspace path and bounded file query arguments.",
    )
}
