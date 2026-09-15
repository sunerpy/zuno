use super::binding::PreparedWorkflowAgentBindingSet;
use super::dispatcher::WorkflowAgentDispatchError;
use super::dispatcher::WorkflowAgentDispatchRequest;
use super::dispatcher::WorkflowAgentDispatcher;
use super::projection::existing_call_result;
use super::projection::host_operation;
use super::projection::workflow_engine_error;
use super::projection::workflow_ledger_engine_error;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WorkflowRunUpdatedNotification;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use zuno_workflows::AcceptCallOutcome;
use zuno_workflows::AcceptWorkflowCall;
use zuno_workflows::RegisteredWorkflow;
use zuno_workflows::WorkflowCallCompletion;
use zuno_workflows::WorkflowCallIdentity;
use zuno_workflows::WorkflowError;
use zuno_workflows::WorkflowFuture;
use zuno_workflows::WorkflowHost;
use zuno_workflows::WorkflowHostCall;
use zuno_workflows::WorkflowHostCallKind;
use zuno_workflows::WorkflowLedger;
use zuno_workflows::WorkflowRunId;

pub(super) struct LedgerWorkflowHost {
    pub(super) ledger: Arc<WorkflowLedger>,
    pub(super) workflow: Arc<RegisteredWorkflow>,
    pub(super) run_id: WorkflowRunId,
    pub(super) parent_thread_id: String,
    pub(super) bindings: Arc<PreparedWorkflowAgentBindingSet>,
    pub(super) dispatcher: Arc<dyn WorkflowAgentDispatcher>,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) next_control_call_id: AtomicU64,
}

impl WorkflowHost for LedgerWorkflowHost {
    fn call<'a>(
        &'a self,
        call: WorkflowHostCall,
        cancellation: CancellationToken,
    ) -> WorkflowFuture<'a, Result<JsonValue, WorkflowError>> {
        Box::pin(async move {
            let operation = host_operation(call.kind);
            let identity = match call.identity {
                Some(identity) => identity,
                None => {
                    let ordinal = self.next_control_call_id.fetch_add(1, Ordering::Relaxed);
                    WorkflowCallIdentity::new(format!("{operation}-{ordinal}"), &call.payload)?
                }
            };
            let accepted = self
                .ledger
                .accept_call(AcceptWorkflowCall {
                    run_id: self.run_id.clone(),
                    identity: identity.clone(),
                    operation: operation.to_string(),
                    request: call.payload.clone(),
                })
                .await
                .map_err(workflow_ledger_engine_error)?;
            if matches!(accepted, AcceptCallOutcome::Dispatch(_)) {
                self.publish_run().await;
            }
            if let AcceptCallOutcome::Existing(existing) = accepted {
                return existing_call_result(existing);
            }

            let outcome = if cancellation.is_cancelled() {
                Err((
                    WorkflowCallCompletion::Cancelled("workflow call was cancelled".to_string()),
                    workflow_engine_error("workflow call was cancelled"),
                ))
            } else if call.kind == WorkflowHostCallKind::Agent {
                let dispatch_cancellation = cancellation.clone();
                let dispatched = tokio::select! {
                    () = cancellation.cancelled() => Err(WorkflowAgentDispatchError::Cancelled(
                        "workflow agent call was cancelled".to_string(),
                    )),
                    result = self.dispatch_agent(
                        identity.id.clone(),
                        call.payload,
                        dispatch_cancellation,
                    ) => result,
                };
                dispatched.map_err(agent_dispatch_completion)
            } else {
                Ok(call.payload)
            };
            match outcome {
                Ok(value) => {
                    self.ledger
                        .complete_call(
                            &self.run_id,
                            &identity.id,
                            WorkflowCallCompletion::Completed(value.clone()),
                        )
                        .await
                        .map_err(workflow_ledger_engine_error)?;
                    self.publish_run().await;
                    Ok(value)
                }
                Err((completion, error)) => {
                    self.ledger
                        .complete_call(&self.run_id, &identity.id, completion)
                        .await
                        .map_err(workflow_ledger_engine_error)?;
                    self.publish_run().await;
                    Err(error)
                }
            }
        })
    }
}

impl LedgerWorkflowHost {
    async fn publish_run(&self) {
        let Ok(record) = self.ledger.get_run(&self.run_id).await else {
            return;
        };
        self.outgoing
            .send_server_notification(ServerNotification::WorkflowRunUpdated(
                WorkflowRunUpdatedNotification {
                    run: super::projection::api_run_record(record),
                },
            ))
            .await;
    }

    async fn dispatch_agent(
        &self,
        call_id: String,
        payload: JsonValue,
        cancellation: CancellationToken,
    ) -> Result<JsonValue, WorkflowAgentDispatchError> {
        let object = payload
            .as_object()
            .ok_or_else(|| agent_dispatch_failed("agent workflow call must be an object"))?;
        let route_name = required_string(object, "route").map_err(agent_dispatch_failed)?;
        let prompt = required_string(object, "prompt").map_err(agent_dispatch_failed)?;
        let route = self
            .workflow
            .workflow()
            .definition()
            .spec
            .routes
            .get(route_name)
            .ok_or_else(|| {
                agent_dispatch_failed(format!("unknown workflow route `{route_name}`"))
            })?;
        let prepared = self.bindings.route(route_name).cloned().ok_or_else(|| {
            agent_dispatch_failed(format!(
                "workflow route `{route_name}` has no admitted backend binding"
            ))
        })?;
        self.dispatcher
            .dispatch(
                prepared,
                WorkflowAgentDispatchRequest {
                    run_id: self.run_id.clone(),
                    call_id,
                    parent_thread_id: self.parent_thread_id.clone(),
                    route: route_name.to_string(),
                    agent_ref: route.agent_ref.clone(),
                    execution_profile: route.execution_profile.clone(),
                    prompt: prompt.to_string(),
                    options: JsonValue::Object(object.clone()),
                },
                cancellation,
            )
            .await
    }
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, JsonValue>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("workflow agent call requires `{field}`"))
}

fn agent_dispatch_completion(
    error: WorkflowAgentDispatchError,
) -> (WorkflowCallCompletion, WorkflowError) {
    match error {
        WorkflowAgentDispatchError::Failed(message) => (
            WorkflowCallCompletion::Failed(message.clone()),
            workflow_engine_error(message),
        ),
        WorkflowAgentDispatchError::Cancelled(message) => (
            WorkflowCallCompletion::Cancelled(message.clone()),
            workflow_engine_error(message),
        ),
        WorkflowAgentDispatchError::Uncertain(message) => (
            WorkflowCallCompletion::Uncertain(message.clone()),
            workflow_engine_error(message),
        ),
    }
}

fn agent_dispatch_failed(message: impl Into<String>) -> WorkflowAgentDispatchError {
    WorkflowAgentDispatchError::Failed(message.into())
}
