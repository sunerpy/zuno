use super::*;
use zuno_application::workspace_edit::*;
use zuno_types::identity::GatewayId;

impl DockerGateway {
    pub(crate) fn scan_workspace_edits(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::ledger::EditRecord>, ApplicationError> {
        self.ledger.scan_edits(limit)
    }
    pub(crate) fn defer_workspace_edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<(), ApplicationError> {
        self.ledger.defer_edit(owner, id)
    }
    pub(crate) fn workspace_edit_completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<WorkspaceEditCompletion>, ApplicationError> {
        self.ledger.edit_completion(owner, id)
    }
    pub(crate) fn acknowledge_workspace_edit(
        &self,
        completion: &WorkspaceEditCompletion,
    ) -> Result<(), ApplicationError> {
        self.ledger.acknowledge_edit(completion)
    }
    pub async fn preview_workspace_edit(
        &self,
        gateway_id: GatewayId,
        lease: &ExecutionLease,
        operation: WorkspaceEditOperation,
    ) -> Result<WorkspaceEditAdmission, ApplicationError> {
        operation.validate()?;
        match self.ledger.edit(&lease.owner, &operation.id) {
            Ok(record) => {
                if record.admission.operation != operation
                    || record.admission.lease.job_id != lease.job_id
                    || record.admission.lease.session_id != lease.session_id
                {
                    return Err(ApplicationError::Conflict);
                }
                let mut admission = record.admission;
                admission.lease = lease.clone();
                return Ok(admission);
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let environment = self.get(&lease.owner, &operation.environment_id).await?;
        if environment.spec.session_id != lease.session_id
            || environment.revision != operation.expected_revision
        {
            return Err(ApplicationError::Conflict);
        }
        let snapshot_id = EnvironmentSnapshotId::new(format!(
            "edit_{}",
            zuno_orchestration::sha256_json(&json!([lease.owner, operation.id]))
        ))
        .map_err(crate::storage)?;
        let base = self
            .snapshot_named(
                &lease.owner,
                &operation.environment_id,
                operation.expected_revision,
                &snapshot_id,
            )
            .await?;
        let source = self.snapshot_path(&lease.owner, &base.id);
        let archive = self.edit_archive(&lease.owner, &operation.id);
        let pending = archive.with_extension(format!("{}.pending", uuid::Uuid::new_v4().simple()));
        let output = pending.clone();
        let expected = base.clone();
        let edits = operation.edits.clone();
        let result = tokio::task::spawn_blocking(move || {
            let source = crate::workspace_merge::SnapshotTree::read(
                &source,
                &expected.sha256,
                expected.bytes,
            )?;
            crate::workspace_merge::write_edited_archive(&source, &edits, &output)
        })
        .await
        .map_err(crate::storage)?;
        let (_, _, review) = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = tokio::fs::remove_file(&pending).await;
                return Err(error);
            }
        };
        tokio::fs::rename(&pending, &archive)
            .await
            .map_err(crate::storage)?;
        let admission = WorkspaceEditAdmission {
            gateway_id,
            lease: lease.clone(),
            environment,
            operation,
            base,
            review,
        };
        admission.validate()?;
        Ok(admission)
    }
    fn edit_archive(&self, owner: &PrincipalKey, id: &OperationId) -> PathBuf {
        self.snapshots.join(format!(
            "edit-{}.tar",
            zuno_orchestration::sha256_json(&json!([owner, id]))
        ))
    }
    pub async fn admit_workspace_edit(
        &self,
        admission: &WorkspaceEditAdmission,
        authority: &dyn WorkspaceEditAuthority,
    ) -> Result<WorkspaceEditReceipt, ApplicationError> {
        admission.validate()?;
        let owner = &admission.lease.owner;
        let operation = &admission.operation;
        let control = self.control(owner, &operation.environment_id)?;
        let _guard = control.lock().await;
        authority.authorize_edit(admission).await?;
        let record = self.ledger.begin_edit(admission)?;
        Ok(record.receipt.unwrap_or(WorkspaceEditReceipt {
            id: operation.id.clone(),
            environment_id: operation.environment_id.clone(),
            state: record.state,
            request_digest: operation.digest(),
            revision: operation.expected_revision,
        }))
    }
    pub(crate) async fn apply_workspace_edit(
        &self,
        admission: &WorkspaceEditAdmission,
    ) -> Result<WorkspaceEditReceipt, ApplicationError> {
        admission.validate()?;
        let owner = &admission.lease.owner;
        let operation = &admission.operation;
        let control = self.control(owner, &operation.environment_id)?;
        let _guard = control.lock().await;
        let record = self.ledger.edit(owner, &operation.id)?;
        if record.admission.operation != *operation
            || record.admission.lease.job_id != admission.lease.job_id
        {
            return Err(ApplicationError::Conflict);
        }
        if let Some(receipt) = record.receipt {
            return Ok(receipt);
        }
        if record.state != WorkspaceEditState::Preparing {
            return Err(ApplicationError::Conflict);
        }
        let environment = self.ledger.environment(owner, &operation.environment_id)?;
        if environment != admission.environment {
            return Err(ApplicationError::Conflict);
        }
        if self.ledger.snapshot(owner, &admission.base.id)? != admission.base {
            return Err(ApplicationError::Conflict);
        }
        let source = self.snapshot_path(owner, &admission.base.id);
        let archive = self.edit_archive(owner, &operation.id);
        let pending = archive.with_extension(format!("{}.pending", uuid::Uuid::new_v4().simple()));
        let output = pending.clone();
        let expected = admission.base.clone();
        let edits = operation.edits.clone();
        let (sha, bytes, review) = tokio::task::spawn_blocking(move || {
            let tree = crate::workspace_merge::SnapshotTree::read(
                &source,
                &expected.sha256,
                expected.bytes,
            )?;
            crate::workspace_merge::write_edited_archive(&tree, &edits, &output)
        })
        .await
        .map_err(crate::storage)??;
        if review != admission.review {
            return Err(ApplicationError::Conflict);
        }
        tokio::fs::rename(&pending, &archive)
            .await
            .map_err(crate::storage)?;
        let mut labels = self.storage_labels(owner, &environment.spec)?;
        // A fresh candidate carries only its own publication label.
        labels
            .as_object_mut()
            .ok_or(ApplicationError::Conflict)?
            .remove("zuno.merge");
        labels
            .as_object_mut()
            .ok_or(ApplicationError::Conflict)?
            .remove("zuno.merge.nonce");
        labels["zuno.edit"] = json!(operation.id);
        labels["zuno.edit.nonce"] = json!(record.nonce);
        let name = format!(
            "zuno-edit-restore-{}",
            zuno_orchestration::sha256_json(&json!([owner, operation.id]))
        );
        self.restore_candidate(
            &environment,
            &record.volume,
            &name,
            &labels,
            super::publication::CandidateArchive {
                path: &archive,
                sha256: &sha,
                bytes,
            },
        )
        .await?;
        self.ledger.publish_edit(owner, &operation.id)
    }
    pub fn inspect_workspace_edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<WorkspaceEditReceipt, ApplicationError> {
        let record = self.ledger.edit(owner, id)?;
        Ok(record.receipt.unwrap_or(WorkspaceEditReceipt {
            id: id.clone(),
            environment_id: record.admission.operation.environment_id.clone(),
            state: record.state,
            request_digest: record.admission.operation.digest(),
            revision: record.admission.operation.expected_revision,
        }))
    }
    pub fn cancel_workspace_edit(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<(), ApplicationError> {
        self.ledger.cancel_edit(owner, id)?;
        Ok(())
    }
    pub fn cancel_admitted_workspace_edit(
        &self,
        admission: &WorkspaceEditAdmission,
    ) -> Result<(), ApplicationError> {
        self.ledger.cancel_admitted_edit(admission)?;
        Ok(())
    }
}
