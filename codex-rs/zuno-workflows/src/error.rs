use crate::WorkflowEngine;
use crate::WorkflowFormat;
use thiserror::Error;

/// A workflow definition, engine, or durable-replay contract failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkflowError {
    #[error("failed to parse {format} workflow: {message}")]
    Parse {
        format: WorkflowFormat,
        message: String,
    },
    #[error("unsupported workflow apiVersion `{actual}`; expected `zuno.workflow/v1`")]
    UnsupportedApiVersion { actual: String },
    #[error("invalid {kind} `{value}`")]
    InvalidIdentity { kind: &'static str, value: String },
    #[error("{kind} must not be blank when present")]
    EmptyText { kind: &'static str },
    #[error("{field} must be between 1 and {maximum}, got {value}")]
    InvalidLimit {
        field: &'static str,
        value: u32,
        maximum: u32,
    },
    #[error("maxConcurrentAgents {concurrent} cannot exceed maxTotalAgents {total}")]
    ParallelismExceedsTotal { concurrent: u32, total: u32 },
    #[error("workflow engine `{engine}` has an invalid program: {message}")]
    InvalidProgram {
        engine: WorkflowEngine,
        message: String,
    },
    #[error("workflow contains duplicate node id `{0}`")]
    DuplicateNode(String),
    #[error("workflow node `{node}` references unknown route `{route}`")]
    UnknownRoute { node: String, route: String },
    #[error("workflow node `{0}` cannot depend on itself")]
    SelfDependency(String),
    #[error("workflow node `{node}` repeats dependency `{dependency}`")]
    DuplicateDependency { node: String, dependency: String },
    #[error("workflow node `{node}` references missing dependency `{dependency}`")]
    MissingDependency { node: String, dependency: String },
    #[error("workflow dependencies contain a cycle")]
    DependencyCycle,
    #[error("workflow script path `{0}` must be a safe relative path")]
    UnsafeScriptPath(String),
    #[error("workflow call `{id}` changed request digest from `{recorded}` to `{actual}`")]
    ReplayDiverged {
        id: String,
        recorded: String,
        actual: String,
    },
    #[error("workflow engine `{engine}` failed: {message}")]
    Engine {
        engine: WorkflowEngine,
        message: String,
    },
}
