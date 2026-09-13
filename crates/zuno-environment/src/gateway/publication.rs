//! Shared restore/read-back verification. Callers own the journal reservation
//! and publish the volume pointer only after this function succeeds.
use super::*;

pub(super) struct CandidateArchive<'a> {
    pub path: &'a Path,
    pub sha256: &'a str,
    pub bytes: u64,
}

impl DockerGateway {
    pub(super) async fn restore_candidate(
        &self,
        environment: &Environment,
        volume: &str,
        name: &str,
        labels: &Value,
        candidate: CandidateArchive<'_>,
    ) -> Result<(), ApplicationError> {
        let CandidateArchive {
            path: archive,
            sha256: sha,
            bytes,
        } = candidate;
        self.remove_merge_helper(name, labels).await?;
        self.cleanup_root_metadata(volume, labels, archive).await?;
        match self
            .docker
            .json(Method::GET, &format!("/volumes/{}", volume), None)
            .await
        {
            Ok(info) => {
                if info.get("Labels") != Some(labels) {
                    return Err(ApplicationError::Conflict);
                }
                self.docker
                    .json(Method::DELETE, &format!("/volumes/{}", volume), None)
                    .await?;
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.docker
            .json(
                Method::POST,
                "/volumes/create",
                Some(&json!({"Name":volume,"Labels":labels})),
            )
            .await?;
        let spec = &environment.spec;
        self.docker.json(Method::POST,&format!("/containers/create?name={name}"),Some(&json!({
            "Image":spec.image,"Cmd":["true"],"Labels":labels,
            "HostConfig":{"ReadonlyRootfs":true,"NetworkMode":"none","CapDrop":["ALL"],"SecurityOpt":["no-new-privileges"],
                "Memory":spec.memory_bytes,"PidsLimit":spec.pids_limit,
                "Mounts":[{"Type":"volume","Source":volume,"Target":"/workspace","VolumeOptions":{"NoCopy":true}}]}
        }))).await?;
        let restored = archive.with_extension(format!("{}.restore", uuid::Uuid::new_v4().simple()));
        let destination = restored.clone();
        let source = archive.to_owned();
        let expected_sha = sha.to_owned();
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
            self.restore_root_metadata(environment, volume, labels, archive)
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
                    let expected = archive.to_owned();
                    let sha = sha.to_owned();
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
        let cleanup = self.remove_merge_helper(name, labels).await;
        verified?;
        cleanup?;
        Ok(())
    }
}
