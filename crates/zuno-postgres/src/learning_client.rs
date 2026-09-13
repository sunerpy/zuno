//! Public learning reads and user cancellation. Private execution snapshots and
//! provider records are never returned through these DTOs.
use crate::{PostgresBackend, database_error, database_time, scoped_transaction};
use serde_json::json;
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use sqlx_postgres::{PgConnection, PgRow};
use zuno_application::{ApplicationError, learning_api::*};
use zuno_types::{
    activity::{
        BackgroundKind, BackgroundProgress, Counter, ItemRecord, SessionItem, UiAction, WorkState,
    },
    identity::{
        JobId, PrincipalId, PrincipalKey, PrincipalKind, PrincipalScope, SessionId, TenantId,
        WorkspaceId,
    },
};

const SELECT_VIEW:&str="SELECT e.job_id,e.source_job_id,e.phase,e.attempt,e.charged_tokens,e.reserved_tokens,e.created_at,e.ready_at,e.deadline_at,
    e.limits->>'totalTokens' AS token_limit,j.workspace_id,j.session_id,j.status,j.time_updated,
    COALESCE(j.result->>'reason',j.result->>'code') AS reason,j.result->>'detail' AS detail,
    (SELECT count(*) FROM zuno_enterprise_preview.learning_model_request r
      WHERE r.tenant_id=e.tenant_id AND r.principal_id=e.principal_id AND r.job_id=e.job_id) AS model_requests,
    (SELECT count(*) FROM zuno_enterprise_preview.learning_model_request r
      WHERE r.tenant_id=e.tenant_id AND r.principal_id=e.principal_id AND r.job_id=e.job_id
      AND (r.state IN('prepared','unknown') OR r.outcome->'event'->'usage'->>'accounted'='false')) AS unconfirmed_requests
    FROM zuno_enterprise_preview.learning_execution e JOIN zuno_enterprise_preview.learning_job j
      ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id";

