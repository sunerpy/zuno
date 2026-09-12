//! Fenced turn-state provider. Only the authenticated data owner holds this pool.

mod context;
mod history;
mod journal;
mod records;
mod store;

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::{PgPool, PgRow, Postgres};
use zuno_application::ApplicationError;
use zuno_application::runtime::{ExecutionLease, RuntimeJob};
use zuno_db::assistant_commit::AssistantCommit;
use zuno_db::event_log::{NewSessionEvent, SessionEvent};
use zuno_db::message::{MessageRecord, MessageRole, MessageWithParts, PartKind, PartRecord};
use zuno_db::provider_backoff::ProviderBackoffCheckpoint;
use zuno_engine::advance::{
    AdvanceAdmission, AdvanceError, AdvanceRequest, AdvanceState, BeginAdvance, CheckpointRef,
};
use zuno_engine::r#loop::TurnError;
use zuno_engine::state::{
    DeveloperContexts, InputMaterialization, LegacyToolSchemas, ProviderEventUpdate,
    ToolPartCommitKind, TurnPersistence, TurnSession, TurnStateError, TurnStateScope,
};

use crate::{database_error, database_time, owner_transaction};

#[derive(Clone)]
pub struct PostgresTurnPersistence {
    pool: PgPool,
    lease: ExecutionLease,
    executor_directory: Option<String>,
}

impl PostgresTurnPersistence {
    pub(crate) fn new(
        pool: PgPool,
        lease: ExecutionLease,
        executor_directory: String,
    ) -> Result<Self, ApplicationError> {
        if executor_directory.trim().is_empty()
            || executor_directory.len() > 4096
            || executor_directory.contains(['\0', '\r', '\n'])
        {
            return Err(ApplicationError::Invalid(
                "invalid executor working directory".to_owned(),
            ));
        }
        Ok(Self {
            pool,
            lease,
            executor_directory: Some(executor_directory),
        })
    }

    pub(crate) fn for_worker(pool: PgPool, lease: ExecutionLease) -> Self {
        Self {
            pool,
            lease,
            executor_directory: None,
        }
    }

