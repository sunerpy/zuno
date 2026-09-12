//! Durable orchestration coordinates. A workflow's native Job owns child Jobs;
//! the control plane resolves the graph and models from immutable definitions.
//! Worker requests never carry futures, physical paths or a delegation grant.

use crate::{
    ApplicationError,
    child::{ChildDefinitionGrant, ChildDispatch, ChildInvocation},
    runtime::{ConfigurationRef, ExecutionLease},
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zuno_orchestration::WorkflowTemplateDescriptor;
use zuno_types::identity::{JobId, NodeRunId, WorkflowRunId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    Preparing,
    Prepared,
    Active,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowRunView {
    pub id: WorkflowRunId,
    pub job_id: JobId,
    pub state: WorkflowState,
    pub name: String,
    pub nodes: Vec<NodeRunView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeRunView {
    pub id: NodeRunId,
    pub node_id: String,
    pub job_id: JobId,
    pub state: zuno_types::activity::InvocationState,
    pub depends_on: Vec<String>,
    pub waits: Vec<crate::api::JobWaitView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowInvocation {
    pub template: String,
    pub root: ChildInvocation,
}
impl WorkflowInvocation {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.root.validate()?;
        if self.template.trim().is_empty()
            || self.template.len() > 256
            || self.template.chars().any(char::is_control)
            || self.root.resume_session_id.is_some()
        {
            return Err(ApplicationError::Invalid(
                "invalid workflow invocation".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A control-plane value, deliberately not deserializable from Worker input.
#[derive(Debug, Clone)]
pub struct WorkflowDefinitionGrant {
    pub group: ChildDefinitionGrant,
    pub template: WorkflowTemplateDescriptor,
    pub nodes: BTreeMap<String, ChildDefinitionGrant>,
}

pub trait WorkflowDefinitionCatalog: Send + Sync {
    fn resolve(&self, parent: &ConfigurationRef, template: &str)
    -> Option<WorkflowDefinitionGrant>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDispatch {
    pub run_id: WorkflowRunId,
    pub group: ChildDispatch,
    pub nodes: Vec<WorkflowNodeDispatch>,
    pub prepared: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowNodeDispatch {
    /// Stable template-local identity, not a physical row number.
    pub node_id: String,
    pub child: ChildDispatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowCommand {
    Dispatch { invocation: Box<WorkflowInvocation> },
    Prepare { job_id: JobId },
}

#[async_trait]
pub trait WorkflowStore: Send + Sync {
    async fn dispatch_workflow(
        &self,
        lease: &ExecutionLease,
        invocation: WorkflowInvocation,
        grant: &WorkflowDefinitionGrant,
    ) -> Result<WorkflowDispatch, ApplicationError>;
    /// Confirms materialized workspaces and advances only a fixed staged graph.
    /// Foreground activation still commits with the original parent's wait.
    async fn prepare_workflow(
        &self,
        lease: &ExecutionLease,
        job: &JobId,
    ) -> Result<WorkflowDispatch, ApplicationError>;
}
