//! One bounded data-owner transaction around the shared Memory service.
//!
//! The synchronous persistence port never runs on a Worker's async reactor.
//! All candidate/document/evidence calls in a request use the same transaction;
//! the transaction-bound provider is private and cannot escape this module.

mod candidates;
mod documents;
mod evidence;
mod maintenance;
mod persistence;
mod policy;
#[cfg(test)]
pub(crate) mod tests;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::Postgres;
use tokio::{runtime::Handle, sync::Semaphore};
use zuno_application::{ApplicationError, runtime::ExecutionLease};
use zuno_memory::{
    MemoryProposal, MemoryService, MemoryServiceError, PromotionPolicy, Scope, ScopeLimits,
    authority::{MemoryAccess, MemoryAuthority},
    persistence::MemoryPersistence,
    remote::{MemoryCommand, MemoryDataService, MemoryReply, MemoryRequest},
    service::MemoryDocumentKey,
};
use zuno_types::{
    MemoryScope, MemorySource,
    identity::{PrincipalScope, SessionId, WorkspaceId},
};

use crate::{
    PostgresBackend, database_error, database_time, owner_transaction, scoped_transaction,
};

type Error = MemoryServiceError;
type Tx = Transaction<'static, Postgres>;

#[derive(Debug, Clone, Copy)]
pub struct MemoryStoreLimits {
    pub concurrent_transactions: usize,
    pub scopes: ScopeLimits,
    pub transaction_timeout: Duration,
}

impl Default for MemoryStoreLimits {
    fn default() -> Self {
        Self {
            concurrent_transactions: 4,
            scopes: ScopeLimits::default(),
            transaction_timeout: Duration::from_secs(10),
        }
    }
}

/// Share one instance across all bindings so user count cannot multiply the
/// blocking-thread or connection budget.
#[derive(Clone)]
pub struct PostgresMemoryBackend {
    backend: PostgresBackend,
    slots: Arc<Semaphore>,
    limits: ScopeLimits,
    transaction_timeout: Duration,
}

impl PostgresMemoryBackend {
    pub fn new(backend: PostgresBackend, limits: MemoryStoreLimits) -> Result<Self, Error> {
        if !(1..=32).contains(&limits.concurrent_transactions)
            || !(Duration::from_secs(1)..=Duration::from_secs(30))
                .contains(&limits.transaction_timeout)
            || Scope::ALL
                .iter()
                .any(|scope| !(1..=131_072).contains(&limits.scopes.for_scope(*scope)))
        {
            return Err(invalid("invalid bounded Memory service limits"));
        }
        Ok(Self {
            backend,
            slots: Arc::new(Semaphore::new(limits.concurrent_transactions)),
            limits: limits.scopes,
            transaction_timeout: limits.transaction_timeout,
        })
    }

    pub fn for_user(
        &self,
        principal: PrincipalScope,
        workspace: WorkspaceId,
    ) -> PostgresMemoryService {
        PostgresMemoryService {
            backend: self.clone(),
            actor: Actor::User {
                principal,
                workspace,
            },
        }
    }

    /// Internal authenticated routing supplies this lease. It is verified again
    /// against current Job ownership, policy and database time in every request.
    pub fn for_worker(&self, lease: ExecutionLease) -> PostgresMemoryService {
        PostgresMemoryService {
            backend: self.clone(),
            actor: Actor::Model { lease },
        }
    }
}

#[derive(Clone)]
enum Actor {
    User {
        principal: PrincipalScope,
        workspace: WorkspaceId,
    },
    Model {
        lease: ExecutionLease,
    },
}

#[derive(Clone)]
pub struct PostgresMemoryService {
    backend: PostgresMemoryBackend,
    actor: Actor,
}

struct TransactionMemory {
    transaction: Mutex<Option<Tx>>,
    runtime: Handle,
    principal: PrincipalScope,
    workspace: WorkspaceId,
    lease: Option<ExecutionLease>,
    limits: ScopeLimits,
    project_key: String,
    deadline: tokio::time::Instant,
}

