use super::projection::*;
use super::*;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkflowDiagnostic as ApiWorkflowDiagnostic;
use codex_app_server_protocol::WorkflowDiagnosticLevel as ApiWorkflowDiagnosticLevel;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowReadParams;
use codex_app_server_protocol::WorkflowReadResponse;
use codex_app_server_protocol::WorkflowSummary;
use codex_app_server_protocol::WorkflowValidateParams;
use codex_app_server_protocol::WorkflowValidateResponse;
use std::sync::Arc;
use zuno_workflows::LocalWorkflowLoadLimits;
use zuno_workflows::LocalWorkflowRoot;
use zuno_workflows::RegisteredWorkflow;
use zuno_workflows::WorkflowDefinition;
use zuno_workflows::WorkflowSource;
use zuno_workflows::WorkflowSourceScope;
use zuno_workflows::load_local_workflow_registry;

impl WorkflowRequestProcessor {
    pub(crate) async fn list(
        &self,
        params: WorkflowListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let snapshot = self.discover(params.cwds, params.force_reload).await?;
        Ok(Some(
            WorkflowListResponse {
                data: snapshot
                    .workflows
                    .iter()
                    .map(|(workflow_id, workflow)| workflow_summary(workflow_id, workflow))
                    .collect(),
                diagnostics: snapshot.diagnostics.iter().map(api_diagnostic).collect(),
            }
            .into(),
        ))
    }

    pub(crate) async fn read(
        &self,
        params: WorkflowReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let workflow = self.resolve_workflow(&params.workflow_id).await?;
        let document_bytes =
            read_workflow_document(&params.workflow_id, workflow.document_path()).await?;
        let actual_digest = full_digest(&document_bytes);
        if actual_digest != workflow.workflow().identity().digest {
            return Err(stale_workflow_document(&params.workflow_id, None));
        }
        let document = String::from_utf8(document_bytes).map_err(|error| {
            stale_workflow_document(
                &params.workflow_id,
                Some(format!("document is no longer UTF-8: {error}")),
            )
        })?;
        let definition =
            serde_json::to_value(workflow.workflow().definition()).map_err(|error| {
                internal_error(format!(
                    "failed to encode workflow `{}` definition: {error}",
                    params.workflow_id
                ))
            })?;
        Ok(Some(
            WorkflowReadResponse {
                workflow: workflow_summary(&params.workflow_id, &workflow),
                format: api_document_format(workflow.document_path())?,
                document,
                definition,
            }
            .into(),
        ))
    }

    pub(crate) async fn validate(
        &self,
        params: WorkflowValidateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let format = workflow_format(params.format);
        let response = match WorkflowDefinition::parse(&params.document, format, &params.source_id)
        {
            Ok(workflow) if workflow.definition().spec.script_file.is_some() => {
                WorkflowValidateResponse {
                    valid: false,
                    workflow: None,
                    diagnostics: vec![ApiWorkflowDiagnostic {
                        level: ApiWorkflowDiagnosticLevel::Error,
                        code: "external-script-context-required".to_string(),
                        message: "standalone validation cannot bind scriptFile bytes; install the document under a workflow root and use workflow/list"
                            .to_string(),
                        source_scope: None,
                        source_id: Some(params.source_id),
                        path: None,
                    }],
                }
            }
            Ok(workflow) => {
                let scope = WorkflowSourceScope::User;
                let identity = workflow.identity();
                WorkflowValidateResponse {
                    valid: true,
                    workflow: Some(WorkflowSummary {
                        workflow_id: identity.source.clone(),
                        identity: api_source_identity(identity),
                        executable_digest: identity.digest.clone(),
                        description: workflow.definition().metadata.description.clone(),
                        engine: api_engine(workflow.definition().spec.engine),
                        scope: api_scope(scope),
                    }),
                    diagnostics: Vec::new(),
                }
            }
            Err(error) => WorkflowValidateResponse {
                valid: false,
                workflow: None,
                diagnostics: vec![ApiWorkflowDiagnostic {
                    level: ApiWorkflowDiagnosticLevel::Error,
                    code: "invalid-document".to_string(),
                    message: error.to_string(),
                    source_scope: None,
                    source_id: Some(params.source_id),
                    path: None,
                }],
            },
        };
        Ok(Some(response.into()))
    }

