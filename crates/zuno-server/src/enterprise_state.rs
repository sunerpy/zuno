//! Authenticated Worker state routes. Public client routes use a separate DTO/API.

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use zuno_application::runtime::{JobFinish, LeaseDuration, RuntimeStore};
use zuno_engine::state::wire::{MAX_WORKER_FRAME_BYTES, StateRequest, StateResponse, execute};
use zuno_engine::state::{TurnPersistence, TurnStateError, TurnStateScope};
use zuno_identity::worker::{
    AuthenticatedWorker, JobGrantAuthority, JobGrantToken, VerifiedJobGrant, WorkerAuthError,
    WorkerAuthority,
};
use zuno_memory::remote::{MemoryDataService, MemoryRequest, MemoryResponse};
use zuno_postgres::PostgresBackend;
use zuno_postgres::PostgresMemoryBackend;
use zuno_types::identity::TenantId;
use zuno_worker::{
    CLAIM_PATH, ClaimRequest, FINISH_PATH, GRANT_HEADER, GrantedJob, RENEW_PATH, RenewedLease,
    STATE_PATH,
};

#[derive(Clone)]
pub struct WorkerStateService {
    backend: PostgresBackend,
    workers: Arc<WorkerAuthority>,
    grants: Arc<JobGrantAuthority>,
    tenant: TenantId,
    lease_duration: LeaseDuration,
    memory: Option<PostgresMemoryBackend>,
}
impl WorkerStateService {
    pub fn new(
        backend: PostgresBackend,
        workers: Arc<WorkerAuthority>,
        grants: Arc<JobGrantAuthority>,
        tenant: TenantId,
        lease_duration: LeaseDuration,
    ) -> Self {
        Self {
            backend,
            workers,
            grants,
            tenant,
            lease_duration,
            memory: None,
        }
    }

    pub fn with_memory(mut self, memory: PostgresMemoryBackend) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn router(self) -> Router {
        let mut router = Router::new()
            .route(&format!("/{CLAIM_PATH}"), post(claim))
            .route(&format!("/{RENEW_PATH}"), post(renew))
            .route(&format!("/{STATE_PATH}"), post(state_call))
            .route(&format!("/{FINISH_PATH}"), post(finish));
        if self.memory.is_some() {
            router = router.route(&format!("/{}", zuno_worker::MEMORY_PATH), post(memory_call));
        }
        router
            .layer(DefaultBodyLimit::max(MAX_WORKER_FRAME_BYTES))
            .route_layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .with_state(self)
    }

    fn grant(
        &self,
        worker: &AuthenticatedWorker,
        headers: &HeaderMap,
    ) -> Result<VerifiedJobGrant, ApiFailure> {
        let values = headers.get_all(GRANT_HEADER);
        if values.iter().count() != 1 {
            return Err(ApiFailure(StatusCode::UNAUTHORIZED));
        }
        let raw = values
            .iter()
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or(ApiFailure(StatusCode::UNAUTHORIZED))?;
        let token = JobGrantToken::try_from(raw.to_owned()).map_err(auth_error)?;
        self.grants
            .verify(worker, &token, now_ms()?)
            .map_err(auth_error)
    }
}

