//! Stream an explicitly authorized initial archive into a private immutable
//! snapshot, then reuse the existing non-overwriting fork publication.
use super::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use zuno_application::workspace_import::{
    WorkspaceImportAssignment, WorkspaceImportReceipt, WorkspaceInitializationAuthority,
};

impl DockerGateway {
    pub async fn initialize_workspace<R: AsyncRead + Unpin + Send>(
        &self,
        assigned: &WorkspaceImportAssignment,
        mut input: R,
        authority: &dyn WorkspaceInitializationAuthority,
    ) -> Result<WorkspaceImportReceipt, ApplicationError> {
        assigned.environment.validate()?;
        if assigned.environment.session_id != assigned.session_id
            || assigned.bytes == 0
            || assigned.bytes > zuno_application::workspace_import::MAX_IMPORT_BYTES
            || assigned.sha256.len() != 64
            || !assigned
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ApplicationError::Invalid(
                "invalid workspace import assignment".to_owned(),
            ));
        }
        let owner = assigned.principal.owner();
        let transfer = self.control(&owner, &assigned.source_environment_id())?;
        let _transfer_guard = transfer.lock().await;
        let id = assigned.snapshot_id();
        let path = self.snapshot_path(&owner, &id);
        let pending = self
            .snapshots
            .join(format!("upload-{}.pending", uuid::Uuid::new_v4().simple()));
        let canonical = self.snapshots.join(format!(
            "upload-{}.canonical",
            uuid::Uuid::new_v4().simple()
        ));
        let received = async {
            let mut file = tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&pending)
                .await
                .map_err(crate::storage)?;
            let mut digest = sha2::Sha256::new();
            use sha2::Digest as _;
            let mut bytes = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let count = input.read(&mut buffer).await.map_err(crate::storage)?;
                if count == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(count as u64)
                    .ok_or(ApplicationError::Conflict)?;
                if bytes > assigned.bytes {
                    return Err(ApplicationError::Invalid(
                        "workspace upload exceeds its declared size".to_owned(),
                    ));
                }
                digest.update(&buffer[..count]);
                file.write_all(&buffer[..count])
                    .await
                    .map_err(crate::storage)?;
            }
            if bytes != assigned.bytes || hex::encode(digest.finalize()) != assigned.sha256 {
                return Err(ApplicationError::Conflict);
            }
            file.sync_all().await.map_err(crate::storage)?;
            let source = pending.clone();
            let destination = canonical.clone();
            let hash = assigned.sha256.clone();
            tokio::task::spawn_blocking(move || {
                crate::workspace_merge::normalize_import_archive(
                    &source,
                    &hash,
                    bytes,
                    &destination,
                )
            })
            .await
            .map_err(crate::storage)?
        }
        .await;
        let _ = tokio::fs::remove_file(&pending).await;
        let (sha256, bytes) = match received {
            Ok(value) => value,
            Err(error) => {
                let _ = tokio::fs::remove_file(&canonical).await;
                return Err(error);
            }
        };
        let snapshot = EnvironmentSnapshot {
            id: id.clone(),
            environment_id: assigned.source_environment_id(),
            revision: 1,
            sha256,
            bytes,
        };
        // A retry cannot replace previously validated bytes or another upload.
        match self.ledger.snapshot(&owner, &id) {
            Ok(previous) => {
                if previous != snapshot {
                    let _ = tokio::fs::remove_file(&canonical).await;
                    return Err(ApplicationError::Conflict);
                }
                let _ = tokio::fs::remove_file(&canonical).await;
            }
            Err(ApplicationError::NotFound) => {
                match tokio::fs::hard_link(&canonical, &path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let path = path.clone();
                        let expected = snapshot.clone();
                        tokio::task::spawn_blocking(move || {
                            crate::archive::verify(&path, &expected.sha256, expected.bytes)
                        })
                        .await
                        .map_err(crate::storage)??;
                    }
                    Err(error) => return Err(crate::storage(error)),
                }
                let _ = tokio::fs::remove_file(&canonical).await;
                std::fs::File::open(&self.snapshots)
                    .and_then(|directory| directory.sync_all())
                    .map_err(crate::storage)?;
                self.ledger.put_snapshot(&owner, &snapshot)?;
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&canonical).await;
                return Err(error);
            }
        }
        if let Some(receipt) = authority.authorize_initialization(assigned).await? {
            assigned.validate_receipt(&receipt)?;
            return Ok(receipt);
        }
        let environment = self
            .fork_snapshot(&owner, &snapshot, assigned.environment.clone())
            .await?;
        let receipt = WorkspaceImportReceipt {
            import_id: assigned.id.clone(),
            archive_sha256: assigned.sha256.clone(),
            snapshot,
            environment,
        };
        assigned.validate_receipt(&receipt)?;
        authority.initialized(assigned, &receipt).await?;
        Ok(receipt)
    }
}
