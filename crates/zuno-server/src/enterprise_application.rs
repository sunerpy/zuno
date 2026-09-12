//! Client application routes. No Worker grants, checkpoints or private replay
//! material are serialized by this module.

use crate::enterprise_browser::EnterpriseBrowser;
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};
use zuno_application::api::{
    ApprovalDecision, ApprovalView, InputVersionView, JobView, JobWaitView, SubmitTurn,
    WorkspaceView,
};
use zuno_application::{
    AgentApplication, ApplicationError, CreateSession, PageSize, SessionCursor, SessionPage,
    SessionPageRequest, SessionSummary,
    activity::{ActivityPersistence, FrameQuery, HistoryQuery},
    authorization::{AnswerApproval, OrganizationStore},
    control::{CancelJob, CancellationReceipt, RuntimeControl},
    runtime::{ConfigurationRef, JobDispatcher, JobInputSelection, JobSubmission},
};
use zuno_identity::{AccessTokenVerifier, IdentityError, VerifiedIdentity, VerifiedIdentityKind};
use zuno_memory::remote::{MemoryDataService, MemoryRequest, MemoryResponse};
use zuno_permission::enterprise::actor_denial;
use zuno_postgres::PostgresBackend;
use zuno_postgres::PostgresMemoryBackend;
use zuno_types::identity::{
    ApprovalId, JobId, PrincipalKey, PrincipalScope, SessionId, TenantId, WorkspaceId,
};

pub const API_PREFIX: &str = "/api/v1";
pub const BROWSER_API_PREFIX: &str = "/app/api/v1";

/// Operator-owned logical workspace/profile mapping; never a host directory.
#[derive(Clone)]
pub struct ApplicationWorkspace {
    pub id: WorkspaceId,
    pub title: String,
    pub configuration: ConfigurationRef,
    pub selection: JobInputSelection,
}

#[derive(Clone)]
pub struct EnterpriseApplication {
    backend: PostgresBackend,
    tenant: TenantId,
    workspaces: Arc<BTreeMap<WorkspaceId, ApplicationWorkspace>>,
    memory: Option<PostgresMemoryBackend>,
    workspace_gateway: Option<Arc<crate::workspace_gateway::GatewayWorkspaceClient>>,
}

impl EnterpriseApplication {
    pub fn new(
        backend: PostgresBackend,
        tenant: TenantId,
        workspaces: Vec<ApplicationWorkspace>,
    ) -> Result<Self, ApplicationError> {
        if workspaces.is_empty() || workspaces.len() > 128 {
            return Err(ApplicationError::Invalid(
                "configure 1–128 logical workspaces".to_owned(),
            ));
        }
        let mut installed = BTreeMap::new();
        for workspace in workspaces {
            workspace.configuration.validate()?;
            workspace.selection.validate()?;
            if workspace.title.trim() != workspace.title
                || workspace.title.is_empty()
                || workspace.title.chars().count() > 256
                || workspace.title.chars().any(char::is_control)
                || installed.insert(workspace.id.clone(), workspace).is_some()
            {
                return Err(ApplicationError::Invalid(
                    "invalid or duplicate workspace".to_owned(),
                ));
            }
        }
        Ok(Self {
            backend,
            tenant,
            workspaces: Arc::new(installed),
            memory: None,
            workspace_gateway: None,
        })
    }

    pub fn with_memory(mut self, memory: PostgresMemoryBackend) -> Self {
        self.memory = Some(memory);
        self
    }
    pub fn with_workspace_gateway(
        mut self,
        reader: Arc<crate::workspace_gateway::GatewayWorkspaceClient>,
    ) -> Self {
        self.workspace_gateway = Some(reader);
        self
    }

    /// External clients supply an API access token. Cookies are not credentials
    /// on this surface; browser requests use the separate BFF surface.
    pub fn api_router(self, verifier: Arc<dyn AccessTokenVerifier>) -> Router {
        Router::new().nest(
            API_PREFIX,
            self.routes()
                .layer(middleware::from_fn_with_state(verifier, bearer_identity))
                .layer(middleware::from_fn(no_store)),
        )
    }

    pub fn browser_router(self, browser: &EnterpriseBrowser) -> Router {
        Router::new().nest(
            BROWSER_API_PREFIX,
            browser.authenticate_routes(self.routes()),
        )
    }