#[async_trait]
impl MemoryDataService for PostgresMemoryService {
    async fn request(&self, request: MemoryRequest) -> Result<MemoryReply, Error> {
        let encoded = serde_json::to_vec(&request).map_err(decode_error)?;
        if encoded.len() > 65_536 {
            return Err(invalid("Memory request exceeds 64 KiB"));
        }
        let permit = self
            .backend
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Unavailable)?;
        let (mut tx, principal, workspace, lease) = match &self.actor {
            Actor::User {
                principal,
                workspace,
            } => (
                scoped_transaction(&self.backend.backend.pool, principal)
                    .await
                    .map_err(app_error)?,
                principal.clone(),
                workspace.clone(),
                None,
            ),
            Actor::Model { lease } => {
                let mut tx = owner_transaction(&self.backend.backend.pool, &lease.owner)
                    .await
                    .map_err(app_error)?;
                let job = crate::runtime::verify_lease(&mut tx, lease)
                    .await
                    .map_err(app_error)?;
                let access = crate::authorization::access_in(&mut tx, &lease.owner)
                    .await
                    .map_err(app_error)?;
                if zuno_permission::enterprise::actor_denial(
                    &access.policy,
                    &access.member,
                    &job.principal,
                )
                .is_some()
                {
                    return Err(Error::Denied);
                }
                let workspace: String = query_scalar(
                    "SELECT workspace_id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
                ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str())
                    .fetch_one(&mut *tx).await.map_err(sql_error)?;
                (
                    tx,
                    job.principal,
                    WorkspaceId::new(workspace).map_err(decode_error)?,
                    Some(lease.clone()),
                )
            }
        };
        let exists: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.workspace WHERE tenant_id=$1 AND principal_id=$2 AND id=$3)",
        ).bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(workspace.as_str())
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        if !exists {
            return Err(Error::Denied);
        }
        // All of this owner's global/project edits, policy changes and source
        // forgetting serialize without locking another user's Memory.
        let lock = zuno_orchestration::sha256_json(&json!(["memory", principal.owner()]));
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(lock)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let provider = Arc::new(TransactionMemory {
            transaction: Mutex::new(Some(tx)),
            runtime: Handle::current(),
            principal,
            project_key: format!("project:{}", workspace.as_str()),
            workspace,
            lease,
            limits: self.backend.limits,
            deadline: tokio::time::Instant::now() + self.backend.transaction_timeout,
        });
        tokio::task::spawn_blocking(move || {
            // Keep the permit until the transaction settles, even if the HTTP
            // request or the caller's future is dropped.
            let _permit = permit;
            let result = provider.clone().perform(request);
            if result.is_err() {
                provider.rollback()?;
            }
            result
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}

impl TransactionMemory {
    fn rollback(&self) -> Result<(), Error> {
        let tx = self
            .transaction
            .lock()
            .map_err(|_| Error::Unavailable)?
            .take();
        if let Some(tx) = tx {
            // A dropped SQLx transaction only queues ROLLBACK. Confirm it before
            // returning the Memory slot; the server's 10s statement timeout and
            // a separate bounded cleanup window also cover cancelled queries.
            self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(12), tx.rollback())
                    .await
                    .map_err(|_| Error::Unavailable)?
                    .map_err(sql_error)
            })?;
        }
        Ok(())
    }

    fn execute<T>(&self, f: impl AsyncFnOnce(&mut Tx) -> Result<T, Error>) -> Result<T, Error> {
        let mut transaction = self.transaction.lock().map_err(|_| Error::Unavailable)?;
        let tx = transaction.as_mut().ok_or(Error::Denied)?;
        self.runtime
            .block_on(tokio::time::timeout_at(self.deadline, f(tx)))
            .map_err(|_| Error::Unavailable)?
    }

    fn key(&self, key: &str) -> Result<MemoryScope, Error> {
        if key == "global" {
            Ok(MemoryScope::Global)
        } else if key == self.project_key {
            Ok(MemoryScope::Project)
        } else {
            Err(Error::Denied)
        }
    }

    fn host_only(&self) -> Result<(), Error> {
        if self.lease.is_some() {
            Err(Error::Denied)
        } else {
            Ok(())
        }
    }

    fn review_action(&self) -> Result<(), Error> {
        self.host_only()?;
        self.execute(async |tx| {
            let access = crate::authorization::access_in(tx, &self.principal.owner())
                .await
                .map_err(app_error)?;
            if self
                .principal
                .client_id()
                .is_none_or(|client| !access.policy.approval_apps.contains(client))
            {
                return Err(Error::Denied);
            }
            Ok(())
        })
    }

    fn service(self: &Arc<Self>) -> Result<MemoryService, Error> {
        MemoryService::storage_only(
            self.clone(),
            self.clone(),
            MemoryDocumentKey::new("global")?,
            MemoryDocumentKey::new(&self.project_key)?,
            self.limits,
            if self.lease.is_some() {
                PromotionPolicy::Automatic
            } else {
                PromotionPolicy::Review
            },
        )
    }

    fn perform(self: Arc<Self>, request: MemoryRequest) -> Result<MemoryReply, Error> {
        let mutating = !matches!(
            request.command,
            MemoryCommand::Read
                | MemoryCommand::ReadEntries { .. }
                | MemoryCommand::Candidates
                | MemoryCommand::Candidate { .. }
                | MemoryCommand::Policy { .. }
        );
        let attribution = if let Some(lease) = &self.lease {
            json!({"kind":"model","jobId":lease.job_id,"sessionId":lease.session_id,"clientId":self.principal.client_id()})
        } else {
            json!({"kind":"user","principal":self.principal.owner(),"clientId":self.principal.client_id()})
        };
        let digest = zuno_orchestration::sha256_json(&json!([attribution, request.command]));
        // Check current authorization even when returning a previously committed
        // idempotent write. Read replies are never cached across policy changes.
        if self.lease.is_some()
            && !matches!(
                request.command,
                MemoryCommand::Read
                    | MemoryCommand::ReadEntries { .. }
                    | MemoryCommand::Propose { .. }
            )
        {
            return Err(Error::Denied);
        }
        if mutating && let Some(lease) = &self.lease {
            let session = lease.session_id.as_str();
            self.execute(async |tx| self.require_generation(tx, Some(session)).await)?;
        }
        if mutating
            && self.lease.is_none()
            && !matches!(request.command, MemoryCommand::Propose { .. })
        {
            self.review_action()?;
        }
        let prior = if mutating {
            self.execute(async |tx| {
                let row = query(
                    "SELECT request_digest,response FROM zuno_enterprise_preview.memory_request
                    WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3 AND request_id=$4",
                )
                .bind(self.principal.tenant_id().as_str())
                .bind(self.principal.principal_id().as_str())
                .bind(self.workspace.as_str())
                .bind(request.request_id.as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(sql_error)?;
                row.map(|row| {
                    if row
                        .try_get::<String, _>("request_digest")
                        .map_err(sql_error)?
                        != digest
                    {
                        return Err(Error::Conflict);
                    }
                    serde_json::from_value(row.try_get("response").map_err(sql_error)?)
                        .map_err(decode_error)
                })
                .transpose()
            })?
        } else {
            None
        };
        let reply = match prior {
            Some(reply) => reply,
            None => {
                let reply = self.dispatch(request.command.clone())?;
                if mutating {
                    self.execute(async |tx| {
                        query("INSERT INTO zuno_enterprise_preview.memory_request(tenant_id,principal_id,workspace_id,request_id,request_digest,response)
                            VALUES($1,$2,$3,$4,$5,$6)")
                            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(self.workspace.as_str())
                            .bind(request.request_id.as_str()).bind(&digest).bind(json!(reply)).execute(&mut **tx).await.map_err(sql_error)?;
                        let now = database_time(tx).await.map_err(app_error)?;
                        query("INSERT INTO zuno_enterprise_preview.memory_audit(tenant_id,principal_id,id,workspace_id,actor,action,data,time_created)
                            VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
                            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                            .bind(uuid::Uuid::now_v7().to_string()).bind(self.workspace.as_str()).bind(&attribution)
                            .bind(json!(request.command)["kind"].as_str().ok_or_else(|| invalid("missing Memory command kind"))?)
                            .bind(json!({"requestId":request.request_id,"requestDigest":digest,"result":reply}))
                            .bind(now).execute(&mut **tx).await.map_err(sql_error)?;
                        Ok(())
                    })?;
                }
                reply
            }
        };
        let mut tx = self
            .transaction
            .lock()
            .map_err(|_| Error::Unavailable)?
            .take()
            .ok_or(Error::Denied)?;
        self.runtime
            .block_on(tokio::time::timeout_at(self.deadline, async {
                if let Some(lease) = &self.lease {
                    crate::runtime::verify_lease(&mut tx, lease)
                        .await
                        .map_err(app_error)?;
                }
                tx.commit().await.map_err(sql_error)
            }))
            .map_err(|_| Error::Unavailable)??;
        Ok(reply)
    }

    fn dispatch(self: &Arc<Self>, command: MemoryCommand) -> Result<MemoryReply, Error> {
        let service = self.service()?;
        let candidate = match command {
            MemoryCommand::Read => {
                if let Some(lease) = &self.lease {
                    let enabled = self.execute(async |tx| {
                        self.use_enabled(tx, Some(lease.session_id.as_str())).await
                    })?;
                    if !enabled {
                        return Ok(MemoryReply::Snapshot {
                            documents: Vec::new(),
                        });
                    }
                }
                return Ok(MemoryReply::Snapshot {
                    documents: service.snapshots()?,
                });
            }
            MemoryCommand::ReadEntries { query } => {
                let views = if let Some(lease) = &self.lease {
                    service.read_for_model(lease.session_id.as_str())?
                } else {
                    service.read_views()?
                };
                return Ok(MemoryReply::Entries {
                    scopes: zuno_memory::remote::read_scopes(views, query)?,
                });
            }
            MemoryCommand::Candidates => {
                return self.execute(async |tx| {
                    Ok(MemoryReply::Candidates {
                        candidates: self
                            .list_candidates(tx)
                            .await?
                            .into_iter()
                            .map(|record| self.candidate_view(record))
                            .collect(),
                    })
                });
            }
            MemoryCommand::Candidate { candidate_id } => {
                let view = self.candidate_view(service.candidate(&candidate_id)?);
                return Ok(MemoryReply::Candidate {
                    candidate: view.candidate,
                    state_digest: view.state_digest,
                });
            }
            MemoryCommand::Propose { change } => {
                if let Some(expected) = change.expected_revision
                    && service.snapshot(change.scope.into())?.revision != expected
                {
                    return Err(Error::Conflict);
                }
                let proposal = MemoryProposal {
                    scope: change.scope,
                    action: change.action,
                    content: change.content,
                    old_text: change.old_text,
                    reason: change.reason,
                    confidence: change.confidence,
                    source: if self.lease.is_some() {
                        MemorySource::Tool
                    } else {
                        MemorySource::User
                    },
                    source_session_id: self
                        .lease
                        .as_ref()
                        .map(|lease| lease.session_id.to_string()),
                    source_message_id: None,
                };
                if let Some(lease) = &self.lease {
                    service.update_from_model(
                        proposal,
                        change.expected_revision,
                        lease.session_id.as_str(),
                    )?
                } else {
                    service.propose_for_review(proposal)?
                }
            }
            MemoryCommand::Apply {
                candidate_id,
                expected_state,
            } => {
                self.check_review_state(&service, &candidate_id, &expected_state)?;
                service.apply(&candidate_id)?
            }
            MemoryCommand::Reject {
                candidate_id,
                expected_state,
            } => {
                self.check_review_state(&service, &candidate_id, &expected_state)?;
                service.reject(&candidate_id)?
            }
            MemoryCommand::Undo {
                candidate_id,
                expected_state,
            } => {
                self.check_review_state(&service, &candidate_id, &expected_state)?;
                service.undo(&candidate_id)?
            }
            MemoryCommand::Edit {
                candidate_id,
                expected_state,
                content,
                old_text,
                reason,
            } => {
                self.check_review_state(&service, &candidate_id, &expected_state)?;
                service.edit(&candidate_id, content, old_text, reason, 1.0)?
            }
            MemoryCommand::Policy { session_id } => {
                self.host_only()?;
                return self.execute(async |tx| {
                    Ok(MemoryReply::Policy {
                        policy: self
                            .policy(tx, session_id.as_ref().map(SessionId::as_str))
                            .await?,
                    })
                });
            }
            MemoryCommand::SetPolicy {
                session_id,
                expected_revision,
                use_memories,
                generate_private,
            } => {
                self.review_action()?;
                return self.execute(async |tx| {
                    Ok(MemoryReply::Policy {
                        policy: self
                            .set_policy(
                                tx,
                                session_id.as_ref().map(SessionId::as_str),
                                expected_revision,
                                use_memories,
                                generate_private,
                            )
                            .await?,
                    })
                });
            }
            MemoryCommand::RecordEvidence { origin, excerpt } => {
                self.review_action()?;
                return self.execute(async |tx| {
                    Ok(MemoryReply::Evidence {
                        reference: self.record_evidence(tx, origin, &excerpt).await?,
                    })
                });
            }
            MemoryCommand::Forget { evidence_ids } => {
                self.host_only()?;
                if evidence_ids.is_empty() || evidence_ids.len() > 128 {
                    return Err(invalid("forget needs 1–128 evidence IDs"));
                }
                let now = self.execute(async |tx| database_time(tx).await.map_err(app_error))?;
                let result = service.forget_sources(&evidence_ids, None, now)?;
                return Ok(MemoryReply::Forgotten {
                    evidence_ids: result.forgotten_experience_ids,
                    retracted: result.retractions.len(),
                });
            }
        };
        let view = self.candidate_view(candidate);
        Ok(MemoryReply::Candidate {
            candidate: view.candidate,
            state_digest: view.state_digest,
        })
    }

    fn check_review_state(
        &self,
        service: &MemoryService,
        id: &str,
        expected: &str,
    ) -> Result<(), Error> {
        self.review_action()?;
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid(
                "expectedState must be the reviewed candidate digest",
            ));
        }
        if self.candidate_view(service.candidate(id)?).state_digest != expected {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    fn candidate_view(
        &self,
        record: zuno_db::memory_candidate::MemoryCandidateRecord,
    ) -> zuno_memory::remote::MemoryCandidateView {
        let state_digest =
            zuno_orchestration::sha256_json(&json!([self.principal.owner(), record]));
        zuno_memory::remote::MemoryCandidateView {
            candidate: record.projection,
            state_digest,
        }
    }
}

impl MemoryAuthority for TransactionMemory {
    fn authorize(&self, _scope: MemoryScope, access: MemoryAccess) -> Result<(), Error> {
        if access == MemoryAccess::Import {
            return Err(Error::Denied);
        }
        if let Some(lease) = &self.lease {
            if !matches!(
                access,
                MemoryAccess::Read | MemoryAccess::Propose | MemoryAccess::Apply
            ) {
                return Err(Error::Denied);
            }
            if access != MemoryAccess::Read {
                return self.execute(async |tx| {
                    self.require_generation(tx, Some(lease.session_id.as_str()))
                        .await
                });
            }
        } else if matches!(
            access,
            MemoryAccess::Apply
                | MemoryAccess::Reject
                | MemoryAccess::Edit
                | MemoryAccess::Undo
                | MemoryAccess::Forget
        ) {
            self.review_action()?;
        }
        Ok(())
    }
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}
fn decode_error(_error: impl std::fmt::Display) -> Error {
    Error::InvalidData
}
fn sql_error(error: sqlx_core::Error) -> Error {
    app_error(database_error(error))
}
fn app_error(error: ApplicationError) -> Error {
    match error {
        ApplicationError::Unavailable => Error::Unavailable,
        ApplicationError::Forbidden | ApplicationError::NotFound => Error::Denied,
        ApplicationError::Conflict | ApplicationError::LeaseLost => Error::Conflict,
        _ => Error::InvalidData,
    }
}
