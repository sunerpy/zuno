//! Council admission uses native Workflow/child Jobs with fixed seat, repair and
//! synthesis definitions. These are configuration grants, not caller authority.
use crate::{
    ApplicationError,
    child::{ChildDefinitionGrant, ChildInvocation},
    runtime::{ConfigurationRef, ExecutionLease},
    workflow::WorkflowDispatch,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zuno_orchestration::CouncilPresetDescriptor;
use zuno_types::activity::Counter;
use zuno_types::identity::JobId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CouncilPhase {
    Seats,
    Stopping,
    Synthesis,
    Completed,
    Failed,
    Cancelled,
    Uncertain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CouncilSeatState {
    Pending,
    Running,
    Waiting,
    Retrying,
    Completed,
    Invalid,
    Failed,
    TimedOut,
    Cancelled,
    Uncertain,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilSeatView {
    pub id: String,
    pub job_id: JobId,
    pub state: CouncilSeatState,
    pub attempts: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilView {
    pub preset: String,
    pub quorum: u32,
    pub phase: CouncilPhase,
    pub seat_deadline: Counter,
    pub deadline: Counter,
    pub synthesis_deadline: Option<Counter>,
    pub seats: Vec<CouncilSeatView>,
}

#[derive(Debug, Clone)]
pub struct CouncilRules {
    pub preset: CouncilPresetDescriptor,
    pub repairs: BTreeMap<String, ChildDefinitionGrant>,
}

#[derive(Debug, Clone)]
pub struct CouncilDefinitionGrant {
    pub group: ChildDefinitionGrant,
    pub rules: CouncilRules,
    pub seats: BTreeMap<String, ChildDefinitionGrant>,
    pub synthesis: ChildDefinitionGrant,
}
pub trait CouncilDefinitionCatalog: Send + Sync {
    fn resolve(&self, parent: &ConfigurationRef, preset: &str) -> Option<CouncilDefinitionGrant>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CouncilInvocation {
    pub preset: String,
    pub root: ChildInvocation,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CouncilCommand {
    Dispatch { invocation: Box<CouncilInvocation> },
    Prepare { job_id: JobId },
}
#[async_trait]
pub trait CouncilStore: Send + Sync {
    async fn dispatch_council(
        &self,
        lease: &ExecutionLease,
        invocation: CouncilInvocation,
        grant: &CouncilDefinitionGrant,
    ) -> Result<WorkflowDispatch, ApplicationError>;
}
