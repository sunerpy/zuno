use super::binding::PreparedWorkflowAgentBindingSet;
use super::binding::WorkflowAgentBindingRequest;
use super::host::LedgerWorkflowHost;
use super::projection::*;
use super::*;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WorkflowRunCancelParams;
use codex_app_server_protocol::WorkflowRunCancelResponse;
use codex_app_server_protocol::WorkflowRunReadParams;
use codex_app_server_protocol::WorkflowRunReadResponse;
use codex_app_server_protocol::WorkflowRunUpdatedNotification;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use zuno_workflows::AcceptRunOutcome;
use zuno_workflows::AcceptWorkflowRun;
use zuno_workflows::GraphWorkflowEngine;
use zuno_workflows::V8WorkflowEngine;
use zuno_workflows::WorkflowEngineProvider;
use zuno_workflows::WorkflowLedgerError;
use zuno_workflows::WorkflowLedgerRun;
use zuno_workflows::WorkflowResult;
use zuno_workflows::WorkflowRunCompletion;
use zuno_workflows::WorkflowRunId;
use zuno_workflows::WorkflowRunStatus;
use zuno_workflows::WorkflowStartRequest;

impl WorkflowRequestProcessor {
    pub(crate) async fn start(
        self: &Arc<Self>,
        params: WorkflowStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let run_id = WorkflowRunId::new(&params.run_id).map_err(workflow_invalid_request)?;
        let ledger = Arc::clone(self.ledger().await?);
        let workflow = self.resolve_workflow(&params.workflow_id).await?;
        verify_executable_digest(&workflow, &params)?;
        let bindings = Arc::new(
            self.resolve_workflow_bindings(&workflow, &params.parent_thread_id)
                .await?,
        );
        let bindings_json = bindings.persisted().to_json().map_err(|error| {
            internal_error(format!("failed to encode workflow bindings: {error}"))
        })?;

        match ledger.get_run(&run_id).await {
            Ok(existing) => {
                verify_existing_start(&existing, &params, &bindings_json)?;
                if existing.status == WorkflowRunStatus::Queued {
                    self.spawn_run(workflow, run_id.clone(), Arc::clone(&bindings))
                        .await;
                }
                return Ok(Some(
                    WorkflowStartResponse {
                        run: api_run_record(existing),
                    }
                    .into(),
                ));
            }
            Err(WorkflowLedgerError::RunNotFound { .. }) => {}
            Err(error) => return Err(workflow_ledger_error(error)),
        }

        let accepted = ledger
            .accept_run(AcceptWorkflowRun {
                run_id: run_id.clone(),
                workflow: workflow.workflow().identity().clone(),
                executable_digest: workflow.executable_digest().to_string(),
                engine: workflow.workflow().definition().spec.engine,
                engine_revision: engine_revision(workflow.workflow().definition().spec.engine)?
                    .to_string(),
                bindings: bindings_json,
                parent_thread_id: params.parent_thread_id,
                args: params.args,
                // Installed user/project/plugin documents plus this explicit client
                // start call are the complete v1 admission surface. There is no
                // model-visible install or auto-start path. Any future dynamic source
                // must add a separate workflow:dynamic authorization before admission.
                pending_approval: false,
            })
            .await
            .map_err(workflow_ledger_error)?;
        let record = accepted.record().clone();
        self.publish_run(record.clone()).await;
        if matches!(accepted, AcceptRunOutcome::Created(_)) {
            self.spawn_run(workflow, run_id, bindings).await;
        }
        Ok(Some(
            WorkflowStartResponse {
                run: api_run_record(record),
            }
            .into(),
        ))
    }

