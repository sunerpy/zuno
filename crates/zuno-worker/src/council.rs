//! The native Council tool admits durable native Jobs through the state owner.
use super::*;
use crate::{child::ChildToolDispatcher, workflow::GroupKind};
use tokio_util::sync::CancellationToken;
use zuno_application::{
    child::{ChildDelivery, ChildInvocation},
    council::{CouncilCommand, CouncilInvocation},
    workflow::WorkflowDispatch,
};
use zuno_engine::r#loop::{
    AvailableTools, DispatchRequest, PreparedToolDispatch, ToolBlockKind, ToolDispatchOutcome,
    ToolDispatchResult, ToolDispatcher, UncertainOutcome,
};
use zuno_error::ToolError;
use zuno_orchestration::CouncilPresetDescriptor;
use zuno_tool::{
    HistoryPolicy, PermissionAsk, PermissionAsker, PermissionOrigin, ToolContext, ToolDefinition,
    ToolOutput, ToolUiIntent,
};
use zuno_tools::{
    council::{
        CouncilHost, CouncilParams, CouncilRequest, CouncilTool, CouncilTurn, PERMISSION_KEY,
        WIRE_ID,
    },
    orchestration_dispatch::OrchestrationDispatch,
    task::ReportDelivery,
};
use zuno_types::identity::InvocationId;

impl WorkerClient {
    pub(crate) async fn council_command(
        &self,
        execution: &WorkerExecution,
        command: CouncilCommand,
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
        let response = self.post(COUNCIL_PATH, Some(&grant), body).await?;
        serde_json::from_slice(&response).map_err(|_| TurnStateError::InvalidData)
    }
}

fn host_error(error: TurnStateError) -> ToolError {
    match error {
        TurnStateError::Forbidden => ToolError::Denied {
            tool: WIRE_ID.to_owned(),
            denial: None,
        },
        _ => ToolError::Uncertain {
            tool: WIRE_ID.to_owned(),
            applied_paths: Vec::new(),
            source: Box::new(std::io::Error::other(format!(
                "Council admission must be reconciled: {error}"
            ))),
        },
    }
}
struct RemoteCouncilHost {
    child: ChildToolDispatcher,
    invocation: InvocationId,
    digest: String,
    params: CouncilParams,
    presentation: serde_json::Value,
}
#[async_trait]
impl CouncilHost for RemoteCouncilHost {
    async fn dispatch(
        &self,
        request: CouncilRequest,
        cancellation: CancellationToken,
    ) -> Result<OrchestrationDispatch<CouncilTurn>, ToolError> {
        let execution = &self.child.execution;
        if request.parent_session_id != execution.job.session_id.as_str()
            || request.preset != self.params.preset.trim()
            || request.review.is_some()
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
            .child
            .client
            .council_command(
                execution,
                CouncilCommand::Dispatch {
                    invocation: Box::new(CouncilInvocation {
                        preset: request.preset.clone(),
                        root: ChildInvocation {
                            invocation_id: self.invocation.clone(),
                            arguments_sha256: self.digest.clone(),
                            logical_key: format!("council:v1:{}", self.digest),
                            prompt: self.params.question.clone(),
                            description: self
                                .params
                                .description
                                .clone()
                                .unwrap_or_else(|| format!("{} Council", request.preset)),
                            delivery,
                            resume_session_id: None,
                            presentation: self.presentation.clone(),
                        },
                    }),
                },
            )
            .await
            .map_err(host_error)?;
        if first.group.delivery != delivery
            || first.group.wait.invocation_id != self.invocation
            || first.group.wait.turn_id != execution.job.turn_id
            || first.group.wait.arguments_sha256 != self.digest
        {
            return Err(host_error(TurnStateError::InvalidData));
        }
        let mut ids = request
            .seats
            .iter()
            .map(|seat| zuno_engine::council::seat_node(&seat.id))
            .collect::<Vec<_>>();
        ids.push(zuno_engine::council::SYNTHESIS_NODE.to_owned());
        let prepared = self
            .child
            .client
            .prepare_group(
                execution,
                self.child.gateway.as_ref().expect("validated gateway"),
                first,
                &ids,
                GroupKind::Council,
                &cancellation,
            )
            .await
            .map_err(host_error)?;
        if delivery == ChildDelivery::Foreground {
            Ok(OrchestrationDispatch::Pending(prepared.group.wait))
        } else {
            Ok(OrchestrationDispatch::Ready(CouncilTurn {
                run_id: prepared.run_id.to_string(), job_id: Some(prepared.group.job_id.to_string()),
                output: "Council was durably admitted under its fixed quorum, deadline and capacity limits.".to_owned(),
            }))
        }
    }
}

struct CouncilPermission {
    execution: WorkerExecution,
    presets: Vec<String>,
}
#[async_trait]
impl PermissionAsker for CouncilPermission {
    async fn ask(
        &self,
        origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        if tool == WIRE_ID
            && ask.permission == PERMISSION_KEY
            && origin.session_id() == self.execution.job.session_id.as_str()
            && origin.principal_scope() == &self.execution.job.principal
            && ask.patterns.len() == 1
            && self
                .presets
                .iter()
                .any(|name| ask.patterns[0] == format!("council:{name}"))
        {
            Ok(())
        } else {
            Err(host_error(TurnStateError::Forbidden))
        }
    }
}

