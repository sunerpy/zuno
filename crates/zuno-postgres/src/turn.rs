//! Fenced turn-state provider. Only the authenticated data owner holds this pool.

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
    executor_directory: String,
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
            executor_directory,
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

fn state_error(error: ApplicationError) -> TurnError {
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

fn decode_event(row: PgRow) -> Result<SessionEvent, TurnError> {
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
