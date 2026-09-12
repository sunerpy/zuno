//! A snapshot is published only after bounded streaming, archive validation and
//! fsync. Source receipts and target publication have separate authorities.
use super::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use zuno_application::workspace_transfer::SnapshotTransferAssignment;

impl DockerGateway {
    pub async fn export_snapshot(
        &self,
        assigned: &SnapshotTransferAssignment,
    ) -> Result<(EnvironmentSnapshot, tokio::fs::File), ApplicationError> {
        let owner = &assigned.request.lease.owner;
        let id = assigned.request.snapshot_id()?;
        let spec = &assigned.source.environment;
        spec.validate()?;
        let snapshot = match self.ledger.snapshot(owner, &id) {
            Ok(snapshot) => snapshot,
            Err(ApplicationError::NotFound) => {
                let source = if assigned.existing_source {
                    self.get(owner, &spec.id).await?
                } else {
                    self.acquire(owner, spec.clone()).await?
                };
                if source.spec != *spec {
                    return Err(ApplicationError::Conflict);
                }
                self.snapshot_named(owner, &spec.id, source.revision, &id)
                    .await?
            }
            Err(error) => return Err(error),
        };
        assigned.validate_snapshot(&snapshot)?;
        if !self.has_snapshot(owner, &snapshot).await? {
            return Err(ApplicationError::NotFound);
        }
        let file = tokio::fs::File::open(self.snapshot_path(owner, &id))
            .await
            .map_err(crate::storage)?;
        Ok((snapshot, file))
    }

    pub async fn has_snapshot(
        &self,
        owner: &PrincipalKey,
        snapshot: &EnvironmentSnapshot,
    ) -> Result<bool, ApplicationError> {
        match self.ledger.snapshot(owner, &snapshot.id) {
            Ok(existing) if existing == *snapshot => {}
            Ok(_) => return Err(ApplicationError::Conflict),
            Err(ApplicationError::NotFound) => return Ok(false),
            Err(error) => return Err(error),
        }
        let path = self.snapshot_path(owner, &snapshot.id);
        let expected = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            crate::archive::verify(&path, &expected.sha256, expected.bytes)
        })
        .await
        .map_err(crate::storage)??;
        Ok(true)
    }

    pub async fn receive_snapshot<R: AsyncRead + Unpin + Send>(
        &self,
        assigned: &SnapshotTransferAssignment,
        snapshot: &EnvironmentSnapshot,
        mut input: R,
    ) -> Result<(), ApplicationError> {
        assigned.validate_snapshot(snapshot)?;
        let owner = &assigned.request.lease.owner;
        let lock_id = EnvironmentId::new(format!(
            "snapshot-{}",
            zuno_orchestration::sha256_json(&json!([owner, snapshot.id]))
        ))
        .map_err(crate::storage)?;
        let lock = self.control(owner, &lock_id)?;
        let _guard = lock.lock().await;
        // RAII removes an incomplete file when a request is cancelled.
        let pending = tempfile::Builder::new()
            .prefix("transfer-")
            .tempfile_in(&self.snapshots)
            .map_err(crate::storage)?;
        let mut file = tokio::fs::File::from_std(pending.reopen().map_err(crate::storage)?);
        let mut received = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            let count = input.read(&mut buffer).await.map_err(crate::storage)?;
            if count == 0 {
                break;
            }
            received = received
                .checked_add(count as u64)
                .ok_or(ApplicationError::Conflict)?;
            if received > snapshot.bytes {
                return Err(ApplicationError::Conflict);
            }
            file.write_all(&buffer[..count])
                .await
                .map_err(crate::storage)?;
        }
        if received != snapshot.bytes {
            return Err(ApplicationError::Conflict);
        }
        file.sync_all().await.map_err(crate::storage)?;
        drop(file);
        let path = pending.path().to_owned();
        let expected = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            crate::archive::verify(&path, &expected.sha256, expected.bytes)
        })
        .await
        .map_err(crate::storage)??;
        let destination = self.snapshot_path(owner, &snapshot.id);
        match tokio::fs::hard_link(pending.path(), &destination).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let expected = snapshot.clone();
                tokio::task::spawn_blocking(move || {
                    crate::archive::verify(&destination, &expected.sha256, expected.bytes)
                })
                .await
                .map_err(crate::storage)??;
            }
            Err(error) => return Err(crate::storage(error)),
        }
        std::fs::File::open(&self.snapshots)
            .and_then(|directory| directory.sync_all())
            .map_err(crate::storage)?;
        match self.ledger.snapshot(owner, &snapshot.id) {
            Ok(previous) if previous == *snapshot => {}
            Ok(_) => return Err(ApplicationError::Conflict),
            Err(ApplicationError::NotFound) => self.ledger.put_snapshot(owner, snapshot)?,
            Err(error) => return Err(error),
        }
        Ok(())
    }

    pub async fn fork_transferred_workspace(
        &self,
        owner: &PrincipalKey,
        assignment: &zuno_application::child::ChildWorkspaceAssignment,
        snapshot: &EnvironmentSnapshot,
    ) -> Result<zuno_application::child::ChildWorkspaceReceipt, ApplicationError> {
        if assignment.resume
            || snapshot.id.as_str() != format!("child-{}", assignment.child_job_id)
            || snapshot.environment_id != assignment.parent.id
        {
            return Err(ApplicationError::Forbidden);
        }
        let target = self
            .fork_snapshot(owner, snapshot, assignment.target.clone())
            .await?;
        if target.revision != 1 {
            return Err(ApplicationError::Conflict);
        }
        Ok(zuno_application::child::ChildWorkspaceReceipt {
            child_job_id: assignment.child_job_id.clone(),
            parent_environment_id: assignment.parent.id.clone(),
            snapshot: Some(snapshot.clone()),
            target,
        })
    }
}