    async fn resolve_workflow_bindings(
        &self,
        workflow: &RegisteredWorkflow,
        parent_thread_id: &str,
    ) -> Result<PreparedWorkflowAgentBindingSet, JSONRPCErrorError> {
        let mut routes = BTreeMap::new();
        for (route_name, route) in &workflow.workflow().definition().spec.routes {
            let binding = self
                .agent_dispatcher
                .prepare(WorkflowAgentBindingRequest {
                    parent_thread_id: parent_thread_id.to_string(),
                    route: route_name.clone(),
                    agent_ref: route.agent_ref.clone(),
                    execution_profile: route.execution_profile.clone(),
                })
                .await
                .map_err(|error| {
                    invalid_request(format!(
                        "workflow route `{route_name}` could not be bound: {error}"
                    ))
                })?;
            routes.insert(route_name.clone(), binding);
        }
        Ok(PreparedWorkflowAgentBindingSet::new(routes))
    }

    pub(crate) async fn run_read(
        &self,
        params: WorkflowRunReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let run_id = WorkflowRunId::new(params.run_id).map_err(workflow_invalid_request)?;
        let run = self
            .ledger()
            .await?
            .get_run(&run_id)
            .await
            .map_err(workflow_ledger_error)?;
        Ok(Some(
            WorkflowRunReadResponse {
                run: api_run_record(run),
            }
            .into(),
        ))
    }

    pub(crate) async fn run_cancel(
        &self,
        params: WorkflowRunCancelParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let run_id = WorkflowRunId::new(params.run_id).map_err(workflow_invalid_request)?;
        let ledger = self.ledger().await?;
        let record = ledger
            .request_cancel(&run_id, params.reason.as_deref())
            .await
            .map_err(workflow_ledger_error)?;
        self.publish_run(record.clone()).await;
        if let Some(run) = self.active_runs.lock().await.get(run_id.as_str()).cloned() {
            run.cancel(
                params
                    .reason
                    .unwrap_or_else(|| "cancelled by app-server client".to_string()),
            );
        }
        Ok(Some(
            WorkflowRunCancelResponse {
                run: api_run_record(record),
            }
            .into(),
        ))
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.cancel();
        if let Ok(active) = self.active_runs.try_lock() {
            for run in active.values() {
                run.cancel("app server is shutting down".to_string());
            }
        }
    }

    pub(super) async fn ledger(&self) -> Result<&Arc<WorkflowLedger>, JSONRPCErrorError> {
        self.ledger
            .get_or_try_init(|| async {
                let ledger = WorkflowLedger::open(&self.sqlite)
                    .await
                    .map_err(workflow_ledger_error)?;
                ledger
                    .recover_interrupted()
                    .await
                    .map_err(workflow_ledger_error)?;
                Ok(Arc::new(ledger))
            })
            .await
    }

    async fn spawn_run(
        self: &Arc<Self>,
        workflow: Arc<RegisteredWorkflow>,
        run_id: WorkflowRunId,
        bindings: Arc<PreparedWorkflowAgentBindingSet>,
    ) {
        let run_key = run_id.as_str().to_string();
        if !self.launching_runs.lock().await.insert(run_key.clone()) {
            return;
        }
        let processor = Arc::clone(self);
        tokio::spawn(async move {
            Arc::clone(&processor)
                .drive_run(workflow, run_id, bindings)
                .await;
            processor.launching_runs.lock().await.remove(&run_key);
        });
    }