    fn routes(self) -> Router {
        let mut router = Router::new()
            .route("/workspaces", get(workspaces))
            .route("/sessions", post(create_session).get(list_sessions))
            .route("/sessions/{session}", get(session))
            .route("/sessions/{session}/input-version", get(input_version))
            .route("/sessions/{session}/turns", post(submit_turn))
            .route("/sessions/{session}/requests/{request}", get(submission))
            .route("/sessions/{session}/history", get(history))
            .route("/sessions/{session}/frames", get(frames))
            .route("/sessions/{session}/live", get(live_progress))
            .route("/jobs/{job}", get(job))
            .route("/jobs/{job}/workflow", get(workflow))
            .route("/jobs/{job}/cancel", post(cancel_job))
            .route("/approvals/{approval}", get(approval))
            .route("/approvals/{approval}/merge", get(merge_review))
            .route("/approvals/{approval}/answer", post(answer));
        if self.workspace_gateway.is_some() {
            router = router.route("/approvals/{approval}/merge/content", get(merge_content));
            router = router
                .route(
                    "/sessions/{session}/workspace/imports",
                    post(begin_workspace_import),
                )
                .route(
                    "/sessions/{session}/workspace/imports/{import}",
                    get(workspace_import).delete(cancel_workspace_import),
                )
                .route(
                    "/sessions/{session}/workspace/imports/{import}/archive",
                    axum::routing::put(upload_workspace),
                );
        }
        if self.memory.is_some() {
            router = router.route("/workspaces/{workspace}/memory", post(memory_request));
        }
        router
            .layer(DefaultBodyLimit::max(
                zuno_application::MAX_INPUT_BYTES + 16384,
            ))
            .with_state(self)
    }

    async fn principal(&self, identity: &VerifiedIdentity) -> Result<PrincipalScope, Failure> {
        if identity.tenant_id() != &self.tenant
            || identity.kind() != VerifiedIdentityKind::DelegatedUser
        {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        let owner = PrincipalKey {
            tenant_id: identity.tenant_id().clone(),
            principal_id: identity.principal_id().clone(),
        };
        let access = self
            .backend
            .organizations(self.tenant.clone())
            .access(&owner)
            .await
            .map_err(|error| match error {
                ApplicationError::NotFound => Failure(StatusCode::FORBIDDEN),
                other => Failure::from(other),
            })?;
        let principal = identity.attribution(access.policy.revision);
        if actor_denial(&access.policy, &access.member, &principal).is_some() {
            return Err(Failure(StatusCode::FORBIDDEN));
        }
        // Every data read/write checks again and keeps its policy/member locks
        // until commit. This preliminary lookup does not grant future access.
        Ok(principal)
    }

    fn sessions(&self, principal: PrincipalScope) -> AgentApplication {
        AgentApplication::new(Arc::new(self.backend.sessions(principal)))
    }
}

async fn begin_workspace_import(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
    Json(request): Json<zuno_application::workspace_import::BeginWorkspaceImport>,
) -> Result<Json<zuno_application::workspace_import::WorkspaceImportView>, Failure> {
    let principal = service.principal(&identity).await?;
    let summary = service
        .sessions(principal.clone())
        .session(&session)
        .await?;
    let workspace = summary
        .workspace_id
        .as_ref()
        .and_then(|id| service.workspaces.get(id))
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    let transfer = service
        .workspace_gateway
        .as_ref()
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    Ok(Json(
        transfer
            .begin_import(
                &principal,
                &session,
                workspace.configuration.clone(),
                request,
            )
            .await?,
    ))
}
async fn workspace_import(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path((session, id)): Path<(SessionId, zuno_types::identity::WorkspaceImportId)>,
) -> Result<Json<zuno_application::workspace_import::WorkspaceImportView>, Failure> {
    use zuno_application::workspace_import::WorkspaceImportStore;
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .import_view(&principal, &session, &id)
            .await?,
    ))
}
async fn cancel_workspace_import(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path((session, id)): Path<(SessionId, zuno_types::identity::WorkspaceImportId)>,
) -> Result<Json<zuno_application::workspace_import::WorkspaceImportView>, Failure> {
    use zuno_application::workspace_import::WorkspaceImportStore;
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .cancel_import(&principal, &session, &id)
            .await?,
    ))
}
async fn upload_workspace(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path((session, id)): Path<(SessionId, zuno_types::identity::WorkspaceImportId)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Result<Json<zuno_application::workspace_import::WorkspaceImportView>, Failure> {
    let principal = service.principal(&identity).await?;
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/x-tar")
    {
        return Err(Failure(StatusCode::BAD_REQUEST));
    }
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| {
            *length > 0 && *length <= zuno_application::workspace_import::MAX_IMPORT_BYTES
        })
        .ok_or(Failure(StatusCode::BAD_REQUEST))?;
    let transfer = service
        .workspace_gateway
        .as_ref()
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    Ok(Json(
        transfer
            .upload(
                &principal,
                zuno_application::workspace_import::WorkspaceUploadRequest {
                    session_id: session,
                    import_id: id,
                },
                length,
                body,
            )
            .await?,
    ))
}

