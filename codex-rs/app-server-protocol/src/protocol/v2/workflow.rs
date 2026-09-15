use crate::JsonSchema;
use crate::TS;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// Source encoding for a user-, project-, or plugin-owned workflow document.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum WorkflowDocumentFormat {
    Json,
    Yaml,
}

/// Execution engine selected by a `zuno.workflow/v1` document.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub enum WorkflowEngine {
    #[serde(rename = "graph/v1")]
    #[ts(rename = "graph/v1")]
    GraphV1,
    #[serde(rename = "javascript/v1")]
    #[ts(rename = "javascript/v1")]
    JavaScriptV1,
    #[serde(rename = "node-worker/v1")]
    #[ts(rename = "node-worker/v1")]
    NodeWorkerV1,
}

/// Discovery authority that supplied a workflow. This is provenance, not a
/// precedence promise; `workflowId` remains the opaque selector.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum WorkflowScope {
    User,
    Project,
    Plugin,
}

/// Immutable identity for one exact workflow source document.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowSourceIdentity {
    /// Human-readable source provenance, such as a path or plugin resource id.
    pub source: String,
    pub name: String,
    pub version: String,
    /// SHA-256 digest of the exact source bytes.
    pub digest: String,
}

/// A discovered and validated workflow. Zuno does not reserve names for
/// built-in business workflows; every entry comes from a discoverable source.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowSummary {
    /// Opaque registry selector. Callers must not derive this from the name.
    pub workflow_id: String,
    pub identity: WorkflowSourceIdentity,
    /// Digest of all executable material. For `scriptFile` workflows this
    /// binds both the document and resolved script bytes.
    pub executable_digest: String,
    pub description: Option<String>,
    pub engine: WorkflowEngine,
    pub scope: WorkflowScope,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListParams {
    /// Working directories used for project and plugin discovery. Empty means
    /// the app-server process working directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cwds: Vec<AbsolutePathBuf>,
    /// Re-read configured roots instead of accepting a registry cache hit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_reload: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListResponse {
    pub data: Vec<WorkflowSummary>,
    /// Source-local discovery failures. A malformed document does not hide an
    /// independent valid workflow from another source.
    #[serde(default)]
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowReadParams {
    pub workflow_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowReadResponse {
    pub workflow: WorkflowSummary,
    pub format: WorkflowDocumentFormat,
    /// Exact source text. Keeping the text preserves YAML comments and makes
    /// the digest independently verifiable by clients.
    pub document: String,
    /// Parsed definition for clients that do not need source-level editing.
    pub definition: JsonValue,
}

/// Validate an arbitrary document without installing it or mutating discovery
/// roots. The source id is only provenance for diagnostics and digesting.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowValidateParams {
    pub source_id: String,
    pub format: WorkflowDocumentFormat,
    pub document: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowDiagnostic {
    pub level: WorkflowDiagnosticLevel,
    pub code: String,
    pub message: String,
    pub source_scope: Option<WorkflowScope>,
    pub source_id: Option<String>,
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum WorkflowDiagnosticLevel {
    Warning,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowValidateResponse {
    pub valid: bool,
    pub workflow: Option<WorkflowSummary>,
    #[serde(default)]
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

/// Durable state of an accepted workflow run.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum WorkflowRunStatus {
    PendingApproval,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// At least one side effect has an unknown outcome and must be inspected
    /// before any replay is considered.
    Uncertain,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum WorkflowCallStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

/// One engine-to-host call in the durable ledger. The request digest allows a
/// recovered engine to detect divergent replay before dispatching a side effect.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowCallRecord {
    pub call_id: String,
    pub request_digest: String,
    /// Extensible operation label (for example `agent`, `phase`, `checkpoint`).
    pub operation: String,
    pub status: WorkflowCallStatus,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    #[ts(type = "number | null")]
    pub started_at: Option<i64>,
    #[ts(type = "number | null")]
    pub completed_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunRecord {
    pub run_id: String,
    pub workflow: WorkflowSourceIdentity,
    pub engine: WorkflowEngine,
    pub engine_revision: String,
    pub parent_thread_id: String,
    /// Digest of the canonical start request, including workflow digest and args.
    pub request_digest: String,
    /// Digest of the frozen backend, plugin generation, execution profile, and
    /// effective runtime policy selected before this run was admitted.
    pub binding_digest: String,
    pub status: WorkflowRunStatus,
    pub result: Option<JsonValue>,
    pub error: Option<String>,
    pub agents_started: u32,
    #[serde(default)]
    pub calls: Vec<WorkflowCallRecord>,
    #[ts(type = "number")]
    pub created_at: i64,
    #[ts(type = "number")]
    pub updated_at: i64,
    #[ts(type = "number | null")]
    pub completed_at: Option<i64>,
}

/// Start one exact workflow revision. `runId` is client supplied so a retry
/// after a lost response is idempotent. Reusing it with different args or a
/// different digest must fail closed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStartParams {
    pub run_id: String,
    pub workflow_id: String,
    /// Must match `WorkflowSummary.executableDigest`, preventing a document or
    /// external script change between discovery and execution.
    pub expected_digest: String,
    pub parent_thread_id: String,
    #[serde(default)]
    pub args: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStartResponse {
    /// The request returns once the durable run is accepted, not when it ends.
    pub run: WorkflowRunRecord,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunReadParams {
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunReadResponse {
    pub run: WorkflowRunRecord,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunCancelParams {
    pub run_id: String,
    #[ts(optional = nullable)]
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunCancelResponse {
    /// Cancellation is idempotent; a terminal run is returned unchanged.
    pub run: WorkflowRunRecord,
}

/// Durable workflow state changed after admission, execution, a host call, or cancellation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowRunUpdatedNotification {
    pub run: WorkflowRunRecord,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientRequest;
    use crate::ClientRequestSerializationScope;
    use crate::JSONRPCRequest;
    use crate::RequestId;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn workflow_start_round_trips_and_serializes_by_run_id() {
        let request = ClientRequest::try_from(JSONRPCRequest {
            id: RequestId::Integer(7),
            method: "workflow/start".to_string(),
            params: Some(json!({
                "runId": "run-7",
                "workflowId": "user:design-review",
                "expectedDigest": "sha256",
                "parentThreadId": "thread-1",
                "args": {"issue": 42}
            })),
            trace: None,
        })
        .expect("workflow/start should decode");

        assert_eq!(request.method_name(), "workflow/start");
        assert_eq!(
            request.serialization_scope(),
            Some(ClientRequestSerializationScope::WorkflowRun {
                run_id: "run-7".to_string(),
            })
        );
        assert_eq!(
            serde_json::to_value(request).expect("workflow/start should encode"),
            json!({
                "id": 7,
                "method": "workflow/start",
                "params": {
                    "runId": "run-7",
                    "workflowId": "user:design-review",
                    "expectedDigest": "sha256",
                    "parentThreadId": "thread-1",
                    "args": {"issue": 42}
                }
            })
        );
    }

    #[test]
    fn workflow_read_preserves_exact_document_and_parsed_definition() {
        let response = WorkflowReadResponse {
            workflow: WorkflowSummary {
                workflow_id: "plugin:review".to_string(),
                identity: WorkflowSourceIdentity {
                    source: "plugin/review/workflows/review.yaml".to_string(),
                    name: "review".to_string(),
                    version: "1".to_string(),
                    digest: "abc".to_string(),
                },
                executable_digest: "def".to_string(),
                description: None,
                engine: WorkflowEngine::JavaScriptV1,
                scope: WorkflowScope::Plugin,
            },
            format: WorkflowDocumentFormat::Yaml,
            document: "# retained comment\napiVersion: zuno.workflow/v1\n".to_string(),
            definition: json!({"apiVersion": "zuno.workflow/v1"}),
        };

        let value = serde_json::to_value(&response).expect("workflow/read should encode");
        assert_eq!(value["workflow"]["engine"], json!("javascript/v1"));
        assert_eq!(value["workflow"]["scope"], json!("plugin"));
        assert_eq!(value["format"], json!("yaml"));
        assert_eq!(
            value["document"],
            json!("# retained comment\napiVersion: zuno.workflow/v1\n")
        );
        assert_eq!(
            serde_json::from_value::<WorkflowReadResponse>(value)
                .expect("workflow/read should decode"),
            response
        );
    }

    #[test]
    fn uncertain_run_status_is_explicit_on_the_wire() {
        assert_eq!(
            serde_json::to_value(WorkflowRunStatus::Uncertain)
                .expect("uncertain status should encode"),
            json!("uncertain")
        );
        assert_eq!(
            serde_json::to_value(WorkflowCallStatus::Uncertain)
                .expect("uncertain call status should encode"),
            json!("uncertain")
        );
    }
}
