use super::MAX_WORKFLOW_DOCUMENT_BYTES;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::WorkflowCallRecord as ApiWorkflowCallRecord;
use codex_app_server_protocol::WorkflowCallStatus as ApiWorkflowCallStatus;
use codex_app_server_protocol::WorkflowDiagnostic as ApiWorkflowDiagnostic;
use codex_app_server_protocol::WorkflowDiagnosticLevel as ApiWorkflowDiagnosticLevel;
use codex_app_server_protocol::WorkflowDocumentFormat as ApiWorkflowDocumentFormat;
use codex_app_server_protocol::WorkflowEngine as ApiWorkflowEngine;
use codex_app_server_protocol::WorkflowRunRecord as ApiWorkflowRunRecord;
use codex_app_server_protocol::WorkflowRunStatus as ApiWorkflowRunStatus;
use codex_app_server_protocol::WorkflowScope as ApiWorkflowScope;
use codex_app_server_protocol::WorkflowSourceIdentity as ApiWorkflowSourceIdentity;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowSummary;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use tokio::io::AsyncReadExt;
use zuno_workflows::RegisteredWorkflow;
use zuno_workflows::WorkflowCallStatus;
use zuno_workflows::WorkflowDiagnostic;
use zuno_workflows::WorkflowDiagnosticCode;
use zuno_workflows::WorkflowDiagnosticLevel;
use zuno_workflows::WorkflowEngine;
use zuno_workflows::WorkflowError;
use zuno_workflows::WorkflowFormat;
use zuno_workflows::WorkflowHostCallKind;
use zuno_workflows::WorkflowLedgerCall;
use zuno_workflows::WorkflowLedgerError;
use zuno_workflows::WorkflowLedgerRun;
use zuno_workflows::WorkflowResult;
use zuno_workflows::WorkflowRunCompletion;
use zuno_workflows::WorkflowRunStatus;
use zuno_workflows::WorkflowSourceScope;
use zuno_workflows::WorkflowStopReason;