async fn merge_review(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(approval): Path<ApprovalId>,
) -> Result<Json<zuno_application::workspace_merge::WorkspaceMergeView>, Failure> {
    let principal = service.principal(&identity).await?;
    let (admission, admitted) = service
        .backend
        .workspace_merge_for_approval(&principal, &approval)
        .await?;
    Ok(Json(
        zuno_application::workspace_merge::WorkspaceMergeView {
            approval_id: approval,
            operation_id: admission.operation.id,
            child_job_id: admission.operation.child_job_id,
            plan: admission.operation.plan,
            admitted,
        },
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeContentQuery {
    side: zuno_application::workspace_merge::MergeContentSide,
    path: zuno_application::workspace_merge::WorkspacePath,
}
async fn merge_content(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(approval): Path<ApprovalId>,
    Query(query): Query<MergeContentQuery>,
) -> Result<Response, Failure> {
    let principal = service.principal(&identity).await?;
    let reader = service
        .workspace_gateway
        .as_ref()
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    Ok(reader
        .content(
            &principal,
            zuno_application::workspace_merge::MergeContentRequest {
                approval_id: approval,
                side: query.side,
                path: query.path,
            },
        )
        .await?)
}

async fn memory_request(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(workspace): Path<WorkspaceId>,
    Json(request): Json<MemoryRequest>,
) -> Result<Json<MemoryResponse>, Failure> {
    let principal = service.principal(&identity).await?;
    let definition = service
        .workspaces
        .get(&workspace)
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    service
        .backend
        .register_workspace(&principal, &workspace, &definition.title)
        .await?;
    let memory = service
        .memory
        .as_ref()
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    Ok(Json(MemoryResponse {
        result: memory
            .for_user(principal, workspace)
            .request(request)
            .await
            .map_err(Into::into),
    }))
}

async fn cancel_job(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(job): Path<JobId>,
    Json(request): Json<CancelJob>,
) -> Result<Json<CancellationReceipt>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .runtime(service.tenant.clone())
            .cancel(&principal, &job, request)
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HistoryParameters {
    #[serde(default)]
    limit: PageSize,
    before: Option<zuno_types::activity::Counter>,
    through: Option<zuno_types::activity::Counter>,
}

async fn history(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
    Query(request): Query<HistoryParameters>,
) -> Result<Json<zuno_types::activity::HistoryPage>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .activity(principal)
            .history(
                &session,
                HistoryQuery {
                    limit: request.limit,
                    before: request.before,
                    through: request.through,
                },
            )
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrameParameters {
    #[serde(default)]
    limit: PageSize,
    #[serde(default)]
    after: zuno_types::activity::Counter,
}

async fn frames(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
    Query(request): Query<FrameParameters>,
) -> Result<Json<zuno_types::activity::FramePage>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .activity(principal)
            .frames(
                &session,
                FrameQuery {
                    limit: request.limit,
                    after: request.after,
                },
            )
            .await?,
    ))
}

async fn live_progress(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
) -> Result<Json<Option<zuno_types::activity::LiveFrame>>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service.backend.live_progress(&principal, &session).await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListSessions {
    pub before_updated_at: Option<i64>,
    pub before_session_id: Option<SessionId>,
    #[serde(default)]
    pub limit: PageSize,
}
impl ListSessions {
    fn page(self) -> Result<SessionPageRequest, Failure> {
        let after = match (self.before_updated_at, self.before_session_id) {
            (Some(updated_at), Some(session_id)) if updated_at >= 0 => Some(SessionCursor {
                updated_at,
                session_id,
            }),
            (None, None) => None,
            _ => return Err(Failure(StatusCode::BAD_REQUEST)),
        };
        Ok(SessionPageRequest {
            after,
            limit: self.limit,
        })
    }
}

pub fn job_view(state: zuno_postgres::ClientJobState) -> JobView {
    let mut view = JobView::from(state.job);
    view.stop_requested = state.stop_requested;
    view.pending_operations = state.pending_operations;
    view.waits = state
        .waits
        .into_iter()
        .map(|wait| JobWaitView {
            invocation_id: wait.invocation_id,
            target: wait.target,
        })
        .collect();
    view
}

struct Failure(StatusCode);
impl From<ApplicationError> for Failure {
    fn from(error: ApplicationError) -> Self {
        Self(match error {
            ApplicationError::Invalid(_) => StatusCode::BAD_REQUEST,
            ApplicationError::NotFound => StatusCode::NOT_FOUND,
            ApplicationError::Forbidden => StatusCode::FORBIDDEN,
            ApplicationError::Conflict | ApplicationError::LeaseLost => StatusCode::CONFLICT,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        })
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let error = match self.0 {
            StatusCode::BAD_REQUEST => "invalid_request",
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            _ => "unavailable",
        };
        (self.0, Json(serde_json::json!({"error":error}))).into_response()
    }
}

