mod workspace;

use crate::docker::Docker;
use crate::ledger::{Ledger, Operation};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use zuno_application::ApplicationError;
use zuno_application::environment::{
    CommandOperation, Environment, EnvironmentProvider, EnvironmentSnapshot, EnvironmentSpec,
    OperationAuthority, OperationCompletion, OperationCompletionSink, OperationGateway,
    OperationPhase, OperationReceipt, OutputCursor, OutputPage,
};
use zuno_application::runtime::ExecutionLease;
use zuno_types::identity::{EnvironmentId, EnvironmentSnapshotId, OperationId, PrincipalKey};

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;

pub struct DockerGateway {
    docker: Docker,
    ledger: Ledger,
    authority: Arc<dyn OperationAuthority>,
    controls: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    snapshots: PathBuf,
}
impl DockerGateway {
    /// Reconstruct delivery work from the ledger after a gateway restart. A
    /// successful sink acknowledgement is persisted before output may be removed.
    pub async fn deliver_completions(
        &self,
        sink: &dyn OperationCompletionSink,
        limit: u32,
    ) -> Result<u32, ApplicationError> {
        let operations = self.ledger.scan_deliveries(limit)?;
        let mut delivered = 0;
        for operation in operations {
            let control = self.control(&operation.owner, &operation.request.environment_id)?;
            let Ok(guard) = control.try_lock() else {
                // The live submitter owns the created -> started boundary.
                // Other environments must remain eligible for this scan.
                continue;
            };
            let operation = self
                .ledger
                .operation(&operation.owner, &operation.request.id)?;
            let completion = if let Some(completion) = self
                .ledger
                .completion(&operation.owner, &operation.request.id)?
            {
                completion
            } else {
                let receipt = self.observe(&operation).await?;
                if !matches!(
                    receipt.phase,
                    OperationPhase::Completed | OperationPhase::Cancelled
                ) {
                    continue;
                }
                let mut page = if receipt.phase == OperationPhase::Cancelled
                    && receipt.exit_code.is_none()
                {
                    OutputPage {
                        chunks: Vec::new(),
                        next: OutputCursor::default(),
                        end_of_available: true,
                    }
                } else {
                    self.output(
                        &operation.owner,
                        &operation.request.id,
                        OutputCursor::default(),
                        u32::try_from(zuno_application::environment::MAX_COMPLETION_OUTPUT_BYTES)
                            .expect("bounded constant"),
                    )
                    .await?
                };
                let omitted_chunks =
                    page.chunks.len() > zuno_application::environment::MAX_COMPLETION_OUTPUT_CHUNKS;
                page.chunks
                    .truncate(zuno_application::environment::MAX_COMPLETION_OUTPUT_CHUNKS);
                let completion = OperationCompletion {
                    lease: operation.lease,
                    operation: operation.request,
                    receipt,
                    output: page.chunks,
                    output_truncated: !page.end_of_available || omitted_chunks,
                };
                self.ledger.capture(&completion)?;
                completion
            };
            drop(guard);
            sink.publish(&completion).await?;
            self.ledger.acknowledge(&completion)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    pub async fn connect(
        socket: &Path,
        ledger: &Path,
        authority: Arc<dyn OperationAuthority>,
    ) -> Result<Self, ApplicationError> {
        let docker = Docker::connect(socket).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = ledger.parent().ok_or_else(|| {
                ApplicationError::Invalid(
                    "execution ledger requires a private data directory".to_owned(),
                )
            })?;
            let metadata = std::fs::symlink_metadata(parent).map_err(crate::storage)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(ApplicationError::Invalid(
                    "execution gateway data directory must be private (0700)".to_owned(),
                ));
            }
        }
        let snapshots = ledger.with_extension("snapshots");
        let ledger = Ledger::open(ledger)?;
        std::fs::create_dir_all(&snapshots).map_err(crate::storage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::symlink_metadata(&snapshots)
                .map_err(crate::storage)?
                .file_type()
                .is_symlink()
            {
                return Err(ApplicationError::Forbidden);
            }
            std::fs::set_permissions(&snapshots, std::fs::Permissions::from_mode(0o700))
                .map_err(crate::storage)?;
        }
        Ok(Self {
            docker,
            ledger,
            authority,
            controls: Mutex::new(BTreeMap::new()),
            snapshots,
        })
    }