#[derive(Clone)]
pub struct CouncilToolDispatcher {
    inner: Arc<dyn ToolDispatcher>,
    child: ChildToolDispatcher,
    presets: Vec<CouncilPresetDescriptor>,
    definition: ToolDefinition,
}
impl CouncilToolDispatcher {
    pub fn new(
        inner: Arc<dyn ToolDispatcher>,
        child: ChildToolDispatcher,
        presets: Vec<CouncilPresetDescriptor>,
    ) -> Result<Self, TurnStateError> {
        if presets.is_empty()
            || child.gateway.is_none()
            || inner
                .available_tools()
                .definitions
                .iter()
                .any(|tool| tool.id == WIRE_ID)
        {
            return Err(TurnStateError::InvalidData);
        }
        for preset in &presets {
            zuno_engine::council::validate_policy(preset)
                .map_err(|_| TurnStateError::InvalidData)?;
        }
        let mut parameters = zuno_tool::schema::params_schema::<CouncilParams>();
        // Generic Council has no authority to open or bind a native review.
        if let Some(properties) = parameters
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            properties.remove("reviewID");
            properties.remove("sourceSnapshotID");
        }
        let definition = ToolDefinition {
            id: WIRE_ID.to_owned(),
            display_name: "Run Council".to_owned(),
            description: format!(
                "Run a fixed Council with durable seats, bounded format correction and model-only synthesis. Available presets: {}.",
                presets
                    .iter()
                    .map(|preset| preset.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            parameters,
            ui_intent: ToolUiIntent::Subagent,
            history_policy: HistoryPolicy::ExactDeclaration,
            presentation: zuno_types::activity::InvocationPresentation::builtin(
                zuno_types::activity::InvocationAction::Council,
            ),
        };
        Ok(Self {
            inner,
            child,
            presets,
            definition,
        })
    }
}
fn blocked(kind: ToolBlockKind, message: &str) -> PreparedToolDispatch {
    PreparedToolDispatch::ready(ToolDispatchResult::blocked(
        ToolOutput::text("Council", message),
        kind,
    ))
}
#[async_trait]
impl ToolDispatcher for CouncilToolDispatcher {
    fn available_tools(&self) -> AvailableTools {
        let mut tools = self.inner.available_tools();
        tools.definitions.push(self.definition.clone());
        tools
    }
    fn concurrency_policy(&self, request: &DispatchRequest) -> zuno_tool::ToolConcurrencyPolicy {
        if request.call.name == WIRE_ID {
            zuno_tool::ToolConcurrencyPolicy::IsolatedBackground
        } else {
            self.inner.concurrency_policy(request)
        }
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.name != WIRE_ID {
            return self.inner.prepare(request).await;
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
                "Council does not belong to this execution",
            );
        }
        let Ok(invocation) = InvocationId::new(&request.call.id) else {
            return blocked(ToolBlockKind::InvalidArguments, "invalid Council call ID");
        };
        let digest = zuno_orchestration::sha256_json(&request.call.input);
        let mut args = request.call.input.clone();
        zuno_tool::guard::strip_cross_cutting(&mut args);
        let params = match serde_json::from_value::<CouncilParams>(args.clone()) {
            Ok(params)
                if request.call.input_error.is_none()
                    && params.review_id.is_none()
                    && params.source_snapshot_id.is_none() =>
            {
                params
            }
            _ => {
                return blocked(
                    ToolBlockKind::InvalidArguments,
                    "Council arguments do not match the active schema",
                );
            }
        };
        let host = Arc::new(RemoteCouncilHost {
            child: self.child.clone(),
            invocation: invocation.clone(),
            digest: digest.clone(),
            params: params.clone(),
            presentation: args.clone(),
        });
        let tool = match CouncilTool::new(
            self.presets.clone(),
            self.child.planner(invocation, digest, args),
            host,
        ) {
            Ok(tool) => tool,
            Err(_) => {
                return blocked(
                    ToolBlockKind::Denied,
                    "Council definition is not executable",
                );
            }
        };
        let mut context = ToolContext::new_scoped(
            request.session_id,
            request.message_id,
            request.call.id,
            request.agent,
            Arc::new(CouncilPermission {
                execution: execution.clone(),
                presets: self
                    .presets
                    .iter()
                    .map(|preset| preset.name.clone())
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
                    let output = ToolOutput::text("Council", zuno_error::source::describe(&error));
                    let result = match error {
                        ToolError::Denied { .. } => {
                            ToolDispatchResult::blocked(output, ToolBlockKind::Denied)
                        }
                        ToolError::InvalidArgs { .. } => {
                            ToolDispatchResult::blocked(output, ToolBlockKind::InvalidArguments)
                        }
                        ToolError::Uncertain { .. } => ToolDispatchResult::error(output)
                            .with_uncertain_outcome(UncertainOutcome {
                                tool: WIRE_ID.to_owned(),
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
