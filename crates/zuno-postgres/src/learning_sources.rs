//! Durable completion wakeups and source admission facts. This module stores no
//! model prompt, holds no provider, and never waits for external work.
use crate::database_error;
use serde_json::Value;
use sqlx_core::{query::query, row::Row};
use sqlx_postgres::PgConnection;
use std::collections::BTreeSet;
use zuno_application::ApplicationError;
use zuno_types::identity::PrincipalKey;

mod backfill;
pub(crate) use backfill::backfill;

pub(crate) enum SourceDisposition {
    Captured,
    Omitted,
    Unavailable,
}
impl SourceDisposition {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Captured => "captured",
            Self::Omitted => "omitted",
            Self::Unavailable => "unavailable",
        }
    }
}

async fn database_time(connection: &mut PgConnection) -> Result<i64, ApplicationError> {
    sqlx_core::query_scalar::query_scalar(
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .fetch_one(connection)
    .await
    .map_err(database_error)
}

/// Called inside runtime completion. This takes no Memory/parent-session lock;
/// a concurrent scan acknowledges only the version it actually observed.
pub(crate) async fn completed(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    job: &str,
) -> Result<(), ApplicationError> {
    let mut current = job.to_owned();
    let mut seen = BTreeSet::new();
    loop {
        if seen.len() > 16 || !seen.insert(current.clone()) {
            return Err(ApplicationError::Conflict);
        }
        let parent = query(
            "SELECT parent_job_id FROM zuno_enterprise_preview.runtime_child
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(&current)
        .fetch_optional(&mut *connection)
        .await
        .map_err(database_error)?;
        match parent {
            Some(parent) => current = parent.try_get("parent_job_id").map_err(database_error)?,
            None => break,
        }
    }
    let now = database_time(connection).await?;
    query("INSERT INTO zuno_enterprise_preview.learning_root_scan
        (tenant_id,principal_id,root_job_id,updated_at)
        SELECT r.tenant_id,r.principal_id,r.job_id,$4 FROM zuno_enterprise_preview.runtime_job r
        JOIN zuno_enterprise_preview.session s ON s.tenant_id=r.tenant_id AND s.principal_id=r.principal_id AND s.id=r.session_id
        WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.job_id=$3 AND s.parent_id IS NULL
        ON CONFLICT(tenant_id,principal_id,root_job_id) DO UPDATE SET
          source_version=learning_root_scan.source_version+1,updated_at=excluded.updated_at")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(current).bind(now)
        .execute(connection).await.map_err(database_error)?;
    Ok(())
}

pub(crate) async fn claim(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    root: &str,
    origin: &Value,
    digest: Option<&str>,
    disposition: SourceDisposition,
    execution: Option<&str>,
) -> Result<(), ApplicationError> {
    let now = database_time(connection).await?;
    query("INSERT INTO zuno_enterprise_preview.learning_source_claim
        (tenant_id,principal_id,root_job_id,origin,source_digest,disposition,learning_job_id,created_at)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8)
        ON CONFLICT(tenant_id,principal_id,root_job_id,origin) DO NOTHING")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(root)
        .bind(origin).bind(digest).bind(disposition.as_str()).bind(execution).bind(now)
        .execute(connection).await.map_err(database_error)?;
    Ok(())
}
