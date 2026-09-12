//! Worker-side transport. No PostgreSQL dependency or database credentials.

pub mod gateway;

use async_trait::async_trait;
use futures::StreamExt as _;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::AsyncReadExt as _;
use url::Url;
use zuno_application::runtime::{ExecutionLease, RuntimeJob};
use zuno_auth::Secret;
use zuno_engine::state::remote::{RemoteTurnPersistence, StateTransport};
use zuno_engine::state::wire::{MAX_WORKER_FRAME_BYTES, StateRequest, StateResponse};
use zuno_engine::state::{TurnStateError, TurnStateScope};
use zuno_identity::worker::JobGrantToken;
use zuno_types::identity::WorkerInstanceId;

pub const CLAIM_PATH: &str = "internal/worker/v1/claim";
pub const RENEW_PATH: &str = "internal/worker/v1/renew";
pub const STATE_PATH: &str = "internal/worker/v1/state";
pub const GRANT_HEADER: &str = "x-zuno-job-grant";
pub const GATEWAY_TICKET_PATH: &str = "internal/worker/v1/gateway-ticket";
pub const GATEWAY_RESOLVE_PATH: &str = "internal/gateway/v1/resolve";
pub const GATEWAY_PREPARE_PATH: &str = "internal/gateway/v1/prepare";
pub const GATEWAY_AUTHORIZE_PATH: &str = "internal/gateway/v1/authorize";
pub const GATEWAY_COMPLETION_PATH: &str = "internal/gateway/v1/completion";
pub const GATEWAY_TICKET_HEADER: &str = "x-zuno-gateway-ticket";
pub const GATEWAY_EXECUTE_PATH: &str = "internal/execution/v1/request";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssuedGatewayRequest {
    pub assignment: zuno_application::environment::wire::GatewayAssignment,
    pub ticket: zuno_identity::gateway::GatewayTicket,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRequest {
    pub worker: WorkerInstanceId,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantedJob {
    pub job: RuntimeJob,
    pub input: zuno_application::runtime::JobInput,
    pub lease: ExecutionLease,
    pub grant: JobGrantToken,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewedLease {
    pub lease: ExecutionLease,
    pub grant: JobGrantToken,
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
    ) -> Result<Option<WorkerExecution>, TurnStateError> {
        let body = serde_json::to_vec(&ClaimRequest {
            worker: worker.clone(),
        })
        .map_err(|_| TurnStateError::InvalidData)?;
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
                {
                    return Err(TurnStateError::InvalidData);
                }
                Ok(WorkerExecution {
                    job: value.job,
                    input: value.input,
                    credential: Arc::new(RwLock::new(RenewedLease {
                        lease: value.lease,
                        grant: value.grant,
                    })),
                })
            })
            .transpose()
    }

    pub async fn renew(
        &self,
        execution: &WorkerExecution,
    ) -> Result<ExecutionLease, TurnStateError> {
        let (old, grant) = {
            let current = execution
                .credential
                .read()
                .map_err(|_| TurnStateError::InvalidData)?;
            (current.lease.clone(), current.grant.clone())
        };
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
        let mut current = execution
            .credential
            .write()
            .map_err(|_| TurnStateError::InvalidData)?;
        if next.lease.expires_at_ms >= current.lease.expires_at_ms {
            *current = next;
        }
        Ok(current.lease.clone())
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
            }),
            TurnStateScope {
                owner: execution.job.principal.owner(),
                session_id: execution.job.session_id.to_string(),
            },
            directory,
        )
    }
}

pub struct WorkerExecution {
    pub job: RuntimeJob,
    pub input: zuno_application::runtime::JobInput,
    credential: Arc<RwLock<RenewedLease>>,
}
impl WorkerExecution {
    pub fn lease(&self) -> Result<ExecutionLease, TurnStateError> {
        self.credential
            .read()
            .map(|value| value.lease.clone())
            .map_err(|_| TurnStateError::InvalidData)
    }
}
struct HttpStateTransport {
    client: WorkerClient,
    credential: Arc<RwLock<RenewedLease>>,
}
#[async_trait]
impl StateTransport for HttpStateTransport {
    async fn exchange(&self, request: StateRequest) -> Result<StateResponse, TurnStateError> {
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
        StateResponse::decode(&bytes)
    }
}
