use crate::CompiledGraph;
use crate::WorkflowError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Component;
use std::path::Path;

pub const WORKFLOW_API_VERSION: &str = "zuno.workflow/v1";
pub const DEFAULT_MAX_CONCURRENT_AGENTS: u32 = 4;
pub const DEFAULT_MAX_TOTAL_AGENTS: u32 = 12;
pub const DEFAULT_MAX_ITEMS_PER_CALL: u32 = 128;
pub const MAX_WORKFLOW_AGENTS: u32 = 1_000;
pub const MAX_WORKFLOW_ITEMS: u32 = 4_096;
pub const MAX_ID_CHARS: usize = 256;
pub const MAX_INLINE_SCRIPT_BYTES: usize = 256 * 1024;

/// User-authored workflow document encodings supported by v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowFormat {
    Json,
    Yaml,
}

impl fmt::Display for WorkflowFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
        })
    }
}

/// An installed graph engine or dynamic script engine.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum WorkflowEngine {
    #[serde(rename = "graph/v1")]
    GraphV1,
    #[serde(rename = "javascript/v1")]
    JavaScriptV1,
    #[serde(rename = "node-worker/v1")]
    NodeWorkerV1,
}

impl fmt::Display for WorkflowEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::GraphV1 => "graph/v1",
            Self::JavaScriptV1 => "javascript/v1",
            Self::NodeWorkerV1 => "node-worker/v1",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDefinition {
    pub api_version: String,
    pub kind: WorkflowKind,
    pub metadata: WorkflowMetadata,
    pub spec: WorkflowSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum WorkflowKind {
    Workflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowMetadata {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowSpec {
    pub engine: WorkflowEngine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, WorkflowRoute>,
    #[serde(default)]
    pub limits: WorkflowLimits,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<WorkflowNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowRoute {
    pub agent_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowNode {
    pub id: String,
    pub route: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowLimits {
    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: u32,
    #[serde(default = "default_max_total_agents")]
    pub max_total_agents: u32,
    #[serde(default = "default_max_items_per_call")]
    pub max_items_per_call: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_time_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_heap_size_bytes: Option<u64>,
}

impl Default for WorkflowLimits {
    fn default() -> Self {
        Self {
            max_concurrent_agents: DEFAULT_MAX_CONCURRENT_AGENTS,
            max_total_agents: DEFAULT_MAX_TOTAL_AGENTS,
            max_items_per_call: DEFAULT_MAX_ITEMS_PER_CALL,
            max_wall_time_ms: None,
            max_tokens: None,
            max_heap_size_bytes: None,
        }
    }
}

const fn default_max_concurrent_agents() -> u32 {
    DEFAULT_MAX_CONCURRENT_AGENTS
}
const fn default_max_total_agents() -> u32 {
    DEFAULT_MAX_TOTAL_AGENTS
}
const fn default_max_items_per_call() -> u32 {
    DEFAULT_MAX_ITEMS_PER_CALL
}

/// Immutable provenance for one exact workflow source document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowSourceIdentity {
    pub source: String,
    pub name: String,
    pub version: String,
    pub digest: String,
}

/// A fully validated workflow ready for engine compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedWorkflow {
    definition: WorkflowDefinition,
    identity: WorkflowSourceIdentity,
    graph: Option<CompiledGraph>,
}

impl ValidatedWorkflow {
    pub fn definition(&self) -> &WorkflowDefinition {
        &self.definition
    }
    pub fn identity(&self) -> &WorkflowSourceIdentity {
        &self.identity
    }
    pub fn graph(&self) -> Option<&CompiledGraph> {
        self.graph.as_ref()
    }
}

impl WorkflowDefinition {
    /// Parses, validates, and binds one exact source document to its provenance.
    pub fn parse(
        source: &str,
        format: WorkflowFormat,
        source_id: impl Into<String>,
    ) -> Result<ValidatedWorkflow, WorkflowError> {
        let definition: Self = match format {
            WorkflowFormat::Json => {
                serde_json::from_str(source).map_err(|error| WorkflowError::Parse {
                    format,
                    message: error.to_string(),
                })?
            }
            WorkflowFormat::Yaml => {
                serde_yaml::from_str(source).map_err(|error| WorkflowError::Parse {
                    format,
                    message: error.to_string(),
                })?
            }
        };
        definition.validate(source_id.into(), source)
    }

    fn validate(
        self,
        source_id: String,
        raw_source: &str,
    ) -> Result<ValidatedWorkflow, WorkflowError> {
        if self.api_version != WORKFLOW_API_VERSION {
            return Err(WorkflowError::UnsupportedApiVersion {
                actual: self.api_version,
            });
        }
        validate_identity("workflow source", &source_id)?;
        validate_identity("workflow name", &self.metadata.name)?;
        validate_identity("workflow version", &self.metadata.version)?;
        validate_optional_text("workflow description", self.metadata.description.as_deref())?;
        validate_limits(&self.spec.limits)?;
        for (name, route) in &self.spec.routes {
            validate_identity("workflow route", name)?;
            validate_identity("workflow agent reference", &route.agent_ref)?;
            if let Some(profile) = &route.execution_profile {
                validate_identity("workflow execution profile", profile)?;
            }
        }

        let graph = match self.spec.engine {
            WorkflowEngine::GraphV1 => {
                if self.spec.script.is_some() || self.spec.script_file.is_some() {
                    return Err(invalid_program(
                        self.spec.engine,
                        "graph workflows cannot contain scripts",
                    ));
                }
                Some(CompiledGraph::compile(
                    &self.spec.nodes,
                    &self.spec.routes,
                    &self.spec.limits,
                )?)
            }
            WorkflowEngine::JavaScriptV1 | WorkflowEngine::NodeWorkerV1 => {
                if !self.spec.nodes.is_empty() {
                    return Err(invalid_program(
                        self.spec.engine,
                        "script workflows cannot contain graph nodes",
                    ));
                }
                match (&self.spec.script, &self.spec.script_file) {
                    (Some(script), None)
                        if !script.trim().is_empty() && script.len() <= MAX_INLINE_SCRIPT_BYTES => {
                    }
                    (None, Some(path)) if safe_relative_path(path) => {}
                    (Some(_), Some(_)) => {
                        return Err(invalid_program(
                            self.spec.engine,
                            "set exactly one of script or scriptFile",
                        ));
                    }
                    (Some(_), None) => {
                        return Err(invalid_program(
                            self.spec.engine,
                            "inline script is blank or exceeds 256 KiB",
                        ));
                    }
                    (None, Some(path)) => {
                        return Err(WorkflowError::UnsafeScriptPath(path.clone()));
                    }
                    (None, None) => {
                        return Err(invalid_program(
                            self.spec.engine,
                            "script or scriptFile is required",
                        ));
                    }
                }
                None
            }
        };
        let identity = WorkflowSourceIdentity {
            source: source_id,
            name: self.metadata.name.clone(),
            version: self.metadata.version.clone(),
            digest: sha256_hex(raw_source.as_bytes()),
        };
        Ok(ValidatedWorkflow {
            definition: self,
            identity,
            graph,
        })
    }
}

fn validate_limits(limits: &WorkflowLimits) -> Result<(), WorkflowError> {
    validate_limit(
        "maxConcurrentAgents",
        limits.max_concurrent_agents,
        MAX_WORKFLOW_AGENTS,
    )?;
    validate_limit(
        "maxTotalAgents",
        limits.max_total_agents,
        MAX_WORKFLOW_AGENTS,
    )?;
    validate_limit(
        "maxItemsPerCall",
        limits.max_items_per_call,
        MAX_WORKFLOW_ITEMS,
    )?;
    if limits.max_concurrent_agents > limits.max_total_agents {
        return Err(WorkflowError::ParallelismExceedsTotal {
            concurrent: limits.max_concurrent_agents,
            total: limits.max_total_agents,
        });
    }
    if limits.max_wall_time_ms == Some(0) {
        return Err(WorkflowError::InvalidLimit {
            field: "maxWallTimeMs",
            value: 0,
            maximum: u32::MAX,
        });
    }
    if limits.max_tokens == Some(0) {
        return Err(WorkflowError::InvalidLimit {
            field: "maxTokens",
            value: 0,
            maximum: u32::MAX,
        });
    }
    Ok(())
}

fn validate_limit(field: &'static str, value: u32, maximum: u32) -> Result<(), WorkflowError> {
    if !(1..=maximum).contains(&value) {
        return Err(WorkflowError::InvalidLimit {
            field,
            value,
            maximum,
        });
    }
    Ok(())
}

pub(crate) fn validate_identity(kind: &'static str, value: &str) -> Result<(), WorkflowError> {
    if value.trim().is_empty()
        || value.chars().count() > MAX_ID_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(WorkflowError::InvalidIdentity {
            kind,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_optional_text(kind: &'static str, value: Option<&str>) -> Result<(), WorkflowError> {
    if value.is_some_and(|value| value.trim().is_empty()) {
        return Err(WorkflowError::EmptyText { kind });
    }
    Ok(())
}

fn safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.trim().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn invalid_program(engine: WorkflowEngine, message: impl Into<String>) -> WorkflowError {
    WorkflowError::InvalidProgram {
        engine,
        message: message.into(),
    }
}

pub(crate) fn sha256_hex(value: &[u8]) -> String {
    Sha256::digest(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