    fn control(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, ApplicationError> {
        let key = Self::volume(owner, id);
        let mut controls = self
            .controls
            .lock()
            .map_err(|_| ApplicationError::Unavailable)?;
        controls.retain(|_, control| control.strong_count() > 0);
        if let Some(control) = controls.get(&key).and_then(Weak::upgrade) {
            return Ok(control);
        }
        let control = Arc::new(tokio::sync::Mutex::new(()));
        controls.insert(key, Arc::downgrade(&control));
        Ok(control)
    }

    fn volume(owner: &PrincipalKey, id: &EnvironmentId) -> String {
        format!(
            "zuno-env-{}",
            zuno_orchestration::sha256_json(&json!([owner, id]))
        )
    }
    fn container(owner: &PrincipalKey, id: &OperationId) -> String {
        format!(
            "zuno-op-{}",
            zuno_orchestration::sha256_json(&json!([owner, id]))
        )
    }
    fn environment_labels(owner: &PrincipalKey, spec: &EnvironmentSpec) -> Value {
        json!({
            "zuno.channel":"enterprise-preview",
            "zuno.environment":spec.id,
            "zuno.owner":zuno_orchestration::sha256_json(&json!(owner)),
            "zuno.definition":zuno_orchestration::sha256_json(&json!(spec)),
        })
    }
    fn operation_labels(operation: &Operation) -> Value {
        json!({
            "zuno.channel":"enterprise-preview",
            "zuno.operation":operation.request.id,
            "zuno.environment":operation.request.environment_id,
            "zuno.owner":zuno_orchestration::sha256_json(&json!(operation.owner)),
            "zuno.request":zuno_orchestration::sha256_json(&json!([operation.lease.job_id,operation.lease.session_id,operation.request])),
        })
    }
    fn snapshot_path(&self, owner: &PrincipalKey, id: &EnvironmentSnapshotId) -> PathBuf {
        self.snapshots.join(format!(
            "{}.tar",
            zuno_orchestration::sha256_json(&json!([owner, id]))
        ))
    }

    async fn snapshot_container(
        &self,
        environment: &Environment,
        name: &str,
        readonly: bool,
    ) -> Result<(), ApplicationError> {
        let config = json!({
            "Image":environment.spec.image,"Cmd":["true"],"Labels":self.storage_labels(&environment.owner,&environment.spec)?,
            "HostConfig":{"ReadonlyRootfs":true,"NetworkMode":"none","CapDrop":["ALL"],"SecurityOpt":["no-new-privileges"],
                "Memory":environment.spec.memory_bytes,"PidsLimit":environment.spec.pids_limit,
                "Mounts":[{"Type":"volume","Source":Self::volume(&environment.owner,&environment.spec.id),"Target":"/workspace","ReadOnly":readonly}]}
        });
        self.docker
            .json(
                Method::POST,
                &format!("/containers/create?name={name}"),
                Some(&config),
            )
            .await?;
        Ok(())
    }
    async fn volume_exists(&self, environment: &Environment) -> Result<(), ApplicationError> {
        let volume = Self::volume(&environment.owner, &environment.spec.id);
        let info = self
            .docker
            .json(Method::GET, &format!("/volumes/{volume}"), None)
            .await?;
        if info.get("Labels") != Some(&self.storage_labels(&environment.owner, &environment.spec)?)
        {
            return Err(ApplicationError::Conflict);
        }
        Ok(())
    }
    async fn container_info(&self, operation: &Operation) -> Result<Value, ApplicationError> {
        let info = self
            .docker
            .json(
                Method::GET,
                &format!("/containers/{}/json", operation.container),
                None,
            )
            .await?;
        if info.pointer("/Config/Labels") != Some(&Self::operation_labels(operation)) {
            return Err(ApplicationError::Conflict);
        }
        Ok(info)
    }
    async fn prepare_container(
        &self,
        operation: &Operation,
        environment: &Environment,
    ) -> Result<(), ApplicationError> {
        match self.container_info(operation).await {
            Ok(_) => return Ok(()),
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        // Every process gets its own durable Docker identity and logs. The
        // workspace volume persists independently of the command container.
        let spec = &environment.spec;
        let config = json!({
            "Image":spec.image,"Cmd":operation.request.argv,"WorkingDir":"/workspace",
            "Tty":false,"OpenStdin":false,"AttachStdout":true,"AttachStderr":true,
            "Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin","HOME=/tmp"],
            "Labels":Self::operation_labels(operation),
            "HostConfig":{
                "ReadonlyRootfs":true,"NetworkMode":"none","CapDrop":["ALL"],
                "SecurityOpt":["no-new-privileges"],
                "Memory":spec.memory_bytes,"NanoCpus":u64::from(spec.cpu_millis)*1_000_000,
                "PidsLimit":spec.pids_limit,
                "Mounts":[{"Type":"volume","Source":Self::volume(&environment.owner,&spec.id),"Target":"/workspace"}],
                "Tmpfs":{"/tmp":"rw,noexec,nosuid,size=16777216"},
                "LogConfig":{"Type":"json-file","Config":{}},
            },
        });
        match self
            .docker
            .json(
                Method::POST,
                &format!("/containers/create?name={}", operation.container),
                Some(&config),
            )
            .await
        {
            Ok(_) | Err(ApplicationError::Conflict) => {}
            Err(error) => return Err(error),
        }
        self.container_info(operation).await?;
        Ok(())
    }

    async fn observe(&self, operation: &Operation) -> Result<OperationReceipt, ApplicationError> {
        if matches!(
            operation.receipt.phase,
            OperationPhase::Completed | OperationPhase::Cancelled
        ) {
            return Ok(operation.receipt.clone());
        }
        let mut info = match self.container_info(operation).await {
            Ok(info) => info,
            Err(ApplicationError::NotFound) => {
                return if operation.receipt.phase == OperationPhase::Prepared {
                    Ok(operation.receipt.clone())
                } else {
                    self.ledger.observed(
                        &operation.owner,
                        &operation.request.id,
                        OperationPhase::Uncertain,
                        None,
                    )
                };
            }
            Err(error) => return Err(error),
        };
        if operation.receipt.cancellation_requested
            && info.pointer("/State/Running").and_then(Value::as_bool) == Some(true)
        {
            let _ = self
                .docker
                .json(
                    Method::POST,
                    &format!("/containers/{}/stop?t=5", operation.container),
                    None,
                )
                .await;
            info = self.container_info(operation).await?;
        }
        let status = info
            .pointer("/State/Status")
            .and_then(Value::as_str)
            .ok_or(ApplicationError::Conflict)?;
        let (phase, exit) = match status {
            "running" | "paused" | "restarting" => (OperationPhase::Running, None),
            "exited" | "dead" => {
                let started = info
                    .pointer("/State/StartedAt")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty() && !value.starts_with("0001-"));
                if !started {
                    (OperationPhase::Uncertain, None)
                } else {
                    (
                        OperationPhase::Completed,
                        info.pointer("/State/ExitCode").and_then(Value::as_i64),
                    )
                }
            }
            "created" if operation.receipt.phase == OperationPhase::Prepared => {
                (OperationPhase::Prepared, None)
            }
            _ => (OperationPhase::Uncertain, None),
        };
        self.ledger
            .observed(&operation.owner, &operation.request.id, phase, exit)
    }
}