pub(super) async fn read_workflow_document(
    workflow_id: &str,
    path: &Path,
) -> Result<Vec<u8>, JSONRPCErrorError> {
    let metadata = tokio::fs::symlink_metadata(path).await.map_err(|error| {
        stale_workflow_document(
            workflow_id,
            Some(format!("cannot inspect {}: {error}", path.display())),
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(stale_workflow_document(
            workflow_id,
            Some(format!("{} is no longer a regular file", path.display())),
        ));
    }
    if metadata.len() > MAX_WORKFLOW_DOCUMENT_BYTES as u64 {
        return Err(stale_workflow_document(
            workflow_id,
            Some(format!(
                "document exceeds the {MAX_WORKFLOW_DOCUMENT_BYTES}-byte read limit"
            )),
        ));
    }

    let file = tokio::fs::File::open(path).await.map_err(|error| {
        stale_workflow_document(
            workflow_id,
            Some(format!("cannot open {}: {error}", path.display())),
        )
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_WORKFLOW_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| {
            stale_workflow_document(
                workflow_id,
                Some(format!("cannot read {}: {error}", path.display())),
            )
        })?;
    if bytes.len() > MAX_WORKFLOW_DOCUMENT_BYTES {
        return Err(stale_workflow_document(
            workflow_id,
            Some(format!(
                "document exceeds the {MAX_WORKFLOW_DOCUMENT_BYTES}-byte read limit"
            )),
        ));
    }
    Ok(bytes)
}

pub(super) fn stale_workflow_document(
    workflow_id: &str,
    detail: Option<String>,
) -> JSONRPCErrorError {
    let detail = detail
        .map(|value| format!(" ({value})"))
        .unwrap_or_default();
    invalid_request(format!(
        "workflow `{workflow_id}` changed after discovery{detail}; call workflow/list with forceReload"
    ))
}

pub(super) fn verify_executable_digest(
    workflow: &RegisteredWorkflow,
    params: &WorkflowStartParams,
) -> Result<(), JSONRPCErrorError> {
    if workflow.executable_digest() != params.expected_digest {
        return Err(invalid_request(format!(
            "workflow `{}` executable digest changed: expected {}, actual {}",
            params.workflow_id,
            params.expected_digest,
            workflow.executable_digest()
        )));
    }
    Ok(())
}

pub(super) fn verify_existing_start(
    existing: &WorkflowLedgerRun,
    params: &WorkflowStartParams,
    bindings: &JsonValue,
) -> Result<(), JSONRPCErrorError> {
    if existing.workflow.source != params.workflow_id
        || existing.executable_digest != params.expected_digest
        || existing.bindings != *bindings
        || existing.parent_thread_id != params.parent_thread_id
        || existing.args != params.args
    {
        return Err(invalid_request(format!(
            "workflow run `{}` is already bound to a different start request",
            params.run_id
        )));
    }
    Ok(())
}

pub(super) fn workflow_completion(result: WorkflowResult) -> WorkflowRunCompletion {
    match result.stop_reason {
        WorkflowStopReason::Completed => WorkflowRunCompletion::Completed {
            result: result.value,
            agents_started: result.agents_started,
        },
        WorkflowStopReason::Failed => WorkflowRunCompletion::Failed {
            error: result
                .error
                .unwrap_or_else(|| "workflow failed without an error".to_string()),
            agents_started: result.agents_started,
        },
        WorkflowStopReason::Cancelled => WorkflowRunCompletion::Cancelled {
            error: result
                .error
                .unwrap_or_else(|| "workflow was cancelled".to_string()),
            agents_started: result.agents_started,
        },
        WorkflowStopReason::Uncertain => WorkflowRunCompletion::Uncertain {
            error: result
                .error
                .unwrap_or_else(|| "workflow outcome is uncertain".to_string()),
            agents_started: result.agents_started,
        },
    }
}

pub(super) fn existing_call_result(call: WorkflowLedgerCall) -> Result<JsonValue, WorkflowError> {
    match call.status {
        WorkflowCallStatus::Completed => Ok(call.result.unwrap_or(JsonValue::Null)),
        WorkflowCallStatus::Running => Err(workflow_engine_error(format!(
            "workflow call `{}` is already running; refusing duplicate dispatch",
            call.call_id
        ))),
        WorkflowCallStatus::Failed
        | WorkflowCallStatus::Cancelled
        | WorkflowCallStatus::Uncertain => {
            Err(workflow_engine_error(call.error.unwrap_or_else(|| {
                format!("workflow call `{}` ended as {}", call.call_id, call.status)
            })))
        }
    }
}

pub(super) fn workflow_summary(
    workflow_id: &str,
    workflow: &RegisteredWorkflow,
) -> WorkflowSummary {
    let validated = workflow.workflow();
    WorkflowSummary {
        workflow_id: workflow_id.to_string(),
        identity: api_source_identity(validated.identity()),
        executable_digest: workflow.executable_digest().to_string(),
        description: validated.definition().metadata.description.clone(),
        engine: api_engine(validated.definition().spec.engine),
        scope: api_scope(workflow.source().scope),
    }
}

pub(super) fn api_source_identity(
    identity: &zuno_workflows::WorkflowSourceIdentity,
) -> ApiWorkflowSourceIdentity {
    ApiWorkflowSourceIdentity {
        source: identity.source.clone(),
        name: identity.name.clone(),
        version: identity.version.clone(),
        digest: identity.digest.clone(),
    }
}

pub(super) fn engine_revision(engine: WorkflowEngine) -> Result<&'static str, JSONRPCErrorError> {
    match engine {
        WorkflowEngine::GraphV1 => Ok("graph/v1+zuno/v1"),
        WorkflowEngine::JavaScriptV1 => Ok("javascript/v1+code-mode/v1"),
        WorkflowEngine::NodeWorkerV1 => Err(invalid_request(
            "node-worker/v1 is not installed; configure an external engine provider",
        )),
    }
}

pub(super) fn api_engine(engine: WorkflowEngine) -> ApiWorkflowEngine {
    match engine {
        WorkflowEngine::GraphV1 => ApiWorkflowEngine::GraphV1,
        WorkflowEngine::JavaScriptV1 => ApiWorkflowEngine::JavaScriptV1,
        WorkflowEngine::NodeWorkerV1 => ApiWorkflowEngine::NodeWorkerV1,
    }
}

pub(super) fn api_scope(scope: WorkflowSourceScope) -> ApiWorkflowScope {
    match scope {
        WorkflowSourceScope::User => ApiWorkflowScope::User,
        WorkflowSourceScope::Project => ApiWorkflowScope::Project,
        WorkflowSourceScope::Plugin => ApiWorkflowScope::Plugin,
    }
}

pub(super) fn api_document_format(
    path: &Path,
) -> Result<ApiWorkflowDocumentFormat, JSONRPCErrorError> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => Ok(ApiWorkflowDocumentFormat::Json),
        Some("yaml" | "yml") => Ok(ApiWorkflowDocumentFormat::Yaml),
        _ => Err(internal_error(format!(
            "workflow document {} has no supported format",
            path.display()
        ))),
    }
}

