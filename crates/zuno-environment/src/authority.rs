//! Adapter to the existing organization approval state owner.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zuno_application::ApplicationError;
use zuno_application::authorization::{ApprovalBinding, ApprovalProposal, OrganizationStore};
use zuno_application::environment::{CommandOperation, Environment, OperationAuthority};
use zuno_application::runtime::{ExecutionLease, RuntimeStore};
use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};

pub struct OrganizationOperationAuthority {
    runtime: Arc<dyn RuntimeStore>,
    organizations: Arc<dyn OrganizationStore>,
}
impl OrganizationOperationAuthority {
    /// Both ports come from the same state-owner backend bundle. A separated
    /// gateway supplies authenticated remote adapters rather than a DB credential.
    pub fn new(runtime: Arc<dyn RuntimeStore>, organizations: Arc<dyn OrganizationStore>) -> Self {
        Self {
            runtime,
            organizations,
        }
    }

    pub async fn proposal(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &CommandOperation,
    ) -> Result<ApprovalProposal, ApplicationError> {
        operation.validate()?;
        environment.spec.validate()?;
        if lease.owner != environment.owner
            || lease.session_id != environment.spec.session_id
            || operation.environment_id != environment.spec.id
            || operation.expected_revision != environment.revision
        {
            return Err(ApplicationError::Conflict);
        }
        let job = self.runtime.get(&lease.owner, &lease.job_id).await?;
        if job.session_id != lease.session_id {
            return Err(ApplicationError::Conflict);
        }
        Ok(ApprovalProposal {
            binding: ApprovalBinding {
                job_id: job.id,
                session_id: job.session_id,
                turn_id: job.turn_id,
                invocation_id: operation.invocation_id.clone(),
                operation_id: operation.id.clone(),
                arguments_sha256: zuno_orchestration::sha256_json(&json!(operation.argv)),
                resources_sha256: zuno_orchestration::sha256_json(&json!([
                    environment.owner,
                    environment.spec,
                    environment.revision
                ])),
                effect: EffectKind::Process,
            },
            facts: PreparedEffectFacts {
                kind: EffectKind::Process,
                resource_authorized: true,
                isolation: IsolationFact::Enforced,
                builtin_handler: true,
                sensitive: false,
                explicit_deny: false,
                mandatory_human: true,
            },
            presentation: json!({"operationID":operation.id,"environmentID":environment.spec.id,"argv":operation.argv}),
        })
    }
}

#[async_trait]
impl OperationAuthority for OrganizationOperationAuthority {
    async fn authorize(
        &self,
        lease: &ExecutionLease,
        environment: &Environment,
        operation: &CommandOperation,
    ) -> Result<(), ApplicationError> {
        let proposal = self.proposal(lease, environment, operation).await?;
        self.organizations.check_execution(lease, proposal).await?;
        Ok(())
    }
}