async fn memory_call(
    State(service): State<WorkerStateService>,
    Extension(worker): Extension<AuthenticatedWorker>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Json<MemoryResponse>, ApiFailure> {
    let grant = service.grant(&worker, &headers)?;
    if bytes.len() > 65_536 {
        return Err(ApiFailure(StatusCode::PAYLOAD_TOO_LARGE));
    }
    let request: MemoryRequest =
        serde_json::from_slice(&bytes).map_err(|_| ApiFailure(StatusCode::BAD_REQUEST))?;
    let memory = service
        .memory
        .as_ref()
        .ok_or(ApiFailure(StatusCode::NOT_FOUND))?;
    Ok(Json(MemoryResponse {
        result: memory
            .for_worker(grant.lease().clone())
            .request(request)
            .await
            .map_err(Into::into),
    }))
}

struct ApiFailure(StatusCode);
impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let code = match self.0 {
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::CONFLICT => "lease_lost",
            StatusCode::SERVICE_UNAVAILABLE => "unavailable",
            _ => "invalid_request",
        };
        (self.0, Json(serde_json::json!({"error":code}))).into_response()
    }
}
fn now_ms() -> Result<i64, ApiFailure> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| i64::try_from(time.as_millis()).ok())
        .ok_or(ApiFailure(StatusCode::SERVICE_UNAVAILABLE))
}
fn auth_error(error: WorkerAuthError) -> ApiFailure {
    use zuno_identity::IdentityError;
    ApiFailure(match error {
        WorkerAuthError::Identity(
            IdentityError::KeysUnavailable | IdentityError::IntrospectionUnavailable,
        ) => StatusCode::SERVICE_UNAVAILABLE,
        WorkerAuthError::Denied => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    })
}
fn state_error(error: zuno_engine::r#loop::TurnError) -> ApiFailure {
    ApiFailure(match error {
        zuno_engine::r#loop::TurnError::State(TurnStateError::Forbidden) => StatusCode::FORBIDDEN,
        zuno_engine::r#loop::TurnError::State(TurnStateError::LeaseLost) => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    })
}

async fn authenticate(
    State(service): State<WorkerStateService>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiFailure> {
    let values = request.headers().get_all(AUTHORIZATION);
    if values.iter().count() != 1 {
        return Err(ApiFailure(StatusCode::UNAUTHORIZED));
    }
    let token = values
        .iter()
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty() && value.len() <= 32768)
        .ok_or(ApiFailure(StatusCode::UNAUTHORIZED))?;
    let worker = service
        .workers
        .authenticate(token)
        .await
        .map_err(auth_error)?;
    if worker.subject().tenant_id != service.tenant {
        return Err(ApiFailure(StatusCode::FORBIDDEN));
    }
    request.extensions_mut().insert(worker);
    Ok(next.run(request).await)
}

async fn claim(
    State(service): State<WorkerStateService>,
    Extension(worker): Extension<AuthenticatedWorker>,
    Json(request): Json<ClaimRequest>,
) -> Result<Json<Option<GrantedJob>>, ApiFailure> {
    if request.version != zuno_engine::state::wire::WORKER_PROTOCOL_VERSION {
        return Err(ApiFailure(StatusCode::BAD_REQUEST));
    }
    if worker.expires_at_ms().saturating_sub(now_ms()?) < 1000 {
        return Err(ApiFailure(StatusCode::UNAUTHORIZED));
    }
    let runtime = service
        .backend
        .runtime(service.tenant.clone())
        .with_configurations(&request.configurations)
        .map_err(|_| ApiFailure(StatusCode::BAD_REQUEST))?;
    let Some(claimed) = runtime
        .claim(&request.worker, service.lease_duration)
        .await
        .map_err(|_| ApiFailure(StatusCode::SERVICE_UNAVAILABLE))?
    else {
        return Ok(Json(None));
    };
    let view = service.backend.worker_state(claimed.lease.clone());
    let scope = TurnStateScope {
        owner: claimed.lease.owner.clone(),
        session_id: claimed.lease.session_id.to_string(),
    };
    if let Err(error) = view.clock(&scope).await {
        if matches!(
            error,
            zuno_engine::r#loop::TurnError::State(
                TurnStateError::Forbidden | TurnStateError::NotFound
            )
        ) {
            runtime
                .finish(
                    &claimed.lease,
                    JobFinish::Failed {
                        code: "organization_authorization_denied".to_owned(),
                    },
                )
                .await
                .map_err(|_| ApiFailure(StatusCode::SERVICE_UNAVAILABLE))?;
        }
        return Err(state_error(error));
    }
    let input = view.primary_input().await.map_err(state_error)?;
    let issued_at = now_ms()?;
    let grant = service
        .grants
        .issue(&worker, &claimed.lease, issued_at)
        .map_err(auth_error)?;
    let valid_for_ms =
        u32::try_from(claimed.lease.expires_at_ms.min(worker.expires_at_ms()) - issued_at)
            .map_err(|_| ApiFailure(StatusCode::SERVICE_UNAVAILABLE))?;
    Ok(Json(Some(GrantedJob {
        job: claimed.job,
        input,
        lease: claimed.lease,
        grant,
        valid_for_ms,
    })))
}

async fn renew(
    State(service): State<WorkerStateService>,
    Extension(worker): Extension<AuthenticatedWorker>,
    headers: HeaderMap,
) -> Result<Json<RenewedLease>, ApiFailure> {
    let grant = service.grant(&worker, &headers)?;
    let view = service.backend.worker_state(grant.lease().clone());
    let lease = view
        .renew_authorized(service.lease_duration)
        .await
        .map_err(state_error)?;
    let issued_at = now_ms()?;
    let grant = service
        .grants
        .issue(&worker, &lease, issued_at)
        .map_err(auth_error)?;
    let valid_for_ms = u32::try_from(lease.expires_at_ms.min(worker.expires_at_ms()) - issued_at)
        .map_err(|_| ApiFailure(StatusCode::SERVICE_UNAVAILABLE))?;
    Ok(Json(RenewedLease {
        lease,
        grant,
        valid_for_ms,
    }))
}

async fn state_call(
    State(service): State<WorkerStateService>,
    Extension(worker): Extension<AuthenticatedWorker>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<Response, ApiFailure> {
    let grant = service.grant(&worker, &headers)?;
    let request = StateRequest::decode(&bytes).map_err(|_| ApiFailure(StatusCode::BAD_REQUEST))?;
    let scope = TurnStateScope {
        owner: grant.lease().owner.clone(),
        session_id: grant.lease().session_id.to_string(),
    };
    let view = service.backend.worker_state(grant.lease().clone());
    let response = StateResponse::new(execute(&view, &scope, request.command).await)
        .encode()
        .map_err(|_| ApiFailure(StatusCode::PAYLOAD_TOO_LARGE))?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        response,
    )
        .into_response())
}

async fn finish(
    State(service): State<WorkerStateService>,
    Extension(worker): Extension<AuthenticatedWorker>,
    headers: HeaderMap,
    Json(outcome): Json<JobFinish>,
) -> Result<StatusCode, ApiFailure> {
    let grant = service.grant(&worker, &headers)?;
    // Successful kernel settlement already has a stronger atomic checkpoint
    // path. This endpoint must not manufacture success without that evidence.
    if matches!(outcome, JobFinish::Completed { .. }) {
        return Err(ApiFailure(StatusCode::BAD_REQUEST));
    }
    service
        .backend
        .runtime(service.tenant)
        .finish(grant.lease(), outcome)
        .await
        .map_err(|error| {
            ApiFailure(match error {
                zuno_application::ApplicationError::LeaseLost => StatusCode::CONFLICT,
                zuno_application::ApplicationError::Forbidden => StatusCode::FORBIDDEN,
                zuno_application::ApplicationError::Invalid(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            })
        })?;
    Ok(StatusCode::NO_CONTENT)
}
