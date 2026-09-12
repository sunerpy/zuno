//! Worker-side transport. No PostgreSQL dependency or database credentials.

pub mod child;
pub mod gateway;
pub mod live;
pub mod memory;
#[cfg(test)]
mod presentation_tests;
pub mod runtime;
pub mod tools;
pub mod workflow;

use async_trait::async_trait;
use futures::StreamExt as _;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use url::Url;
use zuno_application::runtime::{ConfigurationRef, ExecutionLease, RuntimeJob};
use zuno_auth::Secret;
use zuno_engine::state::remote::{RemoteTurnPersistence, StateTransport};
use zuno_engine::state::wire::{MAX_WORKER_FRAME_BYTES, StateRequest, StateResponse};
use zuno_engine::state::{TurnStateError, TurnStateScope};
use zuno_identity::worker::JobGrantToken;
use zuno_types::identity::WorkerInstanceId;

pub const CLAIM_PATH: &str = "internal/worker/v1/claim";
pub const RENEW_PATH: &str = "internal/worker/v1/renew";
pub const STATE_PATH: &str = "internal/worker/v1/state";
pub const LIVE_PATH: &str = "internal/worker/v1/live";
pub const MEMORY_PATH: &str = "internal/worker/v1/memory";
pub const CHILD_PATH: &str = "internal/worker/v1/children";
pub const WORKFLOW_PATH: &str = "internal/worker/v1/workflows";
pub const FINISH_PATH: &str = "internal/worker/v1/finish";
pub const GRANT_HEADER: &str = "x-zuno-job-grant";
pub const GATEWAY_TICKET_PATH: &str = "internal/worker/v1/gateway-ticket";
pub const GATEWAY_RESOLVE_PATH: &str = "internal/gateway/v1/resolve";
pub const GATEWAY_PREPARE_PATH: &str = "internal/gateway/v1/prepare";
pub const GATEWAY_AUTHORIZE_PATH: &str = "internal/gateway/v1/authorize";
pub const GATEWAY_COMPLETION_PATH: &str = "internal/gateway/v1/completion";
pub const GATEWAY_CANCELLATIONS_PATH: &str = "internal/gateway/v1/cancellations";
pub const GATEWAY_CHILD_WORKSPACE_PATH: &str = "internal/gateway/v1/child-workspace";
pub const GATEWAY_TICKET_HEADER: &str = "x-zuno-gateway-ticket";
pub const GATEWAY_EXECUTE_PATH: &str = "internal/execution/v1/request";

