//! Named immutable snapshots and retry-safe publication of isolated workspaces.

use super::*;
use crate::ledger::ForkPreparation;

impl DockerGateway {
    /// The assignment was resolved and admitted by the authenticated data owner.
    /// Neither source/target paths nor arbitrary volume identities are caller input.
    pub async fn prepare_child_workspace(
        &self,
        owner: &PrincipalKey,
        assignment: &zuno_application::child::ChildWorkspaceAssignment,
        existing_parent: bool,
    ) -> Result<zuno_application::child::ChildWorkspaceReceipt, ApplicationError> {
        use zuno_application::child::ChildWorkspaceReceipt;
        assignment.parent.validate()?;
        assignment.target.validate()?;
        if assignment.parent.id == assignment.target.id {
            return Err(ApplicationError::Conflict);
        }
        if assignment.resume {
            let target = self.get(owner, &assignment.target.id).await?;
            if target.spec != assignment.target {
                return Err(ApplicationError::Conflict);
            }
            return Ok(ChildWorkspaceReceipt {
                child_job_id: assignment.child_job_id.clone(),
                parent_environment_id: assignment.parent.id.clone(),
                snapshot: None,
                target,
            });
        }
        let parent = if existing_parent {
            self.get(owner, &assignment.parent.id).await?
        } else {
            self.acquire(owner, assignment.parent.clone()).await?
        };
        if parent.spec != assignment.parent {
            return Err(ApplicationError::Conflict);
        }
        let snapshot_id = EnvironmentSnapshotId::new(format!("child-{}", assignment.child_job_id))
            .map_err(crate::storage)?;
        let snapshot = match self.ledger.snapshot(owner, &snapshot_id) {
            Ok(snapshot) if snapshot.environment_id == parent.spec.id => snapshot,
            Ok(_) => return Err(ApplicationError::Conflict),
            Err(ApplicationError::NotFound) => {
                self.snapshot_named(owner, &parent.spec.id, parent.revision, &snapshot_id)
                    .await?
            }
            Err(error) => return Err(error),
        };
        let target = self
            .fork(owner, &snapshot, assignment.target.clone())
            .await?;
        // Without a control-plane receipt this target has not been admitted for
        // execution yet. A changed target requires inspection, not false readiness.
        if target.revision != 1 {
            return Err(ApplicationError::Conflict);
        }
        Ok(ChildWorkspaceReceipt {
            child_job_id: assignment.child_job_id.clone(),
            parent_environment_id: parent.spec.id,
            snapshot: Some(snapshot),
            target,
        })
    }

    pub(super) fn storage_labels(
        &self,
        owner: &PrincipalKey,
        spec: &EnvironmentSpec,
    ) -> Result<Value, ApplicationError> {
        let mut labels = Self::environment_labels(owner, spec);
        if let Some(nonce) = self.ledger.fork_nonce(owner, &spec.id)? {
            labels["zuno.fork"] = json!(nonce);
        }
        if let Some(record) = self.ledger.active_volume(owner, &spec.id)? {
            labels["zuno.merge"] = json!(record.request.id);
            labels["zuno.merge.nonce"] = json!(record.nonce);
        }
        Ok(labels)
    }

    /// Remove only our never-started transfer helper. A matching name alone is
    /// insufficient; an unrelated or running container is preserved.
    async fn remove_transfer_helper(
        &self,
        environment: &Environment,
        name: &str,
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
        if info.pointer("/Config/Labels")
            != Some(&self.storage_labels(&environment.owner, &environment.spec)?)
            || info.pointer("/State/Status").and_then(Value::as_str) != Some("created")
        {
            return Err(ApplicationError::Conflict);
        }
        self.docker
            .json(Method::DELETE, &format!("/containers/{name}"), None)
            .await?;
        Ok(())
    }

