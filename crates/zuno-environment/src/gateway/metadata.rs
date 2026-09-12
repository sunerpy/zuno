//! Numeric metadata restoration in the unpublished volume. No supplied shell
//! text, network, model credentials, state database or Docker socket is mounted.
use super::*;

impl DockerGateway {
    async fn remove_metadata_helper(
        &self,
        name: &str,
        labels: &Value,
        argv: &Value,
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
            || info.pointer("/Config/Cmd") != Some(argv)
        {
            return Err(ApplicationError::Conflict);
        }
        if info.pointer("/State/Running").and_then(Value::as_bool) == Some(true) {
            self.docker
                .json(Method::POST, &format!("/containers/{name}/stop?t=1"), None)
                .await?;
        }
        self.docker
            .json(Method::DELETE, &format!("/containers/{name}"), None)
            .await?;
        Ok(())
    }
    async fn metadata_request(
        &self,
        volume: &str,
        labels: &Value,
        archive: &Path,
    ) -> Result<Option<(String, Value, Value)>, ApplicationError> {
        let source = archive.to_owned();
        let metadata = tokio::task::spawn_blocking(move || crate::archive::root_metadata(&source))
            .await
            .map_err(crate::storage)??;
        let Some((mode, uid, gid)) = metadata else {
            return Ok(None);
        };
        let argv = json!([
            "/bin/sh",
            "-c",
            "chmod \"$1\" /workspace && chown \"$2:$3\" /workspace",
            "zuno-root-metadata",
            format!("{mode:o}"),
            uid.to_string(),
            gid.to_string()
        ]);
        let mut labels = labels.clone();
        labels["zuno.restore.metadata"] = json!(zuno_orchestration::sha256_json(&json!([
            volume, mode, uid, gid
        ])));
        let name = format!("zuno-metadata-{}", zuno_orchestration::sha256_json(&labels));
        Ok(Some((name, labels, argv)))
    }
    pub(super) async fn cleanup_root_metadata(
        &self,
        volume: &str,
        labels: &Value,
        archive: &Path,
    ) -> Result<(), ApplicationError> {
        if let Some((name, labels, argv)) = self.metadata_request(volume, labels, archive).await? {
            self.remove_metadata_helper(&name, &labels, &argv).await?;
        }
        Ok(())
    }
    pub(super) async fn restore_root_metadata(
        &self,
        environment: &Environment,
        volume: &str,
        labels: &Value,
        archive: &Path,
    ) -> Result<(), ApplicationError> {
        let Some((name, labels, argv)) = self.metadata_request(volume, labels, archive).await?
        else {
            return Ok(());
        };
        self.remove_metadata_helper(&name, &labels, &argv).await?;
        let spec = &environment.spec;
        self.docker.json(Method::POST,&format!("/containers/create?name={name}"),Some(&json!({
            "Image":spec.image,"Entrypoint":[],"Cmd":argv,"User":"0:0","WorkingDir":"/","Labels":labels,
            "Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "HostConfig":{"ReadonlyRootfs":true,"NetworkMode":"none","CapDrop":["ALL"],"CapAdd":["CHOWN","FOWNER"],"SecurityOpt":["no-new-privileges"],
                "Memory":spec.memory_bytes,"NanoCpus":u64::from(spec.cpu_millis)*1_000_000,"PidsLimit":spec.pids_limit,
                "Mounts":[{"Type":"volume","Source":volume,"Target":"/workspace","VolumeOptions":{"NoCopy":true}}]}
        }))).await?;
        let completed = async {
            self.docker
                .json(Method::POST, &format!("/containers/{name}/start"), None)
                .await?;
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                loop {
                    let info = self
                        .docker
                        .json(Method::GET, &format!("/containers/{name}/json"), None)
                        .await?;
                    if info.pointer("/Config/Labels") != Some(&labels)
                        || info.pointer("/Config/Cmd") != Some(&argv)
                    {
                        return Err(ApplicationError::Conflict);
                    }
                    if info.pointer("/State/Running").and_then(Value::as_bool) == Some(false) {
                        if info.pointer("/State/ExitCode").and_then(Value::as_i64) == Some(0) {
                            return Ok(());
                        }
                        return Err(ApplicationError::Invalid(
                            "restore image cannot apply workspace root ownership and mode"
                                .to_owned(),
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .map_err(|_| ApplicationError::Unavailable)?
        }
        .await;
        let cleanup = self.remove_metadata_helper(&name, &labels, &argv).await;
        completed?;
        cleanup?;
        Ok(())
    }
}