    /// Renewal for an authenticated Worker also checks current organization
    /// policy in the same transaction as extending the lease.
    pub async fn renew_authorized(
        &self,
        duration: zuno_application::runtime::LeaseDuration,
    ) -> Result<ExecutionLease, TurnError> {
        let scope = TurnStateScope {
            owner: self.lease.owner.clone(),
            session_id: self.lease.session_id.to_string(),
        };
        let (mut tx, _) = self.transaction(&scope).await?;
        let expires = database_time(&mut tx)
            .await
            .map_err(state_error)?
            .checked_add(i64::from(duration.milliseconds()))
            .ok_or(TurnStateError::InvalidData)?;
        let expires_at_ms=query_scalar(
            "UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=GREATEST(lease_expires,$4)
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 RETURNING lease_expires",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(expires)
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        self.commit_transaction(tx).await?;
        Ok(ExecutionLease {
            expires_at_ms,
            ..self.lease.clone()
        })
    }

    pub async fn primary_input(&self) -> Result<zuno_application::runtime::JobInput, TurnError> {
        let scope = TurnStateScope {
            owner: self.lease.owner.clone(),
            session_id: self.lease.session_id.to_string(),
        };
        let (mut tx, job) = self.transaction(&scope).await?;
        let input=query(
            "SELECT prompt,time_created FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(job.input_id.as_str())
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        let prompt: Value = input.try_get("prompt").map_err(sql_error)?;
        let created_at_ms: i64 = input.try_get("time_created").map_err(sql_error)?;
        if created_at_ms < 0 {
            return Err(TurnStateError::InvalidData.into());
        }
        if prompt.get("kind").and_then(Value::as_str) != Some("user") {
            return Err(TurnStateError::InvalidData.into());
        }
        let text = prompt
            .pointer("/prompt/text")
            .and_then(Value::as_str)
            .ok_or(TurnStateError::InvalidData)?
            .to_owned();
        let agent = prompt
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let model = prompt
            .get("model")
            .filter(|value| !value.is_null())
            .map(|model| {
                Ok::<_, TurnStateError>(zuno_application::runtime::JobInputModel {
                    provider_id: model
                        .get("providerID")
                        .or_else(|| model.get("providerId"))
                        .and_then(Value::as_str)
                        .ok_or(TurnStateError::InvalidData)?
                        .to_owned(),
                    model_id: model
                        .get("modelID")
                        .or_else(|| model.get("modelId"))
                        .and_then(Value::as_str)
                        .ok_or(TurnStateError::InvalidData)?
                        .to_owned(),
                })
            })
            .transpose()?;
        self.commit_transaction(tx).await?;
        Ok(zuno_application::runtime::JobInput {
            id: job.input_id,
            created_at_ms,
            text,
            agent,
            model,
        })
    }

    async fn transaction(
        &self,
        scope: &TurnStateScope,
    ) -> Result<(Transaction<'static, Postgres>, RuntimeJob), TurnError> {
        if scope.owner != self.lease.owner || scope.session_id != self.lease.session_id.as_str() {
            return Err(TurnStateError::NotFound.into());
        }
        let mut tx = owner_transaction(&self.pool, &scope.owner)
            .await
            .map_err(state_error)?;
        let job = crate::runtime::verify_lease(&mut tx, &self.lease)
            .await
            .map_err(state_error)?;
        let access = crate::authorization::access_in(&mut tx, &scope.owner)
            .await
            .map_err(state_error)?;
        if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, &job.principal)
            .is_some()
        {
            return Err(TurnStateError::Forbidden.into());
        }
        Ok((tx, job))
    }

    async fn commit_transaction(&self, mut tx: Transaction<'_, Postgres>) -> Result<(), TurnError> {
        // The first check admits the operation. A slow query must not turn that
        // admission into unlimited write authority after the lease expires.
        crate::runtime::verify_lease(&mut tx, &self.lease)
            .await
            .map_err(state_error)?;
        tx.commit().await.map_err(sql_error)
    }
}

pub(crate) fn state_error(error: ApplicationError) -> TurnError {
    match error {
        ApplicationError::Unavailable => TurnStateError::Unavailable,
        ApplicationError::NotFound => TurnStateError::NotFound,
        ApplicationError::Forbidden => TurnStateError::Forbidden,
        ApplicationError::LeaseLost => TurnStateError::LeaseLost,
        ApplicationError::Conflict => TurnStateError::Conflict,
        _ => TurnStateError::InvalidData,
    }
    .into()
}

fn sql_error(error: sqlx_core::Error) -> TurnError {
    state_error(database_error(error))
}

async fn event(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    draft: NewSessionEvent,
) -> Result<SessionEvent, TurnError> {
    let sequence = crate::session::emit(
        tx,
        &job.principal,
        job.session_id.as_str(),
        &draft.event_type,
        Value::Object(draft.properties.clone()),
    )
    .await
    .map_err(state_error)?;
    Ok(SessionEvent {
        id: format!(
            "evt_{}",
            zuno_orchestration::sha256_json(&json!([
                job.principal.owner(),
                job.session_id,
                sequence
            ]))
        ),
        session_id: job.session_id.to_string(),
        sequence,
        event_type: draft.event_type,
        version: 1,
        properties: draft.properties,
    })
}

pub(crate) fn decode_event(row: PgRow) -> Result<SessionEvent, TurnError> {
    let data: Value = row.try_get("data").map_err(sql_error)?;
    Ok(SessionEvent {
        id: row.try_get("id").map_err(sql_error)?,
        session_id: row.try_get("session_id").map_err(sql_error)?,
        sequence: row.try_get("sequence").map_err(sql_error)?,
        event_type: row.try_get("type").map_err(sql_error)?,
        version: u32::try_from(row.try_get::<i32, _>("version").map_err(sql_error)?)
            .map_err(|_| TurnStateError::InvalidData)?,
        properties: data
            .as_object()
            .cloned()
            .ok_or(TurnStateError::InvalidData)?,
    })
}

pub(crate) async fn reclaimable_checkpoint(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
) -> Result<bool, ApplicationError> {
    let Some(checkpoint) = &job.checkpoint else {
        return Ok(false);
    };
    if checkpoint.driver != "default"
        || checkpoint.schema_version != zuno_engine::advance::DRIVER_CHECKPOINT_VERSION
    {
        return Ok(false);
    }
    let scope = TurnStateScope {
        owner: job.principal.owner(),
        session_id: job.session_id.to_string(),
    };
    let Some(event) = journal::latest(tx, &scope)
        .await
        .map_err(ApplicationError::storage)?
    else {
        return Ok(false);
    };
    let unfinished = history::unfinished(tx, &scope)
        .await
        .map_err(ApplicationError::storage)?;
    zuno_engine::advance::reclaimable_checkpoint(
        &event,
        &scope.owner,
        &checkpoint.reference,
        &unfinished,
    )
    .map_err(ApplicationError::storage)
}