#[async_trait]
impl EnvironmentProvider for DockerGateway {
    async fn acquire(
        &self,
        owner: &PrincipalKey,
        spec: EnvironmentSpec,
    ) -> Result<Environment, ApplicationError> {
        spec.validate()?;
        let control = self.control(owner, &spec.id)?;
        let _guard = control.lock().await;
        match self.ledger.environment(owner, &spec.id) {
            Ok(environment) => {
                if environment.spec != spec {
                    return Err(ApplicationError::Conflict);
                }
                self.volume_exists(&environment).await?;
                return Ok(environment);
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        if self.ledger.contains_environment(owner, &spec.id)? {
            return Err(ApplicationError::Conflict);
        }
        self.ledger
            .require_unreserved_environment(owner, &spec.id)?;
        let volume = Self::volume(owner, &spec.id);
        let body = json!({"Name":volume,"Labels":Self::environment_labels(owner,&spec)});
        self.docker
            .json(Method::POST, "/volumes/create", Some(&body))
            .await?;
        let environment = Environment {
            owner: owner.clone(),
            spec: spec.clone(),
            revision: 1,
        };
        self.volume_exists(&environment).await?;
        self.ledger.create_environment(owner, &spec)
    }
    async fn get(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
    ) -> Result<Environment, ApplicationError> {
        let environment = self.ledger.environment(owner, id)?;
        self.volume_exists(&environment).await?;
        Ok(environment)
    }

    async fn snapshot(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        expected_revision: u64,
    ) -> Result<EnvironmentSnapshot, ApplicationError> {
        let snapshot_id =
            EnvironmentSnapshotId::new(format!("snapshot-{}", uuid::Uuid::now_v7().simple()))
                .map_err(crate::storage)?;
        self.snapshot_named(owner, id, expected_revision, &snapshot_id)
            .await
    }

    async fn fork(
        &self,
        owner: &PrincipalKey,
        snapshot: &EnvironmentSnapshot,
        spec: EnvironmentSpec,
    ) -> Result<Environment, ApplicationError> {
        self.fork_snapshot(owner, snapshot, spec).await
    }

    async fn release(
        &self,
        owner: &PrincipalKey,
        id: &EnvironmentId,
        expected_revision: u64,
    ) -> Result<(), ApplicationError> {
        let control = self.control(owner, id)?;
        let _guard = control.lock().await;
        let Some(operations) = self.ledger.release(owner, id, expected_revision, false)? else {
            return Ok(());
        };
        for operation_id in operations {
            let operation = self.ledger.operation(owner, &operation_id)?;
            let container = Self::container(owner, &operation_id);
            if container != operation.container {
                return Err(ApplicationError::Conflict);
            }
            let info = match self
                .docker
                .json(Method::GET, &format!("/containers/{container}/json"), None)
                .await
            {
                Ok(info) => info,
                Err(ApplicationError::NotFound) => continue,
                Err(error) => return Err(error),
            };
            if info.pointer("/Config/Labels") != Some(&Self::operation_labels(&operation)) {
                return Err(ApplicationError::Forbidden);
            }
            self.docker
                .json(Method::DELETE, &format!("/containers/{container}"), None)
                .await?;
        }
        let volume = Self::volume(owner, id);
        match self
            .docker
            .json(Method::GET, &format!("/volumes/{volume}"), None)
            .await
        {
            Ok(info) => {
                if info.pointer("/Labels/zuno.owner").and_then(Value::as_str)
                    != Some(zuno_orchestration::sha256_json(&json!(owner)).as_str())
                    || info
                        .pointer("/Labels/zuno.environment")
                        .and_then(Value::as_str)
                        != Some(id.as_str())
                    || info.pointer("/Labels/zuno.channel").and_then(Value::as_str)
                        != Some("enterprise-preview")
                {
                    return Err(ApplicationError::Forbidden);
                }
            }
            Err(ApplicationError::NotFound) => {
                self.ledger.release(owner, id, expected_revision, true)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        match self
            .docker
            .json(
                Method::DELETE,
                &format!("/volumes/{}", Self::volume(owner, id)),
                None,
            )
            .await
        {
            Ok(_) | Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.ledger.release(owner, id, expected_revision, true)?;
        Ok(())
    }
}

#[async_trait]
impl OperationGateway for DockerGateway {
    async fn submit(
        &self,
        lease: &ExecutionLease,
        request: CommandOperation,
    ) -> Result<OperationReceipt, ApplicationError> {
        request.validate()?;
        let control = self.control(&lease.owner, &request.environment_id)?;
        let _guard = control.lock().await;
        let environment = self.get(&lease.owner, &request.environment_id).await?;
        self.authority
            .authorize(lease, &environment, &request)
            .await?;
        let operation =
            self.ledger
                .admit(lease, &request, Self::container(&lease.owner, &request.id))?;
        if operation.receipt.phase != OperationPhase::Prepared {
            return self.observe(&operation).await;
        }
        self.prepare_container(&operation, &environment).await?;
        let info = self.container_info(&operation).await?;
        if info.pointer("/State/Status").and_then(Value::as_str) != Some("created") {
            return self.observe(&operation).await;
        }
        // Recheck after Docker preparation; neither a prior approval nor
        // container creation extends a Worker lease.
        self.authority
            .authorize(lease, &environment, &request)
            .await?;
        if !self.ledger.begin_start(&lease.owner, &request.id)? {
            return self
                .observe(&self.ledger.operation(&lease.owner, &request.id)?)
                .await;
        }
        let result = self
            .docker
            .json(
                Method::POST,
                &format!("/containers/{}/start", operation.container),
                None,
            )
            .await;
        if result.is_err() {
            // An acknowledgement can be lost after the command started or
            // completed. Inspect the original container; never repeat start.
            let current = self.ledger.operation(&lease.owner, &request.id)?;
            return self.observe(&current).await;
        }
        self.observe(&self.ledger.operation(&lease.owner, &request.id)?)
            .await
    }

    async fn inspect(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
    ) -> Result<OperationReceipt, ApplicationError> {
        let operation = self.ledger.operation(owner, id)?;
        let control = self.control(owner, &operation.request.environment_id)?;
        let _guard = control.lock().await;
        self.observe(&self.ledger.operation(owner, id)?).await
    }

    async fn output(
        &self,
        owner: &PrincipalKey,
        id: &OperationId,
        cursor: OutputCursor,
        maximum_bytes: u32,
    ) -> Result<OutputPage, ApplicationError> {
        let operation = self.ledger.operation(owner, id)?;
        self.container_info(&operation).await?;
        self.docker
            .log_page(
                &format!(
                    "/containers/{}/logs?stdout=true&stderr=true&timestamps=false",
                    operation.container
                ),
                cursor,
                maximum_bytes,
            )
            .await
    }

    async fn cancel(
        &self,
        lease: &ExecutionLease,
        id: &OperationId,
    ) -> Result<OperationReceipt, ApplicationError> {
        let operation = self.ledger.operation(&lease.owner, id)?;
        let control = self.control(&lease.owner, &operation.request.environment_id)?;
        let _guard = control.lock().await;
        let operation = self.ledger.operation(&lease.owner, id)?;
        if operation.lease.job_id != lease.job_id || operation.lease.session_id != lease.session_id
        {
            return Err(ApplicationError::Conflict);
        }
        let environment = self
            .get(&lease.owner, &operation.request.environment_id)
            .await?;
        self.authority
            .authorize(lease, &environment, &operation.request)
            .await?;
        let receipt = self.observe(&operation).await?;
        if matches!(
            receipt.phase,
            OperationPhase::Completed | OperationPhase::Cancelled
        ) {
            return Ok(receipt);
        }
        let requested = self.ledger.cancel_requested(&lease.owner, id)?;
        if requested.phase == OperationPhase::Cancelled {
            return Ok(requested);
        }
        match self
            .docker
            .json(
                Method::POST,
                &format!("/containers/{}/stop?t=5", operation.container),
                None,
            )
            .await
        {
            Ok(_) => {
                self.observe(&self.ledger.operation(&lease.owner, id)?)
                    .await
            }
            Err(_) => self
                .ledger
                .observed(&lease.owner, id, OperationPhase::Uncertain, None),
        }
    }
}