/// Older checkpoints did not record display provenance. That absence may be
/// filled by the installed adapter; every execution-relevant declaration field
/// and any explicitly recorded presentation must still match.
pub(crate) fn definition_matches(
    locked: &zuno_tool::ToolDefinition,
    current: &zuno_tool::ToolDefinition,
) -> bool {
    if locked.presentation != Default::default() {
        return locked == current;
    }
    let mut expected = locked.clone();
    expected.presentation = current.presentation.clone();
    expected == *current
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssuedGatewayRequest {
    pub assignment: zuno_application::environment::wire::GatewayAssignment,
    pub ticket: zuno_identity::gateway::GatewayTicket,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRequest {
    pub version: u32,
    pub worker: WorkerInstanceId,
    pub configurations: Vec<ConfigurationRef>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantedJob {
    pub job: RuntimeJob,
    pub input: zuno_application::runtime::JobInput,
    pub lease: ExecutionLease,
    pub grant: JobGrantToken,
    /// Conservative remaining lifetime at the server's response boundary.
    pub valid_for_ms: u32,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewedLease {
    pub lease: ExecutionLease,
    pub grant: JobGrantToken,
    pub valid_for_ms: u32,
}

struct WorkerCredential {
    lease: ExecutionLease,
    grant: JobGrantToken,
    deadline: tokio::time::Instant,
    boundary_committed: bool,
}

#[derive(Debug)]
pub enum LeaseRenewal {
    Renewed(Box<ExecutionLease>),
    /// This Worker received an authoritative checkpoint/finish acknowledgement.
    Released,
}

fn grant_deadline(
    started: tokio::time::Instant,
    valid_for_ms: u32,
) -> Result<tokio::time::Instant, TurnStateError> {
    if !(1..=300_000).contains(&valid_for_ms) {
        return Err(TurnStateError::InvalidData);
    }
    // Starting before the request subtracts network and server processing time.
    // Database time still fences every mutation; this is only a local stop bound.
    let deadline = started + Duration::from_millis(u64::from(valid_for_ms));
    if deadline <= tokio::time::Instant::now() {
        return Err(TurnStateError::LeaseLost);
    }
    Ok(deadline)
}

#[async_trait]
pub trait AccessTokenSource: Send + Sync {
    async fn token(&self) -> Result<Secret, TurnStateError>;
}

/// A workload-identity sidecar or operator rotates this private token file.
pub struct FileAccessTokenSource {
    path: PathBuf,
}
impl FileAccessTokenSource {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}
#[async_trait]
impl AccessTokenSource for FileAccessTokenSource {
    async fn token(&self) -> Result<Secret, TurnStateError> {
        let metadata = tokio::fs::metadata(&self.path)
            .await
            .map_err(|_| TurnStateError::Unavailable)?;
        if !metadata.is_file() || metadata.len() > 32 * 1024 {
            return Err(TurnStateError::InvalidData);
        }
        let file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|_| TurnStateError::Unavailable)?;
        let mut bytes = Vec::new();
        file.take(32 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| TurnStateError::Unavailable)?;
        if bytes.len() > 32 * 1024 {
            return Err(TurnStateError::InvalidData);
        }
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| TurnStateError::InvalidData)?
            .trim();
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(TurnStateError::InvalidData);
        }
        Ok(Secret::new(value))
    }
}

#[derive(Clone)]
pub struct WorkerClient {
    client: reqwest::Client,
    base: Url,
    tokens: Arc<dyn AccessTokenSource>,
}
impl WorkerClient {
    /// Settle a failure before the kernel could record its own outcome. Unknown
    /// effect outcomes must use Uncertain, never an optimistic success.
    pub async fn finish(
        &self,
        execution: &WorkerExecution,
        outcome: zuno_application::runtime::JobFinish,
    ) -> Result<(), TurnStateError> {
        let _gate = execution.lease_gate.lock().await;
        outcome
            .validate()
            .map_err(|_| TurnStateError::InvalidData)?;
        if matches!(
            outcome,
            zuno_application::runtime::JobFinish::Completed { .. }
        ) {
            return Err(TurnStateError::InvalidData);
        }
        let grant = execution
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        if execution.boundary_started.swap(true, Ordering::AcqRel) {
            return Err(TurnStateError::LeaseLost);
        }
        self.post(
            FINISH_PATH,
            Some(&grant),
            serde_json::to_vec(&outcome).map_err(|_| TurnStateError::InvalidData)?,
        )
        .await?;
        execution
            .credential
            .write()
            .map_err(|_| TurnStateError::InvalidData)?
            .boundary_committed = true;
        Ok(())
    }

    pub fn new(
        base: Url,
        tokens: Arc<dyn AccessTokenSource>,
        certificate: Option<reqwest::Certificate>,
    ) -> Result<Self, TurnStateError> {
        if base.scheme() != "https"
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
        {
            return Err(TurnStateError::InvalidData);
        }
        let mut builder = zuno_network::client_builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30));
        if let Some(certificate) = certificate {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build().map_err(|_| TurnStateError::Unavailable)?;
        Ok(Self {
            client,
            base,
            tokens,
        })
    }

    async fn post(
        &self,
        path: &str,
        grant: Option<&JobGrantToken>,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, TurnStateError> {
        self.post_header(
            path,
            grant.map(|grant| (GRANT_HEADER, grant.expose())),
            body,
        )
        .await
    }

    async fn post_header(
        &self,
        path: &str,
        extra: Option<(&str, &str)>,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, TurnStateError> {
        if body.len() > MAX_WORKER_FRAME_BYTES {
            return Err(TurnStateError::InvalidData);
        }
        let token = self.tokens.token().await?;
        let mut request = self
            .client
            .post(
                self.base
                    .join(path)
                    .map_err(|_| TurnStateError::InvalidData)?,
            )
            .bearer_auth(token.expose())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        if let Some((name, value)) = extra {
            let mut header =
                HeaderValue::from_str(value).map_err(|_| TurnStateError::InvalidData)?;
            header.set_sensitive(true);
            request = request.header(name, header);
        }
        // A failed POST can have committed. Never mechanically replay it.
        let response = request
            .send()
            .await
            .map_err(|_| TurnStateError::Unavailable)?;
        match response.status().as_u16() {
            200..=299 => {}
            401 | 403 => return Err(TurnStateError::Forbidden),
            409 => return Err(TurnStateError::LeaseLost),
            500..=599 => return Err(TurnStateError::Unavailable),
            _ => return Err(TurnStateError::InvalidData),
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| TurnStateError::Unavailable)?;
            if bytes.len().saturating_add(chunk.len()) > MAX_WORKER_FRAME_BYTES {
                return Err(TurnStateError::InvalidData);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    pub async fn claim(
        &self,
        worker: WorkerInstanceId,
        configurations: &[ConfigurationRef],
    ) -> Result<Option<WorkerExecution>, TurnStateError> {
        validate_configurations(configurations)?;
        let body = serde_json::to_vec(&ClaimRequest {
            version: zuno_engine::state::wire::WORKER_PROTOCOL_VERSION,
            worker: worker.clone(),
            configurations: configurations.to_vec(),
        })
        .map_err(|_| TurnStateError::InvalidData)?;
        let started = tokio::time::Instant::now();
        let bytes = self.post(CLAIM_PATH, None, body).await?;
        let granted: Option<GrantedJob> =
            serde_json::from_slice(&bytes).map_err(|_| TurnStateError::InvalidData)?;
        granted
            .map(|value| {
                if value.lease.worker != worker
                    || value.lease.owner != value.job.principal.owner()
                    || value.lease.job_id != value.job.id
                    || value.lease.session_id != value.job.session_id
                    || value.input.id != value.job.input_id
                    || !configurations.contains(&value.job.configuration)
                {
                    return Err(TurnStateError::InvalidData);
                }
                Ok(WorkerExecution {
                    job: value.job,
                    input: value.input,
                    lease_gate: Arc::new(tokio::sync::Mutex::new(())),
                    boundary_started: Arc::new(AtomicBool::new(false)),
                    credential: Arc::new(RwLock::new(WorkerCredential {
                        lease: value.lease,
                        grant: value.grant,
                        deadline: grant_deadline(started, value.valid_for_ms)?,
                        boundary_committed: false,
                    })),
                })
            })
            .transpose()
    }

    pub async fn renew(&self, execution: &WorkerExecution) -> Result<LeaseRenewal, TurnStateError> {
        // A successful boundary releases its DB lease before the response
        // reaches this process. Do not race a renewal against that final POST.
        let _gate = execution.lease_gate.lock().await;
        let (old, grant) = {
            let current = execution
                .credential
                .read()
                .map_err(|_| TurnStateError::InvalidData)?;
            if current.boundary_committed {
                return Ok(LeaseRenewal::Released);
            }
            (current.lease.clone(), current.grant.clone())
        };
        let started = tokio::time::Instant::now();
        let bytes = self.post(RENEW_PATH, Some(&grant), b"{}".to_vec()).await?;
        let next: RenewedLease =
            serde_json::from_slice(&bytes).map_err(|_| TurnStateError::InvalidData)?;
        if next.lease.owner != old.owner
            || next.lease.job_id != old.job_id
            || next.lease.session_id != old.session_id
            || next.lease.worker != old.worker
            || next.lease.attempt_id != old.attempt_id
            || next.lease.epoch != old.epoch
            || next.lease.checkpoint_version != old.checkpoint_version
            || next.lease.expires_at_ms < old.expires_at_ms
        {
            return Err(TurnStateError::InvalidData);
        }
        let deadline = grant_deadline(started, next.valid_for_ms)?;
        let mut current = execution
            .credential
            .write()
            .map_err(|_| TurnStateError::InvalidData)?;
        if next.lease.expires_at_ms >= current.lease.expires_at_ms {
            *current = WorkerCredential {
                lease: next.lease,
                grant: next.grant,
                deadline,
                boundary_committed: false,
            };
        }
        Ok(LeaseRenewal::Renewed(Box::new(current.lease.clone())))
    }

    pub fn persistence(
        &self,
        execution: &WorkerExecution,
        directory: String,
    ) -> Result<RemoteTurnPersistence, TurnStateError> {
        RemoteTurnPersistence::new(
            Arc::new(HttpStateTransport {
                client: self.clone(),
                credential: Arc::clone(&execution.credential),
                lease_gate: Arc::clone(&execution.lease_gate),
                boundary_started: Arc::clone(&execution.boundary_started),
            }),
            TurnStateScope {
                owner: execution.job.principal.owner(),
                session_id: execution.job.session_id.to_string(),
            },
            directory,
        )
    }
}

pub fn validate_configurations(configurations: &[ConfigurationRef]) -> Result<(), TurnStateError> {
    if configurations.is_empty() || configurations.len() > 64 {
        return Err(TurnStateError::InvalidData);
    }
    for (index, configuration) in configurations.iter().enumerate() {
        configuration
            .validate()
            .map_err(|_| TurnStateError::InvalidData)?;
        if configurations[..index].contains(configuration) {
            return Err(TurnStateError::InvalidData);
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct WorkerExecution {
    pub job: RuntimeJob,
    pub input: zuno_application::runtime::JobInput,
    credential: Arc<RwLock<WorkerCredential>>,
    lease_gate: Arc<tokio::sync::Mutex<()>>,
    boundary_started: Arc<AtomicBool>,
}
impl WorkerExecution {
    pub fn boundary_started(&self) -> bool {
        self.boundary_started.load(Ordering::Acquire)
    }
    pub fn deadline(&self) -> Result<tokio::time::Instant, TurnStateError> {
        self.credential
            .read()
            .map(|value| value.deadline)
            .map_err(|_| TurnStateError::InvalidData)
    }
    pub fn lease(&self) -> Result<ExecutionLease, TurnStateError> {
        self.credential
            .read()
            .map(|value| value.lease.clone())
            .map_err(|_| TurnStateError::InvalidData)
    }
}
struct HttpStateTransport {
    client: WorkerClient,
    credential: Arc<RwLock<WorkerCredential>>,
    lease_gate: Arc<tokio::sync::Mutex<()>>,
    boundary_started: Arc<AtomicBool>,
}
#[async_trait]
impl StateTransport for HttpStateTransport {
    async fn exchange(&self, request: StateRequest) -> Result<StateResponse, TurnStateError> {
        let boundary = matches!(
            &request.command,
            zuno_engine::state::wire::StateCommand::CommitAdvance { .. }
        );
        let _gate = if boundary {
            Some(self.lease_gate.lock().await)
        } else {
            None
        };
        if boundary {
            if self.boundary_started.swap(true, Ordering::AcqRel) {
                return Err(TurnStateError::LeaseLost);
            }
        } else if self.boundary_started.load(Ordering::Acquire) {
            return Err(TurnStateError::LeaseLost);
        }
        let grant = self
            .credential
            .read()
            .map_err(|_| TurnStateError::InvalidData)?
            .grant
            .clone();
        let bytes = self
            .client
            .post(STATE_PATH, Some(&grant), request.encode()?)
            .await?;
        let response = StateResponse::decode(&bytes)?;
        if boundary
            && matches!(
                &response.result,
                Ok(zuno_engine::state::wire::StateReply::Checkpoint(_))
            )
        {
            self.credential
                .write()
                .map_err(|_| TurnStateError::InvalidData)?
                .boundary_committed = true;
        }
        Ok(response)
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn response_time_is_subtracted_and_expired_responses_cannot_start_work() {
        let started = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_millis(70)).await;
        let deadline = grant_deadline(started, 100).unwrap();
        assert_eq!(
            deadline.duration_since(tokio::time::Instant::now()),
            Duration::from_millis(30)
        );
        tokio::time::advance(Duration::from_millis(30)).await;
        assert!(matches!(
            grant_deadline(started, 100),
            Err(TurnStateError::LeaseLost)
        ));
        for invalid in [0, 300_001, u32::MAX] {
            assert!(matches!(
                grant_deadline(started, invalid),
                Err(TurnStateError::InvalidData)
            ));
        }
    }
}
