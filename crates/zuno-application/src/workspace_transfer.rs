//! Private, source-authenticated workspace snapshot transport. Requests identify
//! an existing runtime operation; they cannot select a path, endpoint or owner.
use crate::{
    ApplicationError,
    environment::{EnvironmentSnapshot, wire::GatewayAssignment},
    runtime::ExecutionLease,
};
use serde::{Deserialize, Serialize};
use zuno_types::identity::{EnvironmentSnapshotId, GatewayId, JobId, OperationId};

pub const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SnapshotTransferPurpose {
    ChildWorkspace {
        child_job_id: JobId,
    },
    MergeSource {
        operation_id: OperationId,
        child_job_id: JobId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotTransferRequest {
    pub lease: ExecutionLease,
    pub purpose: SnapshotTransferPurpose,
}

impl SnapshotTransferRequest {
    pub fn snapshot_id(&self) -> Result<EnvironmentSnapshotId, ApplicationError> {
        match &self.purpose {
            SnapshotTransferPurpose::ChildWorkspace { child_job_id } => {
                EnvironmentSnapshotId::new(format!("child-{child_job_id}"))
            }
            SnapshotTransferPurpose::MergeSource { operation_id, .. } => {
                EnvironmentSnapshotId::new(format!(
                    "merge-{}",
                    zuno_orchestration::sha256_json(&serde_json::json!([
                        self.lease.owner,
                        operation_id,
                        "child"
                    ]))
                ))
            }
        }
        .map_err(ApplicationError::storage)
    }
}

/// Constructed by the data owner from immutable configurations and child
/// lineage. It is internal state, never a public upload capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotTransferAssignment {
    pub request: SnapshotTransferRequest,
    pub source: GatewayAssignment,
    pub target_gateway_id: GatewayId,
    pub existing_source: bool,
}

impl SnapshotTransferAssignment {
    /// A new Worker lease can finish the same transfer, but cannot replace its
    /// source, target, owner, logical operation or immutable configuration.
    pub fn digest(&self) -> String {
        zuno_orchestration::sha256_json(&serde_json::json!([
            self.request.lease.owner,
            self.request.lease.job_id,
            self.request.lease.session_id,
            self.request.purpose,
            self.source,
            self.target_gateway_id,
            self.existing_source
        ]))
    }

    pub fn validate_snapshot(
        &self,
        snapshot: &EnvironmentSnapshot,
    ) -> Result<(), ApplicationError> {
        self.source.environment.validate()?;
        if snapshot.id != self.request.snapshot_id()?
            || snapshot.environment_id != self.source.environment.id
            || snapshot.revision == 0
            || snapshot.bytes == 0
            || snapshot.bytes > MAX_SNAPSHOT_BYTES
            || snapshot.sha256.len() != 64
            || !snapshot
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }
}

/// Only the assigned source gateway may commit this immutable fact. The target
/// verifies the fact through the control plane before publishing received bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotTransferCompletion {
    pub assignment: SnapshotTransferAssignment,
    pub snapshot: EnvironmentSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotTransferContext {
    pub assignment: SnapshotTransferAssignment,
    pub snapshot: Option<EnvironmentSnapshot>,
}
