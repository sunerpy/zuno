use crate::ValidatedWorkflow;
use crate::WorkflowEngine;
use crate::WorkflowError;
use crate::sha256_hex;
use crate::validate_identity;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type WorkflowFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Capabilities and deployment facts advertised before a workflow is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowEngineDescriptor {
    pub engine: WorkflowEngine,
    pub dynamic_scripts: bool,
    pub isolated_process: bool,
    pub durable_replay: bool,
}

/// Engine-neutral compilation input after loading an optional script file.
#[derive(Debug, Clone)]
pub struct WorkflowCompileRequest {
    pub workflow: Arc<ValidatedWorkflow>,
    pub resolved_script: Option<Arc<str>>,
}

/// Opaque engine artifact bound to one exact validated workflow.
#[derive(Debug, Clone)]
pub struct CompiledWorkflow {
    pub workflow: Arc<ValidatedWorkflow>,
    pub engine_revision: String,
    pub artifact: Arc<[u8]>,
}

/// Engine-neutral start input. The host has already frozen policy and routes.
#[derive(Debug, Clone)]
pub struct WorkflowStartRequest {
    pub compiled: Arc<CompiledWorkflow>,
    pub run_id: WorkflowRunId,
    pub parent_thread_id: String,
    pub args: JsonValue,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkflowRunId(String);

impl WorkflowRunId {
    pub fn new(value: impl Into<String>) -> Result<Self, WorkflowError> {
        let value = value.into();
        validate_identity("workflow run id", &value)?;
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable identity and canonical request digest for one replayable host call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowCallIdentity {
    pub id: String,
    pub request_digest: String,
}

impl WorkflowCallIdentity {
    pub fn new(id: impl Into<String>, request: &JsonValue) -> Result<Self, WorkflowError> {
        let id = id.into();
        validate_identity("workflow call id", &id)?;
        let canonical = canonical_json(request);
        let encoded = serde_json::to_vec(&canonical).map_err(|error| WorkflowError::Engine {
            engine: WorkflowEngine::JavaScriptV1,
            message: format!("failed to canonicalize workflow call: {error}"),
        })?;
        Ok(Self {
            id,
            request_digest: sha256_hex(&encoded),
        })
    }

    pub fn verify_replay(&self, request: &JsonValue) -> Result<(), WorkflowError> {
        let actual = Self::new(self.id.clone(), request)?.request_digest;
        if actual == self.request_digest {
            Ok(())
        } else {
            Err(WorkflowError::ReplayDiverged {
                id: self.id.clone(),
                recorded: self.request_digest.clone(),
                actual,
            })
        }
    }
}

fn canonical_json(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(values) => JsonValue::Array(values.iter().map(canonical_json).collect()),
        JsonValue::Object(values) => {
            let sorted = values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            JsonValue::Object(sorted.into_iter().collect())
        }
        value => value.clone(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowStopReason {
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowResult {
    pub value: JsonValue,
    pub stop_reason: WorkflowStopReason,
    pub error: Option<String>,
    pub agents_started: u32,
}

/// One accepted workflow run. Implementations must make cancellation and disposal idempotent.
pub trait WorkflowRun: Send + Sync {
    fn id(&self) -> &WorkflowRunId;
    fn cancel(&self, reason: String);
    fn result<'a>(&'a self) -> WorkflowFuture<'a, WorkflowResult>;
    fn dispose<'a>(&'a self) -> WorkflowFuture<'a, Result<(), WorkflowError>>;
}

/// Replaceable graph, V8, or optional Node workflow engine.
pub trait WorkflowEngineProvider: Send + Sync {
    fn descriptor(&self) -> WorkflowEngineDescriptor;
    fn compile<'a>(
        &'a self,
        request: WorkflowCompileRequest,
    ) -> WorkflowFuture<'a, Result<CompiledWorkflow, WorkflowError>>;
    fn start<'a>(
        &'a self,
        request: WorkflowStartRequest,
    ) -> WorkflowFuture<'a, Result<Arc<dyn WorkflowRun>, WorkflowError>>;
}