fn counter(value: i64) -> Result<Counter, ApplicationError> {
    u64::try_from(value)
        .map(Counter)
        .map_err(ApplicationError::storage)
}
fn view(row: &PgRow) -> Result<LearningJobView, ApplicationError> {
    let reason: Option<String> = row.try_get("reason").map_err(database_error)?;
    let state = match row
        .try_get::<String, _>("status")
        .map_err(database_error)?
        .as_str()
    {
        "queued" => LearningState::Queued,
        "running" => LearningState::Running,
        "completed" => LearningState::Completed,
        "skipped" if reason.as_deref() == Some("user_cancelled") => LearningState::Cancelled,
        "skipped" => LearningState::Skipped,
        "failed" => LearningState::Failed,
        "uncertain" => LearningState::Uncertain,
        _ => return Err(ApplicationError::Conflict),
    };
    let stage = match row
        .try_get::<String, _>("phase")
        .map_err(database_error)?
        .as_str()
    {
        "extraction" => LearningStage::Extraction,
        "maintenance" => LearningStage::Maintenance,
        _ => return Err(ApplicationError::Conflict),
    };
    let raw: Option<String> = row.try_get("detail").map_err(database_error)?;
    let failure = reason
        .filter(|_| !matches!(state, LearningState::Completed | LearningState::Cancelled))
        .map(|code| LearningFailureView {
            code: code
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(128)
                .collect(),
            message: raw.map(|value| zuno_error::ProviderError::sanitize_diagnostic(&value, &[])),
        });
    let limit: String = row.try_get("token_limit").map_err(database_error)?;
    let budget = LearningBudgetView {
        limit: Counter(limit.parse().map_err(ApplicationError::storage)?),
        charged: counter(row.try_get("charged_tokens").map_err(database_error)?)?,
        reserved: counter(row.try_get("reserved_tokens").map_err(database_error)?)?,
        model_requests: counter(row.try_get("model_requests").map_err(database_error)?)?,
        unconfirmed_requests: counter(
            row.try_get("unconfirmed_requests")
                .map_err(database_error)?,
        )?,
    };
    Ok(LearningJobView {
        id: JobId::new(row.try_get::<String, _>("job_id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?,
        workspace_id: WorkspaceId::new(
            row.try_get::<String, _>("workspace_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        session_id: SessionId::new(
            row.try_get::<String, _>("session_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        source_job_id: JobId::new(
            row.try_get::<String, _>("source_job_id")
                .map_err(database_error)?,
        )
        .map_err(ApplicationError::storage)?,
        stage,
        state,
        attempts: counter(row.try_get("attempt").map_err(database_error)?)?,
        budget,
        created_at_ms: counter(row.try_get("created_at").map_err(database_error)?)?,
        updated_at_ms: counter(row.try_get("time_updated").map_err(database_error)?)?,
        ready_at_ms: if state == LearningState::Queued {
            Some(counter(row.try_get("ready_at").map_err(database_error)?)?)
        } else {
            None
        },
        deadline_at_ms: row
            .try_get::<Option<i64>, _>("deadline_at")
            .map_err(database_error)?
            .map(counter)
            .transpose()?,
        failure,
        can_cancel: matches!(state, LearningState::Queued | LearningState::Running),
    })
}
async fn read_in(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    job: &JobId,
) -> Result<LearningJobView, ApplicationError> {
    let sql = sqlx_core::sql_str::AssertSqlSafe(format!(
        "{SELECT_VIEW} WHERE e.tenant_id=$1 AND e.principal_id=$2 AND e.job_id=$3"
    ));
    let row = query(sql)
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(job.as_str())
        .fetch_optional(connection)
        .await
        .map_err(database_error)?
        .ok_or(ApplicationError::NotFound)?;
    view(&row)
}

impl PostgresBackend {
    pub async fn client_learning_job(
        &self,
        principal: &PrincipalScope,
        job: &JobId,
    ) -> Result<LearningJobView, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let value = read_in(&mut tx, &principal.owner(), job).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    pub async fn client_learning_jobs(
        &self,
        principal: &PrincipalScope,
        workspace: &WorkspaceId,
        request: LearningPageRequest,
    ) -> Result<LearningPage, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let present: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.workspace
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3)",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(workspace.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if !present {
            return Err(ApplicationError::NotFound);
        }
        let before_time = request
            .before
            .as_ref()
            .map(|cursor| i64::try_from(cursor.created_at_ms.0))
            .transpose()
            .map_err(ApplicationError::storage)?;
        let stage = request.stage.map(|stage| match stage {
            LearningStage::Extraction => "extraction",
            LearningStage::Maintenance => "maintenance",
        });
        let state = request.state.map(|state| match state {
            LearningState::Queued => "queued",
            LearningState::Running => "running",
            LearningState::Completed => "completed",
            LearningState::Skipped => "skipped",
            LearningState::Failed => "failed",
            LearningState::Uncertain => "uncertain",
            LearningState::Cancelled => "cancelled",
        });
        let sql=sqlx_core::sql_str::AssertSqlSafe(format!("{SELECT_VIEW}
            WHERE e.tenant_id=$1 AND e.principal_id=$2 AND j.workspace_id=$3
              AND ($4::bigint IS NULL OR (e.created_at,e.job_id)<($4,$5))
              AND ($6::text IS NULL OR e.phase=$6)
              AND ($7::text IS NULL OR CASE WHEN j.status='skipped' AND j.result->>'reason'='user_cancelled' THEN 'cancelled' ELSE j.status END=$7)
            ORDER BY e.created_at DESC,e.job_id DESC LIMIT $8"));
        let rows = query(sql)
            .bind(principal.tenant_id().as_str())
            .bind(principal.principal_id().as_str())
            .bind(workspace.as_str())
            .bind(before_time)
            .bind(request.before.as_ref().map(|cursor| cursor.job_id.as_str()))
            .bind(stage)
            .bind(state)
            .bind(i64::from(request.limit.get()) + 1)
            .fetch_all(&mut *tx)
            .await
            .map_err(database_error)?;
        let more = rows.len() > usize::from(request.limit.get());
        let items = rows
            .into_iter()
            .take(request.limit.get() as usize)
            .map(|row| view(&row))
            .collect::<Result<Vec<_>, _>>()?;
        let before = if more {
            items.last().map(|job| LearningCursor {
                created_at_ms: job.created_at_ms,
                job_id: job.id.clone(),
            })
        } else {
            None
        };
        tx.commit().await.map_err(database_error)?;
        Ok(LearningPage { items, before })
    }
    pub async fn cancel_learning_job(
        &self,
        principal: &PrincipalScope,
        job: &JobId,
        request: CancelLearning,
    ) -> Result<LearningCancellation, ApplicationError> {
        if principal.kind() != PrincipalKind::User {
            return Err(ApplicationError::Forbidden);
        }
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let owner = principal.owner();
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(zuno_orchestration::sha256_json(&json!(["memory", owner])))
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        let current = read_in(&mut tx, &owner, job).await?;
        let digest =
            zuno_orchestration::sha256_json(&json!([owner, principal.client_id(), job, "cancel"]));
        let prior = query(
            "SELECT request_digest,response FROM zuno_enterprise_preview.learning_control_request
            WHERE tenant_id=$1 AND principal_id=$2 AND request_id=$3",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
        if let Some(prior) = prior {
            if prior
                .try_get::<String, _>("request_digest")
                .map_err(database_error)?
                != digest
            {
                return Err(ApplicationError::Conflict);
            }
            let result = serde_json::from_value(prior.try_get("response").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
            tx.commit().await.map_err(database_error)?;
            return Ok(result);
        }
        let now = database_time(&mut tx).await?;
        if current.can_cancel {
            query("UPDATE zuno_enterprise_preview.learning_job SET status='skipped',owner_id=NULL,lease_token=NULL,lease_expires=NULL,
                result='{\"reason\":\"user_cancelled\"}',time_updated=$4
                WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND status IN('queued','running')")
                .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(job.as_str()).bind(now)
                .execute(&mut *tx).await.map_err(database_error)?;
            query(
                "UPDATE zuno_enterprise_preview.learning_model_request SET state='unknown'
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='prepared'",
            )
            .bind(principal.tenant_id().as_str())
            .bind(principal.principal_id().as_str())
            .bind(job.as_str())
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
            query("UPDATE zuno_enterprise_preview.learning_execution SET charged_tokens=charged_tokens+reserved_tokens,reserved_tokens=0
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
                .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(job.as_str())
                .execute(&mut *tx).await.map_err(database_error)?;
        }
        let result = LearningCancellation {
            request_id: request.request_id.clone(),
            job: read_in(&mut tx, &owner, job).await?,
        };
        query("INSERT INTO zuno_enterprise_preview.learning_control_request
            (tenant_id,principal_id,request_id,job_id,request_digest,response,created_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.request_id.as_str())
            .bind(job.as_str()).bind(digest).bind(json!(result)).bind(now).execute(&mut *tx).await.map_err(database_error)?;
        publish_in(&mut tx, &owner, job).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }
}

pub(crate) async fn publish_in(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    job: &JobId,
) -> Result<(), ApplicationError> {
    let view = match read_in(connection, owner, job).await {
        Ok(view) => view,
        Err(ApplicationError::NotFound) => return Ok(()), // Preserved pre-execution learning rows have no public runtime view.
        Err(error) => return Err(error),
    };
    let state = match view.state {
        LearningState::Queued => WorkState::Pending,
        LearningState::Running => WorkState::Active,
        LearningState::Completed => WorkState::Completed,
        LearningState::Skipped | LearningState::Cancelled => WorkState::Cancelled,
        LearningState::Failed => WorkState::Failed,
        LearningState::Uncertain => WorkState::Uncertain,
    };
    let mut actions = vec![UiAction::ViewLearning {
        job_id: job.clone(),
    }];
    if view.can_cancel {
        actions.push(UiAction::CancelLearning {
            job_id: job.clone(),
        });
    }
    crate::activity::publish(
        connection,
        owner,
        view.session_id.as_str(),
        ItemRecord {
            id: format!("learning:{job}"),
            parent_id: Some(format!("job:{}", view.source_job_id)),
            created_at: view.created_at_ms,
            item: SessionItem::Background {
                job_id: job.clone(),
                label: match view.stage {
                    LearningStage::Extraction => "Memory extraction",
                    LearningStage::Maintenance => "Memory maintenance",
                }
                .to_owned(),
                state,
                activity_kind: Some(match view.stage {
                    LearningStage::Extraction => BackgroundKind::MemoryExtraction,
                    LearningStage::Maintenance => BackgroundKind::MemoryMaintenance,
                }),
                progress: Some(BackgroundProgress::Learning {
                    attempts: view.attempts,
                    token_limit: view.budget.limit,
                    charged_tokens: view.budget.charged,
                    reserved_tokens: view.budget.reserved,
                    model_requests: view.budget.model_requests,
                    unconfirmed_requests: view.budget.unconfirmed_requests,
                }),
            },
            actions,
        },
    )
    .await
}

pub(crate) async fn backfill(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    // Migration runs under the schema owner in one exclusive schema transaction.
    // Enumerate identities, then restore FORCE before reading any scoped data.
    sqlx_core::raw_sql::raw_sql(
        "ALTER TABLE zuno_enterprise_preview.learning_execution NO FORCE ROW LEVEL SECURITY",
    )
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    let rows = query(
        "SELECT tenant_id,principal_id,job_id FROM zuno_enterprise_preview.learning_execution
        ORDER BY tenant_id,principal_id,created_at,job_id",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(
        "ALTER TABLE zuno_enterprise_preview.learning_execution FORCE ROW LEVEL SECURITY",
    )
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    for row in rows {
        let owner = PrincipalKey {
            tenant_id: TenantId::new(
                row.try_get::<String, _>("tenant_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
            principal_id: PrincipalId::new(
                row.try_get::<String, _>("principal_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
        };
        let id = JobId::new(row.try_get::<String, _>("job_id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
        query(
            "SELECT set_config('zuno.tenant_id',$1,true),set_config('zuno.principal_id',$2,true)",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
        publish_in(connection, &owner, &id).await?;
    }
    Ok(())
}