pub(super) fn workflow_format(format: ApiWorkflowDocumentFormat) -> WorkflowFormat {
    match format {
        ApiWorkflowDocumentFormat::Json => WorkflowFormat::Json,
        ApiWorkflowDocumentFormat::Yaml => WorkflowFormat::Yaml,
    }
}

pub(super) fn api_diagnostic(diagnostic: &WorkflowDiagnostic) -> ApiWorkflowDiagnostic {
    ApiWorkflowDiagnostic {
        level: match diagnostic.level {
            WorkflowDiagnosticLevel::Warning => ApiWorkflowDiagnosticLevel::Warning,
            WorkflowDiagnosticLevel::Error => ApiWorkflowDiagnosticLevel::Error,
        },
        code: diagnostic_code(diagnostic.code),
        message: diagnostic.message.clone(),
        source_scope: Some(api_scope(diagnostic.source.scope)),
        source_id: Some(diagnostic.source.id.clone()),
        path: Some(diagnostic.path.clone()),
    }
}

pub(super) fn diagnostic_code(code: WorkflowDiagnosticCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "workflow-discovery".to_string())
}

pub(super) fn api_run_record(record: WorkflowLedgerRun) -> ApiWorkflowRunRecord {
    ApiWorkflowRunRecord {
        run_id: record.run_id.as_str().to_string(),
        workflow: api_source_identity(&record.workflow),
        engine: api_engine(record.engine),
        engine_revision: record.engine_revision,
        parent_thread_id: record.parent_thread_id,
        request_digest: record.request_digest,
        binding_digest: record.binding_digest,
        status: match record.status {
            WorkflowRunStatus::PendingApproval => ApiWorkflowRunStatus::PendingApproval,
            WorkflowRunStatus::Queued => ApiWorkflowRunStatus::Queued,
            WorkflowRunStatus::Running => ApiWorkflowRunStatus::Running,
            WorkflowRunStatus::Completed => ApiWorkflowRunStatus::Completed,
            WorkflowRunStatus::Failed => ApiWorkflowRunStatus::Failed,
            WorkflowRunStatus::Cancelled => ApiWorkflowRunStatus::Cancelled,
            WorkflowRunStatus::Uncertain => ApiWorkflowRunStatus::Uncertain,
        },
        result: record.result,
        error: record.error,
        agents_started: record.agents_started,
        calls: record.calls.into_iter().map(api_call_record).collect(),
        created_at: record.created_at,
        updated_at: record.updated_at,
        completed_at: record.completed_at,
    }
}

pub(super) fn api_call_record(record: WorkflowLedgerCall) -> ApiWorkflowCallRecord {
    ApiWorkflowCallRecord {
        call_id: record.call_id,
        request_digest: record.request_digest,
        operation: record.operation,
        status: match record.status {
            WorkflowCallStatus::Running => ApiWorkflowCallStatus::Running,
            WorkflowCallStatus::Completed => ApiWorkflowCallStatus::Completed,
            WorkflowCallStatus::Failed => ApiWorkflowCallStatus::Failed,
            WorkflowCallStatus::Cancelled => ApiWorkflowCallStatus::Cancelled,
            WorkflowCallStatus::Uncertain => ApiWorkflowCallStatus::Uncertain,
        },
        result: record.result,
        error: record.error,
        started_at: Some(record.started_at),
        completed_at: record.completed_at,
    }
}

pub(super) fn host_operation(kind: WorkflowHostCallKind) -> &'static str {
    match kind {
        WorkflowHostCallKind::Agent => "agent",
        WorkflowHostCallKind::Phase => "phase",
        WorkflowHostCallKind::Log => "log",
        WorkflowHostCallKind::Checkpoint => "checkpoint",
    }
}

pub(super) fn workflow_invalid_request(error: WorkflowError) -> JSONRPCErrorError {
    invalid_request(error.to_string())
}

pub(super) fn workflow_ledger_error(error: WorkflowLedgerError) -> JSONRPCErrorError {
    match error {
        WorkflowLedgerError::RunNotFound { .. }
        | WorkflowLedgerError::RunReplayDiverged { .. }
        | WorkflowLedgerError::CallReplayDiverged { .. }
        | WorkflowLedgerError::InvalidRunTransition { .. }
        | WorkflowLedgerError::InvalidValue(_) => invalid_request(error.to_string()),
        error => internal_error(error.to_string()),
    }
}

pub(super) fn workflow_ledger_engine_error(error: WorkflowLedgerError) -> WorkflowError {
    workflow_engine_error(error.to_string())
}

pub(super) fn workflow_engine_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::Engine {
        engine: WorkflowEngine::JavaScriptV1,
        message: message.into(),
    }
}

pub(super) fn short_digest(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn full_digest(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
