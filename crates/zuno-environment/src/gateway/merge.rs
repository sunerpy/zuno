//! Only a complete candidate volume can become the active workspace. Approval
//! and lineage are supplied by the authenticated control-plane adapter.
use super::*;
use zuno_application::workspace_merge::{
    WorkspaceMergeAuthority, WorkspaceMergeOperation, WorkspaceMergeReceipt, WorkspaceMergeState,
};

impl DockerGateway {
    pub async fn read_workspace_merge_content(
        &self,
        context: zuno_application::workspace_merge::MergeContentContext,
    ) -> Result<crate::workspace_merge::WorkspaceContent, ApplicationError> {
        if self.ledger.snapshot(&context.owner, &context.snapshot.id)? != context.snapshot {
            return Err(ApplicationError::Conflict);
        }
        let path = self.snapshot_path(&context.owner, &context.snapshot.id);
        tokio::task::spawn_blocking(move || {
            let tree = crate::workspace_merge::SnapshotTree::read(
                &path,
                &context.snapshot.sha256,
                context.snapshot.bytes,
            )?;
            tree.content(&context.path, &context.expected)
        })
        .await
        .map_err(crate::storage)?
    }
    pub fn workspace_merge_status(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<WorkspaceMergeReceipt, ApplicationError> {
        let record = self.ledger.merge(owner, id)?;
        Ok(record.receipt.unwrap_or(WorkspaceMergeReceipt {
            id: id.clone(),
            environment_id: record.request.environment_id.clone(),
            state: record.state,
            plan_digest: record.request.plan.digest(),
            revision: record.request.expected_revision,
        }))
    }
    pub(crate) fn scan_workspace_merges(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::ledger::MergeRecord>, ApplicationError> {
        self.ledger.scan_merges(limit)
    }
    pub(crate) fn defer_workspace_merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<(), ApplicationError> {
        self.ledger.defer_merge(owner, id)
    }
    pub(crate) fn workspace_merge_completion(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<zuno_application::workspace_merge::WorkspaceMergeCompletion>, ApplicationError>
    {
        self.ledger.merge_completion(owner, id)
    }
    pub(crate) fn acknowledge_workspace_merge(
        &self,
        completion: &zuno_application::workspace_merge::WorkspaceMergeCompletion,
    ) -> Result<(), ApplicationError> {
        self.ledger.acknowledge_merge(completion)
    }

    /// The existing immutable offer has been bound to current approval by the
    /// authority adapter. Only the gateway reserves execution capacity.
    pub async fn admit_workspace_merge(
        &self,
        lease: &ExecutionLease,
        operation: &WorkspaceMergeOperation,
        authority: &dyn WorkspaceMergeAuthority,
    ) -> Result<WorkspaceMergeReceipt, ApplicationError> {
        operation.validate()?;
        let control = self.control(&lease.owner, &operation.environment_id)?;
        let _guard = control.lock().await;
        let environment = self
            .ledger
            .environment(&lease.owner, &operation.environment_id)?;
        if environment.spec.session_id != lease.session_id {
            return Err(ApplicationError::Forbidden);
        }
        match self.ledger.merge(&lease.owner, &operation.id) {
            Ok(record) => {
                if record.request != *operation
                    || record.lease.job_id != lease.job_id
                    || record.lease.session_id != lease.session_id
                {
                    return Err(ApplicationError::Conflict);
                }
                if let Some(receipt) = record.receipt {
                    return Ok(receipt);
                }
                return Ok(WorkspaceMergeReceipt {
                    id: operation.id.clone(),
                    environment_id: operation.environment_id.clone(),
                    state: record.state,
                    plan_digest: operation.plan.digest(),
                    revision: operation.expected_revision,
                });
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        if environment.revision != operation.expected_revision
            || operation.plan.changes.iter().any(|change| {
                change.choice == zuno_application::workspace_merge::MergeChoice::Conflict
            })
        {
            return Err(ApplicationError::Conflict);
        }
        for snapshot in [&operation.base, &operation.parent, &operation.child] {
            if self.ledger.snapshot(&lease.owner, &snapshot.id)? != *snapshot {
                return Err(ApplicationError::Conflict);
            }
        }
        authority
            .authorize_merge(lease, &environment, operation)
            .await?;
        let record = self.ledger.begin_merge(lease, operation)?;
        Ok(record.receipt.unwrap_or(WorkspaceMergeReceipt {
            id: operation.id.clone(),
            environment_id: operation.environment_id.clone(),
            state: record.state,
            plan_digest: operation.plan.digest(),
            revision: operation.expected_revision,
        }))
    }

    pub fn cancel_admitted_workspace_merge(
        &self,
        admission: &zuno_application::workspace_merge::WorkspaceMergeAdmission,
    ) -> Result<(), ApplicationError> {
        if admission.environment.owner != admission.lease.owner
            || admission.environment.spec.session_id != admission.lease.session_id
            || admission.environment.spec.id != admission.operation.environment_id
        {
            return Err(ApplicationError::Forbidden);
        }
        self.ledger
            .cancel_admitted_merge(&admission.lease, &admission.operation)?;
        Ok(())
    }

    /// Snapshot IDs are stable for this logical operation. A source that later
    /// changes cannot silently replace the originally offered source bytes.
    pub async fn preview_workspace_merge(
        &self,
        lease: &ExecutionLease,
        request: zuno_application::workspace_merge::WorkspaceMergePreviewRequest,
    ) -> Result<WorkspaceMergeOperation, ApplicationError> {
        let zuno_application::workspace_merge::WorkspaceMergePreviewRequest {
            id,
            invocation_id,
            child_job_id,
            environment_id,
            source_id,
            source_snapshot,
            base,
        } = request;
        let (environment_id, source_id, base) = (&environment_id, &source_id, &base);
        let owner = &lease.owner;
        if environment_id == source_id || self.ledger.snapshot(owner, &base.id)? != *base {
            return Err(ApplicationError::Conflict);
        }
        let parent = self.get(owner, environment_id).await?;
        if parent.spec.session_id != lease.session_id {
            return Err(ApplicationError::Forbidden);
        }
        let snapshot_id = |role: &str| {
            EnvironmentSnapshotId::new(format!(
                "merge-{}",
                zuno_orchestration::sha256_json(&json!([owner, id, role]))
            ))
            .map_err(crate::storage)
        };
        let parent_snapshot = self
            .snapshot_named(
                owner,
                environment_id,
                parent.revision,
                &snapshot_id("parent")?,
            )
            .await?;
        let source_snapshot = if let Some(snapshot) = source_snapshot {
            if snapshot.id != snapshot_id("child")?
                || snapshot.environment_id != *source_id
                || self.ledger.snapshot(owner, &snapshot.id)? != snapshot
            {
                return Err(ApplicationError::Conflict);
            }
            snapshot
        } else {
            let source = self.get(owner, source_id).await?;
            self.snapshot_named(owner, source_id, source.revision, &snapshot_id("child")?)
                .await?
        };
        let paths = [
            self.snapshot_path(owner, &base.id),
            self.snapshot_path(owner, &parent_snapshot.id),
            self.snapshot_path(owner, &source_snapshot.id),
        ];
        let snapshots = [
            base.clone(),
            parent_snapshot.clone(),
            source_snapshot.clone(),
        ];
        let plan = tokio::task::spawn_blocking(move || {
            let mut trees = Vec::new();
            for (path, snapshot) in paths.iter().zip(&snapshots) {
                trees.push(crate::workspace_merge::SnapshotTree::read(
                    path,
                    &snapshot.sha256,
                    snapshot.bytes,
                )?);
            }
            zuno_application::workspace_merge::plan(
                trees[0].entries(),
                trees[1].entries(),
                trees[2].entries(),
            )
        })
        .await
        .map_err(crate::storage)??;
        let operation = WorkspaceMergeOperation {
            id,
            invocation_id,
            child_job_id,
            environment_id: environment_id.clone(),
            expected_revision: parent.revision,
            base: base.clone(),
            parent: parent_snapshot,
            child: source_snapshot,
            plan,
        };
        operation.validate()?;
        Ok(operation)
    }

    fn merge_labels(
        &self,
        owner: &PrincipalKey,
        spec: &EnvironmentSpec,
        record: &crate::ledger::MergeRecord,
    ) -> Result<Value, ApplicationError> {
        let mut labels = self.storage_labels(owner, spec)?;
        labels["zuno.merge"] = json!(record.request.id);
        labels["zuno.merge.nonce"] = json!(record.nonce);
        Ok(labels)
    }
    async fn remove_merge_helper(
        &self,
        name: &str,
        labels: &Value,
    ) -> Result<(), ApplicationError> {
        let info = match self
            .docker
            .json(Method::GET, &format!("/containers/{name}/json"), None)
            .await
        {
            Ok(info) => info,
            Err(ApplicationError::NotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        if info.pointer("/Config/Labels") != Some(labels)
            || info.pointer("/State/Status").and_then(Value::as_str) != Some("created")
        {
            return Err(ApplicationError::Conflict);
        }
        self.docker
            .json(Method::DELETE, &format!("/containers/{name}"), None)
            .await?;
        Ok(())
    }

    pub async fn merge_workspace(
        &self,
        lease: &ExecutionLease,
        operation: &WorkspaceMergeOperation,
        authority: &dyn WorkspaceMergeAuthority,
    ) -> Result<WorkspaceMergeReceipt, ApplicationError> {
        operation.validate()?;
        let owner = &lease.owner;
        let control = self.control(owner, &operation.environment_id)?;
        let _guard = control.lock().await;
        match self.ledger.merge(owner, &operation.id) {
            Ok(record) => {
                if record.request != *operation
                    || record.lease.job_id != lease.job_id
                    || record.lease.session_id != lease.session_id
                {
                    return Err(ApplicationError::Conflict);
                }
                if let Some(receipt) = record.receipt {
                    return Ok(receipt);
                }
                if record.state != WorkspaceMergeState::Preparing {
                    return Err(ApplicationError::Conflict);
                }
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let environment = self.ledger.environment(owner, &operation.environment_id)?;
        if environment.revision != operation.expected_revision
            || environment.spec.session_id != lease.session_id
        {
            return Err(ApplicationError::Conflict);
        }
        for snapshot in [&operation.base, &operation.parent, &operation.child] {
            if self.ledger.snapshot(owner, &snapshot.id)? != *snapshot {
                return Err(ApplicationError::Conflict);
            }
        }
        let paths = [
            self.snapshot_path(owner, &operation.base.id),
            self.snapshot_path(owner, &operation.parent.id),
            self.snapshot_path(owner, &operation.child.id),
        ];
        let snapshots = [
            operation.base.clone(),
            operation.parent.clone(),
            operation.child.clone(),
        ];
        let reviewed = operation.plan.clone();
        let archive = self.snapshots.join(format!(
            "merge-{}.tar",
            zuno_orchestration::sha256_json(&json!([owner, operation.id, operation.plan.digest()]))
        ));
        let pending = archive.with_extension(format!("{}.pending", uuid::Uuid::new_v4().simple()));
        let output = pending.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut trees = Vec::new();
            for (path, snapshot) in paths.iter().zip(&snapshots) {
                trees.push(crate::workspace_merge::SnapshotTree::read(
                    path,
                    &snapshot.sha256,
                    snapshot.bytes,
                )?);
            }
            crate::workspace_merge::write_merged_archive(
                &reviewed, &trees[0], &trees[1], &trees[2], &output,
            )
        })
        .await
        .map_err(crate::storage)?;
        let (sha, bytes) = match result {
            Ok(value) => value,
            Err(error) => {
                let _ = tokio::fs::remove_file(&pending).await;
                return Err(error);
            }
        };
        tokio::fs::rename(&pending, &archive)
            .await
            .map_err(crate::storage)?;
        // No physical workspace writes occurred before this current authority
        // check; cancelled or revised approval cannot authorize publication.
        authority
            .authorize_merge(lease, &environment, operation)
            .await?;
        let record = self.ledger.begin_merge(lease, operation)?;
        if let Some(receipt) = record.receipt {
            return Ok(receipt);
        }
        if record.state != WorkspaceMergeState::Preparing {
            return Err(ApplicationError::Conflict);
        }
        let labels = self.merge_labels(owner, &environment.spec, &record)?;
        let name = format!(
            "zuno-merge-restore-{}",
            zuno_orchestration::sha256_json(&json!([owner, operation.id]))
        );
        self.remove_merge_helper(&name, &labels).await?;
        self.cleanup_root_metadata(&record.volume, &labels, &archive)
            .await?;
        match self
            .docker
            .json(Method::GET, &format!("/volumes/{}", record.volume), None)
            .await
        {
            Ok(info) => {
                if info.get("Labels") != Some(&labels) {
                    return Err(ApplicationError::Conflict);
                }
                self.docker
                    .json(Method::DELETE, &format!("/volumes/{}", record.volume), None)
                    .await?;
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.docker
            .json(
                Method::POST,
                "/volumes/create",
                Some(&json!({"Name":record.volume,"Labels":labels})),
            )
            .await?;
        let spec = &environment.spec;
        self.docker.json(Method::POST,&format!("/containers/create?name={name}"),Some(&json!({
            "Image":spec.image,"Cmd":["true"],"Labels":labels,
            "HostConfig":{"ReadonlyRootfs":true,"NetworkMode":"none","CapDrop":["ALL"],"SecurityOpt":["no-new-privileges"],
                "Memory":spec.memory_bytes,"PidsLimit":spec.pids_limit,
                "Mounts":[{"Type":"volume","Source":record.volume,"Target":"/workspace","VolumeOptions":{"NoCopy":true}}]}
        }))).await?;
        let restored = archive.with_extension(format!("{}.restore", uuid::Uuid::new_v4().simple()));
        let destination = restored.clone();
        let source = archive.clone();
        let expected_sha = sha.clone();
        let size = tokio::task::spawn_blocking(move || {
            crate::archive::verify(&source, &expected_sha, bytes)?;
            crate::archive::for_restore(&source, &destination)
        })
        .await
        .map_err(crate::storage)??;
        let applied = self
            .docker
            .upload_archive(
                &format!("/containers/{name}/archive?path=/workspace"),
                &restored,
                size,
            )
            .await;
        let _ = tokio::fs::remove_file(&restored).await;
        let verified = async {
            applied?;
            self.restore_root_metadata(&environment, &record.volume, &labels, &archive)
                .await?;
            let check = archive.with_extension(format!("{}.verify", uuid::Uuid::new_v4().simple()));
            let downloaded = self
                .docker
                .download_archive(
                    &format!("/containers/{name}/archive?path=/workspace"),
                    &check,
                    512 * 1024 * 1024,
                )
                .await;
            let verify = match downloaded {
                Ok((observed_sha, observed_bytes)) => {
                    let observed = check.clone();
                    let expected = archive.clone();
                    tokio::task::spawn_blocking(move || {
                        let actual = crate::workspace_merge::SnapshotTree::read(
                            &observed,
                            &observed_sha,
                            observed_bytes,
                        )?;
                        let expected =
                            crate::workspace_merge::SnapshotTree::read(&expected, &sha, bytes)?;
                        if actual.entries() != expected.entries() {
                            return Err(ApplicationError::Conflict);
                        }
                        Ok(())
                    })
                    .await
                    .map_err(crate::storage)?
                }
                Err(error) => Err(error),
            };
            let _ = tokio::fs::remove_file(&check).await;
            verify
        }
        .await;
        let cleanup = self.remove_merge_helper(&name, &labels).await;
        verified?;
        cleanup?;
        // Publication wins or cancellation wins under the ledger transaction.
        // A partial restore remains unpublished and can be rebuilt from snapshots.
        self.ledger.publish_merge(owner, &operation.id)
    }

    pub fn inspect_workspace_merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<Option<WorkspaceMergeReceipt>, ApplicationError> {
        let record = self.ledger.merge(owner, id)?;
        Ok(record.receipt)
    }
    pub fn cancel_workspace_merge(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<(), ApplicationError> {
        self.ledger.cancel_merge(owner, id)?;
        Ok(())
    }
}
