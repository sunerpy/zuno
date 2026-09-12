//! Public activity is committed with its source, never reconstructed by copying
//! private Worker events into a client response.

mod projection;
mod reader;

use crate::{PostgresBackend, database_error};
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::PgConnection;
use zuno_application::ApplicationError;
use zuno_types::activity::*;
use zuno_types::identity::{PrincipalKey, PrincipalScope, SessionId};

#[derive(Clone)]
pub struct PostgresActivityPersistence {
    backend: PostgresBackend,
    principal: PrincipalScope,
}

impl PostgresBackend {
    pub fn activity(&self, principal: PrincipalScope) -> PostgresActivityPersistence {
        PostgresActivityPersistence {
            backend: self.clone(),
            principal,
        }
    }
}

/// Callers hold the source's session lock in this transaction. The counter is a
/// logical public sequence; it is unrelated to physical rows or private events.
pub(crate) async fn publish(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    record: ItemRecord,
) -> Result<(), ApplicationError> {
    let value = json!(record);
    let previous: Option<Value> = query_scalar(
        "SELECT record FROM zuno_enterprise_preview.activity_item
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(session)
    .bind(&record.id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(database_error)?;
    if previous.as_ref() == Some(&value) {
        return Ok(());
    }
    let sequence: i64 = query_scalar(
        "UPDATE zuno_enterprise_preview.activity_session SET sequence=sequence+1
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 RETURNING sequence",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(session)
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    query(
        "INSERT INTO zuno_enterprise_preview.activity_item(tenant_id,principal_id,session_id,id,position,revision,record)
         VALUES($1,$2,$3,$4,$5,$5,$6)
         ON CONFLICT(tenant_id,principal_id,session_id,id) DO UPDATE SET revision=excluded.revision,record=excluded.record",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(&record.id).bind(sequence).bind(&value)
        .execute(&mut *connection).await.map_err(database_error)?;
    query(
        "INSERT INTO zuno_enterprise_preview.activity_frame(tenant_id,principal_id,session_id,sequence,item_id,version,record)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(sequence).bind(&record.id)
        .bind(ACTIVITY_PROTOCOL_VERSION as i32).bind(value).execute(&mut *connection).await.map_err(database_error)?;
    Ok(())
}

fn counter(value: i64) -> Result<Counter, ApplicationError> {
    Ok(Counter(
        u64::try_from(value).map_err(ApplicationError::storage)?,
    ))
}
fn integer(value: Counter) -> Result<i64, ApplicationError> {
    i64::try_from(value.0)
        .map_err(|_| ApplicationError::Invalid("activity cursor is out of range".to_owned()))
}

pub(crate) use projection::{backfill, event, execution_changed, message, part};