    /// Callers derive the ID from a stable operation, never from a Worker attempt.
    /// Once published, retry returns the same snapshot even if the parent moved on.
    pub async fn snapshot_named(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        revision: u64,
        snapshot_id: &EnvironmentSnapshotId,
    ) -> Result<EnvironmentSnapshot, ApplicationError> {
        let control = self.control(owner, id)?;
        let _guard = control.lock().await;
        match self.ledger.snapshot(owner, snapshot_id) {
            Ok(snapshot) if snapshot.environment_id == *id && snapshot.revision == revision => {
                return Ok(snapshot);
            }
            Ok(_) => return Err(ApplicationError::Conflict),
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let environment = self.ledger.require_idle(owner, id, revision)?;
        self.volume_exists(&environment).await?;
        let name = format!(
            "zuno-snapshot-{}",
            zuno_orchestration::sha256_json(&json!([owner, snapshot_id]))
        );
        self.remove_transfer_helper(&environment, &name).await?;
        self.snapshot_container(&environment, &name, true).await?;
        let path = self.snapshot_path(owner, snapshot_id);
        let temporary = path.with_extension("pending");
        let result = self
            .docker
            .download_archive(
                &format!("/containers/{name}/archive?path=/workspace"),
                &temporary,
                512 * 1024 * 1024,
            )
            .await;
        let cleanup = self.remove_transfer_helper(&environment, &name).await;
        let (sha256, bytes) = match result {
            Ok(value) => value,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error);
            }
        };
        cleanup?;
        let check_path = temporary.clone();
        let check_sha = sha256.clone();
        tokio::task::spawn_blocking(move || crate::archive::verify(&check_path, &check_sha, bytes))
            .await
            .map_err(crate::storage)??;
        tokio::fs::rename(&temporary, &path)
            .await
            .map_err(crate::storage)?;
        std::fs::File::open(&self.snapshots)
            .and_then(|directory| directory.sync_all())
            .map_err(crate::storage)?;
        let snapshot = EnvironmentSnapshot {
            id: snapshot_id.clone(),
            environment_id: id.clone(),
            revision,
            sha256,
            bytes,
        };
        self.ledger.put_snapshot(owner, &snapshot)?;
        Ok(snapshot)
    }

    pub(super) async fn fork_snapshot(
        &self,
        owner: &PrincipalKey,
        snapshot: &EnvironmentSnapshot,
        spec: EnvironmentSpec,
    ) -> Result<Environment, ApplicationError> {
        spec.validate()?;
        let control = self.control(owner, &spec.id)?;
        let _guard = control.lock().await;
        match self.ledger.begin_fork(owner, snapshot, &spec)? {
            ForkPreparation::Committed(environment) => {
                self.volume_exists(&environment).await?;
                return Ok(environment);
            }
            ForkPreparation::Restore => {}
        }
        let path = self.snapshot_path(owner, &snapshot.id);
        let check_path = path.clone();
        let check = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            crate::archive::verify(&check_path, &check.sha256, check.bytes)
        })
        .await
        .map_err(crate::storage)??;
        let environment = Environment {
            owner: owner.clone(),
            spec: spec.clone(),
            revision: 1,
        };
        let name = format!(
            "zuno-fork-{}",
            zuno_orchestration::sha256_json(&json!([owner, spec.id]))
        );
        self.remove_transfer_helper(&environment, &name).await?;
        let volume = Self::volume(owner, &spec.id);
        self.cleanup_root_metadata(&volume, &self.storage_labels(owner, &spec)?, &path)
            .await?;
        match self
            .docker
            .json(Method::GET, &format!("/volumes/{volume}"), None)
            .await
        {
            Ok(info) => {
                // Only this unpublished intent's random nonce permits recovery.
                // A fresh/misconfigured ledger cannot erase an existing workspace.
                if info.get("Labels") != Some(&self.storage_labels(owner, &spec)?) {
                    return Err(ApplicationError::Conflict);
                }
                self.docker
                    .json(Method::DELETE, &format!("/volumes/{volume}"), None)
                    .await?;
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.docker
            .json(
                Method::POST,
                "/volumes/create",
                Some(&json!({
                    "Name":volume,"Labels":self.storage_labels(owner,&spec)?
                })),
            )
            .await?;
        self.volume_exists(&environment).await?;
        self.snapshot_container(&environment, &name, false).await?;
        let restored = path.with_extension(format!("restore-{}", uuid::Uuid::new_v4().simple()));
        let source = path.clone();
        let destination = restored.clone();
        let size =
            tokio::task::spawn_blocking(move || crate::archive::for_restore(&source, &destination))
                .await
                .map_err(crate::storage)??;
        let result = self
            .docker
            .upload_archive(
                &format!("/containers/{name}/archive?path=/workspace"),
                &restored,
                size,
            )
            .await;
        let _ = tokio::fs::remove_file(&restored).await;
        let cleanup = self.remove_transfer_helper(&environment, &name).await;
        result?;
        cleanup?;
        self.restore_root_metadata(
            &environment,
            &volume,
            &self.storage_labels(owner, &spec)?,
            &path,
        )
        .await?;
        self.ledger.finish_fork(owner, &spec)
    }
}
