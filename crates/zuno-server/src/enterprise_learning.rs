//! Workload-authenticated learning state. Public clients cannot claim work,
//! submit model outcomes, or acquire another user's Memory scope.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use std::sync::Arc;
use zuno_application::learning::LearningExecutionLease;
use zuno_identity::worker::{
    AuthenticatedWorker, JobGrantAuthority, LearningGrantToken, WorkerAuthority,
};
use zuno_learning::distributed::{LearningCompletion, LearningJournalRequest};
use zuno_memory::MemoryServiceError;
use zuno_postgres::PostgresLearningRuntime;
use zuno_types::identity::TenantId;
use zuno_worker::learning::*;

#[derive(Clone)]
pub struct LearningStateService {
    runtime: PostgresLearningRuntime,
    workers: Arc<WorkerAuthority>,
    grants: Arc<JobGrantAuthority>,
    tenant: TenantId,
    lease_ms: u32,
}
impl LearningStateService {
    pub fn new(
        runtime: PostgresLearningRuntime,
        workers: Arc<WorkerAuthority>,
        grants: Arc<JobGrantAuthority>,
        tenant: TenantId,
        lease_ms: u32,
    ) -> Self {
        Self {
            runtime,
            workers,
            grants,
            tenant,
            lease_ms,
        }
    }
    pub fn router(self) -> Router {
        Router::new()
            .route(&format!("/{LEARNING_CLAIM_PATH}"), post(claim))
            .route(&format!("/{LEARNING_RENEW_PATH}"), post(renew))
            .route(&format!("/{LEARNING_JOURNAL_PATH}"), post(journal))
            .route(&format!("/{LEARNING_COMPLETE_PATH}"), post(complete))
            .route(&format!("/{LEARNING_STOP_PATH}"), post(stop))
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .with_state(self)
    }
    async fn worker(&self, headers: &HeaderMap) -> Result<AuthenticatedWorker, Failure> {
        let token = field(headers, header::AUTHORIZATION.as_str())?
            .strip_prefix("Bearer ")
            .ok_or(Failure(StatusCode::UNAUTHORIZED))?;
        let worker = self
            .workers
            .authenticate(token)
            .await
            .map_err(|_| Failure(StatusCode::UNAUTHORIZED))?;
        if worker.subject().tenant_id != self.tenant {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        Ok(worker)
    }
    async fn authorize(
        &self,
        headers: &HeaderMap,
        lease: &LearningExecutionLease,
        receipt: bool,
    ) -> Result<AuthenticatedWorker, Failure> {
        let worker = self.worker(headers).await?;
        let token = LearningGrantToken::try_from(field(headers, LEARNING_GRANT_HEADER)?.to_owned())
            .map_err(|_| Failure(StatusCode::UNAUTHORIZED))?;
        let checked = if receipt {
            self.grants.verify_learning_receipt(&worker, &token, now()?)
        } else {
            self.grants.verify_learning(&worker, &token, now()?)
        }
        .map_err(|_| Failure(StatusCode::UNAUTHORIZED))?;
        if checked.lease() != lease {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        Ok(worker)
    }
}
async fn claim(
    State(service): State<LearningStateService>,
    headers: HeaderMap,
    Json(request): Json<LearningClaimRequest>,
) -> Result<Json<Option<IssuedLearning>>, Failure> {
    let worker = service.worker(&headers).await?;
    if request.version != 1 {
        return Err(Failure(StatusCode::BAD_REQUEST));
    }
    let Some(claimed) = service
        .runtime
        .claim(request.worker, request.configurations, service.lease_ms)
        .await
        .map_err(memory)?
    else {
        return Ok(Json(None));
    };
    let at = now()?;
    let grant = service
        .grants
        .issue_learning(&worker, &claimed.lease, at)
        .map_err(|_| Failure(StatusCode::FORBIDDEN))?;
    let checked = service
        .grants
        .verify_learning(&worker, &grant, at)
        .map_err(|_| Failure(StatusCode::FORBIDDEN))?;
    Ok(Json(Some(IssuedLearning {
        claimed,
        grant,
        valid_for_ms: (checked.expires_at_ms() - at) as u64,
    })))
}
async fn renew(
    State(service): State<LearningStateService>,
    headers: HeaderMap,
    Json(lease): Json<LearningExecutionLease>,
) -> Result<Json<RenewedLearning>, Failure> {
    let worker = service.authorize(&headers, &lease, false).await?;
    let lease = service
        .runtime
        .renew(lease, service.lease_ms)
        .await
        .map_err(memory)?;
    let at = now()?;
    let grant = service
        .grants
        .issue_learning(&worker, &lease, at)
        .map_err(|_| Failure(StatusCode::FORBIDDEN))?;
    let checked = service
        .grants
        .verify_learning(&worker, &grant, at)
        .map_err(|_| Failure(StatusCode::FORBIDDEN))?;
    Ok(Json(RenewedLearning {
        lease,
        grant,
        valid_for_ms: (checked.expires_at_ms() - at) as u64,
    }))
}
async fn journal(
    State(service): State<LearningStateService>,
    headers: HeaderMap,
    Json(request): Json<LearningJournalRequest>,
) -> Result<StatusCode, Failure> {
    let receipt = matches!(
        request.record.event,
        zuno_learning::LearningModelEvent::Outcome { .. }
    );
    service.authorize(&headers, &request.lease, receipt).await?;
    service.runtime.journal(request).await.map_err(memory)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn complete(
    State(service): State<LearningStateService>,
    headers: HeaderMap,
    Json(request): Json<LearningCompletion>,
) -> Result<StatusCode, Failure> {
    service.authorize(&headers, &request.lease, false).await?;
    service.runtime.complete(request).await.map_err(memory)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn stop(
    State(service): State<LearningStateService>,
    headers: HeaderMap,
    Json(request): Json<LearningStopRequest>,
) -> Result<StatusCode, Failure> {
    service.authorize(&headers, &request.lease, false).await?;
    service
        .runtime
        .stop(request.lease, request.stop)
        .await
        .map_err(memory)?;
    Ok(StatusCode::NO_CONTENT)
}
fn field<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, Failure> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .and_then(|v| v.to_str().ok())
        .ok_or(Failure(StatusCode::UNAUTHORIZED))?;
    if values.next().is_some() {
        return Err(Failure(StatusCode::UNAUTHORIZED));
    }
    Ok(value)
}
fn now() -> Result<i64, Failure> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|v| i64::try_from(v.as_millis()).ok())
        .ok_or(Failure(StatusCode::SERVICE_UNAVAILABLE))
}
struct Failure(StatusCode);
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let name = match self.0 {
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::CONFLICT => "conflict",
            StatusCode::BAD_REQUEST => "invalid_request",
            _ => "unavailable",
        };
        (
            self.0,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({"error":name})),
        )
            .into_response()
    }
}
fn memory(error: MemoryServiceError) -> Failure {
    Failure(match error {
        MemoryServiceError::Denied => StatusCode::FORBIDDEN,
        MemoryServiceError::Conflict => StatusCode::CONFLICT,
        MemoryServiceError::Invalid(_)
        | MemoryServiceError::InvalidData
        | MemoryServiceError::Resident(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    })
}