async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn bearer_identity(
    State(verifier): State<Arc<dyn AccessTokenVerifier>>,
    mut request: Request,
    next: Next,
) -> Result<Response, Failure> {
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let token = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty() && value.len() <= 32768)
        .ok_or(Failure(StatusCode::UNAUTHORIZED))?;
    if values.next().is_some() {
        return Err(Failure(StatusCode::UNAUTHORIZED));
    }
    let identity = verifier.verify(token).await.map_err(|error| {
        Failure(match error {
            IdentityError::KeysUnavailable | IdentityError::IntrospectionUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::UNAUTHORIZED,
        })
    })?;
    request.extensions_mut().insert(identity);
    Ok(next.run(request).await)
}

async fn workspaces(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
) -> Result<Json<Vec<WorkspaceView>>, Failure> {
    service.principal(&identity).await?;
    Ok(Json(
        service
            .workspaces
            .values()
            .map(|workspace| WorkspaceView {
                id: workspace.id.clone(),
                title: workspace.title.clone(),
            })
            .collect(),
    ))
}
async fn create_session(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Json(request): Json<CreateSession>,
) -> Result<Json<SessionSummary>, Failure> {
    let principal = service.principal(&identity).await?;
    let workspace = service
        .workspaces
        .get(&request.workspace_id)
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    service
        .backend
        .register_workspace(&principal, &workspace.id, &workspace.title)
        .await?;
    Ok(Json(
        service.sessions(principal).create_session(request).await?,
    ))
}
async fn list_sessions(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Query(request): Query<ListSessions>,
) -> Result<Json<SessionPage>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .sessions(principal)
            .sessions(request.page()?)
            .await?,
    ))
}
async fn session(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
) -> Result<Json<SessionSummary>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(service.sessions(principal).session(&session).await?))
}
async fn input_version(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
) -> Result<Json<InputVersionView>, Failure> {
    let principal = service.principal(&identity).await?;
    let version = service
        .backend
        .client_input_version(&principal, &session)
        .await?;
    Ok(Json(InputVersionView {
        version: version.to_string(),
    }))
}
async fn submit_turn(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(session): Path<SessionId>,
    Json(request): Json<SubmitTurn>,
) -> Result<Json<JobView>, Failure> {
    let principal = service.principal(&identity).await?;
    let version = request
        .expected_input_version
        .parse::<u64>()
        .ok()
        .filter(|value| value.to_string() == request.expected_input_version)
        .ok_or(Failure(StatusCode::BAD_REQUEST))?;
    let summary = service
        .sessions(principal.clone())
        .session(&session)
        .await?;
    let workspace = summary
        .workspace_id
        .as_ref()
        .and_then(|id| service.workspaces.get(id))
        .ok_or(Failure(StatusCode::NOT_FOUND))?;
    let dispatcher = JobDispatcher::new(Arc::new(service.backend.runtime(service.tenant)));
    Ok(Json(
        dispatcher
            .dispatch(
                &principal,
                JobSubmission {
                    session_id: session,
                    request_id: request.request_id,
                    expected_input_version: version,
                    text: request.text,
                    configuration: workspace.configuration.clone(),
                    selection: Some(workspace.selection.clone()),
                },
            )
            .await?
            .into(),
    ))
}
async fn job(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(job): Path<JobId>,
) -> Result<Json<JobView>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(job_view(
        service.backend.client_job(&principal, &job).await?,
    )))
}
async fn submission(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path((session, request)): Path<(SessionId, zuno_types::identity::RequestId)>,
) -> Result<Json<JobView>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(job_view(
        service
            .backend
            .client_submission(&principal, &session, &request)
            .await?,
    )))
}
async fn workflow(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(job): Path<JobId>,
) -> Result<Json<zuno_application::workflow::WorkflowRunView>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service.backend.client_workflow(&principal, &job).await?,
    ))
}
async fn approval(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(approval): Path<ApprovalId>,
) -> Result<Json<ApprovalView>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant)
            .approval(&principal, &approval)
            .await?
            .into(),
    ))
}
async fn answer(
    State(service): State<EnterpriseApplication>,
    Extension(identity): Extension<VerifiedIdentity>,
    Path(approval): Path<ApprovalId>,
    Json(request): Json<ApprovalDecision>,
) -> Result<Json<ApprovalView>, Failure> {
    let principal = service.principal(&identity).await?;
    Ok(Json(
        service
            .backend
            .organizations(service.tenant)
            .answer(
                &principal,
                AnswerApproval {
                    request_id: request.request_id,
                    approval_id: approval,
                    answer: request.answer,
                },
            )
            .await?
            .into(),
    ))
}