    pub(super) async fn discover(
        &self,
        mut cwds: Vec<codex_utils_absolute_path::AbsolutePathBuf>,
        force_reload: bool,
    ) -> Result<Arc<WorkflowCatalogSnapshot>, JSONRPCErrorError> {
        if cwds.is_empty() {
            cwds.push(self.config.cwd.clone());
        }
        cwds.sort();
        cwds.dedup();
        if cwds.iter().any(|cwd| cwd != &self.config.cwd) {
            return Err(invalid_request(format!(
                "workflow discovery is limited to the app server's trusted working directory {}",
                self.config.cwd.display()
            )));
        }
        if cwds.len() > MAX_DISCOVERY_CWDS {
            return Err(invalid_request(format!(
                "workflow discovery accepts at most {MAX_DISCOVERY_CWDS} working directories"
            )));
        }
        let key = cwds
            .iter()
            .map(|cwd| cwd.as_path().to_string_lossy())
            .collect::<Vec<_>>()
            .join("\u{0}");
        if !force_reload
            && let Some(snapshot) = self.catalog.read().await.snapshots.get(&key).cloned()
        {
            return Ok(snapshot);
        }

        let mut roots = Vec::new();
        roots.push(LocalWorkflowRoot::optional(
            WorkflowSource::new(WorkflowSourceScope::User, "user")
                .map_err(workflow_invalid_request)?,
            self.config.codex_home.join(USER_WORKFLOW_ROOT),
        ));
        for cwd in &cwds {
            roots.push(LocalWorkflowRoot::optional(
                WorkflowSource::new(
                    WorkflowSourceScope::Project,
                    format!(
                        "project-{}",
                        short_digest(cwd.as_path().to_string_lossy().as_bytes())
                    ),
                )
                .map_err(workflow_invalid_request)?,
                cwd.join(PROJECT_WORKFLOW_ROOT),
            ));
        }

        let plugin_outcome = self
            .thread_manager
            .plugins_manager()
            .plugins_for_config(&self.config.plugins_config_input())
            .await;
        for (path, identity) in plugin_outcome.effective_plugin_workflow_roots() {
            roots.push(LocalWorkflowRoot::required(
                WorkflowSource::new(WorkflowSourceScope::Plugin, identity.plugin_id)
                    .map_err(workflow_invalid_request)?,
                path,
            ));
        }

        let outcome = tokio::task::spawn_blocking(move || {
            load_local_workflow_registry(roots, LocalWorkflowLoadLimits::default())
        })
        .await
        .map_err(|error| internal_error(format!("workflow discovery task failed: {error}")))?;
        let workflows = outcome
            .registry
            .iter()
            .map(|(_, workflow)| {
                (
                    workflow.workflow().identity().source.clone(),
                    Arc::clone(workflow),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let snapshot = Arc::new(WorkflowCatalogSnapshot {
            workflows,
            diagnostics: outcome.diagnostics,
        });
        let mut catalog = self.catalog.write().await;
        catalog.snapshots.insert(key, Arc::clone(&snapshot));
        while catalog.snapshots.len() > MAX_CATALOG_SNAPSHOTS {
            if let Some(oldest) = catalog.snapshots.keys().next().cloned() {
                catalog.snapshots.remove(&oldest);
            }
        }
        catalog.known = catalog
            .snapshots
            .values()
            .flat_map(|snapshot| snapshot.workflows.iter())
            .map(|(id, workflow)| (id.clone(), Arc::clone(workflow)))
            .collect();
        Ok(snapshot)
    }

    pub(super) async fn resolve_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Arc<RegisteredWorkflow>, JSONRPCErrorError> {
        if let Some(workflow) = self.catalog.read().await.known.get(workflow_id).cloned() {
            return Ok(workflow);
        }
        let snapshot = self.discover(Vec::new(), false).await?;
        snapshot
            .workflows
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| invalid_request(format!("workflow `{workflow_id}` was not found")))
    }
}
