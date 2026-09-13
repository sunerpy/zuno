//! Tenant policy applied independently to each authenticated owner. Quota
//! admission never grants authority or discards accepted recovery state.
use crate::ApplicationError;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zuno_types::{
    activity::Counter,
    identity::{PrincipalScope, RequestId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaResource {
    RootSessions,
    RootJobs,
    ChildJobs,
    Executions,
    LearningJobs,
    LearningExecutions,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaLimits {
    #[schemars(range(min = 1, max = 1_000_000))]
    pub root_sessions: u32,
    #[schemars(range(min = 1, max = 100_000))]
    pub root_jobs: u32,
    #[schemars(range(min = 1, max = 100_000))]
    pub child_jobs: u32,
    #[schemars(range(min = 1, max = 1024))]
    pub executions: u32,
    #[schemars(range(min = 1, max = 100_000))]
    pub learning_jobs: u32,
    #[schemars(range(min = 1, max = 128))]
    pub learning_executions: u32,
}
impl Default for QuotaLimits {
    fn default() -> Self {
        Self {
            root_sessions: 2048,
            root_jobs: 64,
            child_jobs: 1024,
            executions: 8,
            learning_jobs: 64,
            learning_executions: 2,
        }
    }
}
impl QuotaLimits {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        if !(1..=1_000_000).contains(&self.root_sessions)
            || !(1..=100_000).contains(&self.root_jobs)
            || !(1..=100_000).contains(&self.child_jobs)
            || !(1..=1024).contains(&self.executions)
            || !(1..=100_000).contains(&self.learning_jobs)
            || !(1..=128).contains(&self.learning_executions)
        {
            return Err(ApplicationError::Invalid(
                "invalid enterprise quota limits".to_owned(),
            ));
        }
        Ok(())
    }
    pub fn limit(&self, resource: QuotaResource) -> u32 {
        match resource {
            QuotaResource::RootSessions => self.root_sessions,
            QuotaResource::RootJobs => self.root_jobs,
            QuotaResource::ChildJobs => self.child_jobs,
            QuotaResource::Executions => self.executions,
            QuotaResource::LearningJobs => self.learning_jobs,
            QuotaResource::LearningExecutions => self.learning_executions,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaPolicy {
    pub revision: Counter,
    pub limits: QuotaLimits,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplaceQuotaPolicy {
    pub request_id: RequestId,
    pub expected_revision: Counter,
    pub limits: QuotaLimits,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaUsage {
    pub resource: QuotaResource,
    pub used: Counter,
    pub limit: Counter,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaSnapshot {
    pub policy: QuotaPolicy,
    pub usage: Vec<QuotaUsage>,
}
#[async_trait]
pub trait QuotaStore: Send + Sync {
    async fn snapshot(&self, principal: &PrincipalScope)
    -> Result<QuotaSnapshot, ApplicationError>;
    async fn replace(
        &self,
        principal: &PrincipalScope,
        request: ReplaceQuotaPolicy,
    ) -> Result<QuotaPolicy, ApplicationError>;
}