    async fn drive_run(
        self: Arc<Self>,
        workflow: Arc<RegisteredWorkflow>,
        run_id: WorkflowRunId,
        bindings: Arc<PreparedWorkflowAgentBindingSet>,
    ) {
        let ledger = match self.ledger().await {
            Ok(ledger) => Arc::clone(ledger),
            Err(error) => {
                tracing::error!(
                    run_id = run_id.as_str(),
                    ?error,
                    "workflow ledger unavailable"
                );
                return;
            }
        };
        let running = match ledger.mark_run_running(&run_id).await {
            Ok(record) => record,
            Err(WorkflowLedgerError::InvalidRunTransition { .. }) => return,
            Err(error) => {
                tracing::error!(run_id = run_id.as_str(), %error, "failed to mark workflow running");
                return;
            }
        };
        self.publish_run(running.clone()).await;
        let host = Arc::new(LedgerWorkflowHost {
            ledger: Arc::clone(&ledger),
            workflow: Arc::clone(&workflow),
            run_id: run_id.clone(),
            parent_thread_id: running.parent_thread_id.clone(),
            bindings,
            dispatcher: Arc::clone(&self.agent_dispatcher),
            outgoing: Arc::clone(&self.outgoing),
            next_control_call_id: AtomicU64::new(1),
        });
        let provider: Box<dyn WorkflowEngineProvider> = match running.engine {
            zuno_workflows::WorkflowEngine::GraphV1 => Box::new(GraphWorkflowEngine::new(host)),
            zuno_workflows::WorkflowEngine::JavaScriptV1 => Box::new(V8WorkflowEngine::new(
                Arc::clone(&self.code_mode_sessions),
                host,
                /*isolated_process*/ true,
            )),
            zuno_workflows::WorkflowEngine::NodeWorkerV1 => {
                let completion = WorkflowRunCompletion::Failed {
                    error: "node-worker/v1 is not installed".to_string(),
                    agents_started: 0,
                };
                match ledger.complete_run(&run_id, completion).await {
                    Ok(record) => self.publish_run(record).await,
                    Err(error) => tracing::error!(
                        run_id = run_id.as_str(),
                        %error,
                        "failed to reject unavailable workflow engine"
                    ),
                }
                return;
            }
        };
        let result = async {
            let compiled = provider
                .compile(workflow.compile_request())
                .await
                .map_err(|error| error.to_string())?;
            if compiled.engine_revision != running.engine_revision {
                return Err(format!(
                    "workflow engine revision changed: recorded {}, compiled {}",
                    running.engine_revision, compiled.engine_revision
                ));
            }
            let run = provider
                .start(WorkflowStartRequest {
                    compiled: Arc::new(compiled),
                    run_id: run_id.clone(),
                    parent_thread_id: running.parent_thread_id.clone(),
                    args: running.args.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            self.active_runs
                .lock()
                .await
                .insert(run_id.as_str().to_string(), Arc::clone(&run));
            if ledger
                .get_run(&run_id)
                .await
                .ok()
                .and_then(|record| record.cancel_requested_at)
                .is_some()
            {
                run.cancel("cancelled before workflow engine admission completed".to_string());
            }
            let result = tokio::select! {
                result = run.result() => result,
                () = self.shutdown.cancelled() => {
                    run.cancel("app server is shutting down".to_string());
                    run.result().await
                }
            };
            let dispose_result = run.dispose().await;
            self.active_runs.lock().await.remove(run_id.as_str());
            if let Err(error) = dispose_result {
                tracing::warn!(run_id = run_id.as_str(), %error, "workflow engine disposal failed");
            }
            Ok::<WorkflowResult, String>(result)
        }
        .await;

        let completion = match result {
            Ok(result) => workflow_completion(result),
            Err(error) => WorkflowRunCompletion::Failed {
                error,
                agents_started: 0,
            },
        };
        // A terminal host-call outcome (especially `uncertain`) is written before
        // the engine observer settles. It is authoritative and must never be
        // overwritten or translated into a second, weaker terminal outcome.
        match ledger.get_run(&run_id).await {
            Ok(record) if record.status.is_terminal() => {
                self.publish_run(record).await;
                return;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(
                    run_id = run_id.as_str(),
                    %error,
                    "failed to inspect workflow run before completion"
                );
                return;
            }
        }
        match ledger.complete_run(&run_id, completion).await {
            Ok(record) => self.publish_run(record).await,
            Err(error) => {
                tracing::error!(run_id = run_id.as_str(), %error, "failed to complete workflow ledger run");
            }
        }
    }

    pub(super) async fn publish_run(&self, record: WorkflowLedgerRun) {
        self.outgoing
            .send_server_notification(ServerNotification::WorkflowRunUpdated(
                WorkflowRunUpdatedNotification {
                    run: api_run_record(record),
                },
            ))
            .await;
    }
}
