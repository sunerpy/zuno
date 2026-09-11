use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx_core::{row::Row, transaction::Transaction};
use sqlx_postgres::PgRow;
use sqlx_postgres::{PgPool, Postgres};
use zuno_application::{
    ApplicationError, CreateSession, InputReceipt, InputState, QueueText, SessionCursor,
    SessionPage, SessionPageRequest, SessionPersistence, SessionSummary,
};
use zuno_types::identity::{InputId, PrincipalScope, SessionId, WorkspaceId};

use crate::{database_error, database_time, scoped_transaction};

#[derive(Clone)]
pub struct PostgresSessionPersistence {
    pool: PgPool,
    principal: PrincipalScope,
}

impl PostgresSessionPersistence {
    pub(crate) fn new(pool: PgPool, principal: PrincipalScope) -> Self {
        Self { pool, principal }
    }
}

#[async_trait]
impl SessionPersistence for PostgresSessionPersistence {
    fn principal(&self) -> &PrincipalScope {
        &self.principal
    }

    async fn create(&self, request: CreateSession) -> Result<SessionSummary, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, &self.principal).await?;
        let workspace: bool = sqlx_core::query_scalar::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.workspace
             WHERE tenant_id=$1 AND principal_id=$2 AND id=$3)",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(request.workspace_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if !workspace {
            return Err(ApplicationError::NotFound);
        }
        let id = format!(
            "ses_{}",
            stable_id(
                &self.principal,
                "create-session",
                request.request_id.as_str(),
                None
            )
        );
        let digest = zuno_orchestration::sha256_json(&json!(request));
        let (id, new) = reserve_request(
            &mut tx,
            &self.principal,
            "create-session",
            request.request_id.as_str(),
            &digest,
            &id,
        )
        .await?;
        if new {
            let time = database_time(&mut tx).await?;
            sqlx_core::query::query(
                "INSERT INTO zuno_enterprise_preview.session(tenant_id,principal_id,id,workspace_id,title,time_created,time_updated)
                 VALUES($1,$2,$3,$4,$5,$6,$6)",
            ).bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                .bind(&id).bind(request.workspace_id.as_str()).bind(&request.title).bind(time)
                .execute(&mut *tx).await.map_err(database_error)?;
            emit(
                &mut tx,
                &self.principal,
                &id,
                "application.session.created",
                json!({
                    "requestID":request.request_id,"requestDigest":digest,
                    "workspaceID":request.workspace_id,"principal":self.principal,
                }),
            )
            .await?;
        }
        let row = read_session(&mut tx, &self.principal, &id, false).await?;
        let result = summary(&row)?;
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }

    async fn get(&self, id: &SessionId) -> Result<SessionSummary, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, &self.principal).await?;
        let result = summary(&read_session(&mut tx, &self.principal, id.as_str(), false).await?)?;
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }

    async fn list(&self, request: SessionPageRequest) -> Result<SessionPage, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, &self.principal).await?;
        let rows = sqlx_core::query::query(
            "SELECT id,workspace_id,title,time_created,time_updated FROM zuno_enterprise_preview.session
             WHERE tenant_id=$1 AND principal_id=$2
               AND ($3::bigint IS NULL OR time_updated<$3 OR (time_updated=$3 AND id<$4))
             ORDER BY time_updated DESC,id DESC LIMIT $5",
        ).bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
            .bind(request.after.as_ref().map(|cursor|cursor.updated_at))
            .bind(request.after.as_ref().map(|cursor|cursor.session_id.as_str()))
            .bind(i64::from(request.limit.get())+1)
            .fetch_all(&mut *tx).await.map_err(database_error)?;
        let mut items = rows.iter().map(summary).collect::<Result<Vec<_>, _>>()?;
        let more = items.len() > usize::from(request.limit.get());
        items.truncate(usize::from(request.limit.get()));
        let next = if more {
            items.last().map(|last| SessionCursor {
                updated_at: last.updated_at,
                session_id: last.id.clone(),
            })
        } else {
            None
        };
        tx.commit().await.map_err(database_error)?;
        Ok(SessionPage { items, next })
    }

    async fn queue_text(&self, request: QueueText) -> Result<InputReceipt, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, &self.principal).await?;
        let row = read_session(&mut tx, &self.principal, request.session_id.as_str(), true).await?;
        let proposed = format!(
            "msg_{}",
            stable_id(
                &self.principal,
                "queue-text",
                request.request_id.as_str(),
                Some(request.session_id.as_str()),
            )
        );
        let digest = zuno_orchestration::sha256_json(&json!(request));
        let operation = format!("queue-text:{}", request.session_id);
        let (id, new) = reserve_request(
            &mut tx,
            &self.principal,
            &operation,
            request.request_id.as_str(),
            &digest,
            &proposed,
        )
        .await?;
        if new {
            let agent: Option<String> = row.try_get("agent").map_err(database_error)?;
            let model: Option<Value> = row.try_get("model").map_err(database_error)?;
            let prompt = json!({
                "kind":"user","prompt":{"text":request.text,"files":[],"agents":[]},"agent":agent,"model":model,
            });
            let time = database_time(&mut tx).await?;
            let sequence = emit(
                &mut tx,
                &self.principal,
                request.session_id.as_str(),
                "session.input.admitted",
                json!({"inputID":id,"prompt":prompt,"delivery":"queue","state":"queued",
                    "triggerKind":"user","timeCreated":time}),
            )
            .await?;
            sqlx_core::query::query(
                "INSERT INTO zuno_enterprise_preview.input(
                   tenant_id,principal_id,session_id,id,request_key,prompt,state,admitted_sequence,time_created)
                 VALUES($1,$2,$3,$4,$5,$6,'queued',$7,$8)",
            ).bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                .bind(request.session_id.as_str()).bind(&id).bind(format!("application:{id}"))
                .bind(prompt).bind(sequence).bind(time)
                .execute(&mut *tx).await.map_err(database_error)?;
            emit(
                &mut tx,
                &self.principal,
                request.session_id.as_str(),
                "application.input.queued",
                json!({
                    "inputID":id,"requestID":request.request_id,"principal":self.principal,
                }),
            )
            .await?;
        }
        let row = sqlx_core::query::query(
            "SELECT id,session_id,state,admitted_sequence FROM zuno_enterprise_preview.input
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(request.session_id.as_str())
        .bind(&id)
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        let state: String = row.try_get("state").map_err(database_error)?;
        let state = match state.as_str() {
            "queued" => InputState::Queued,
            "steering" => InputState::Steering,
            "promoted" => InputState::Promoted,
            "consumed" => InputState::Consumed,
            "cancelled" => InputState::Cancelled,
            "failed" => InputState::Failed,
            _ => {
                return Err(ApplicationError::storage(std::io::Error::other(
                    "unknown input state",
                )));
            }
        };
        let sequence: i64 = row.try_get("admitted_sequence").map_err(database_error)?;
        let receipt = InputReceipt {
            id: InputId::new(row.try_get::<String, _>("id").map_err(database_error)?)
                .map_err(ApplicationError::storage)?,
            session_id: SessionId::new(
                row.try_get::<String, _>("session_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
            state,
            admitted_cursor: u64::try_from(sequence)
                .map_err(ApplicationError::storage)?
                .to_string(),
        };
        tx.commit().await.map_err(database_error)?;
        Ok(receipt)
    }
}

fn stable_id(scope: &PrincipalScope, kind: &str, request: &str, session: Option<&str>) -> String {
    zuno_orchestration::sha256_json(&json!([
        kind,
        scope.owner(),
        scope.client_id(),
        session,
        request
    ]))
}

async fn reserve_request(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
    operation: &str,
    request: &str,
    digest: &str,
    resource: &str,
) -> Result<(String, bool), ApplicationError> {
    let client = principal.client_id().map_or("", |id| id.as_str());
    let inserted = sqlx_core::query::query(
        "INSERT INTO zuno_enterprise_preview.request_receipt(
           tenant_id,principal_id,client_id,operation,request_id,request_digest,resource_id)
         VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
    )
    .bind(principal.tenant_id().as_str())
    .bind(principal.principal_id().as_str())
    .bind(client)
    .bind(operation)
    .bind(request)
    .bind(digest)
    .bind(resource)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?
    .rows_affected()
        == 1;
    let row = sqlx_core::query::query(
        "SELECT request_digest,resource_id FROM zuno_enterprise_preview.request_receipt
         WHERE tenant_id=$1 AND principal_id=$2 AND client_id=$3 AND operation=$4 AND request_id=$5",
    ).bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str())
        .bind(client).bind(operation).bind(request)
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    if row
        .try_get::<String, _>("request_digest")
        .map_err(database_error)?
        != digest
    {
        return Err(ApplicationError::Conflict);
    }
    Ok((
        row.try_get("resource_id").map_err(database_error)?,
        inserted,
    ))
}

pub(crate) async fn read_session(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
    id: &str,
    lock: bool,
) -> Result<PgRow, ApplicationError> {
    let sql = if lock {
        "SELECT id,workspace_id,title,time_created,time_updated,agent,model
         FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE"
    } else {
        "SELECT id,workspace_id,title,time_created,time_updated,agent,model
         FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3"
    };
    sqlx_core::query::query(sql)
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(database_error)
}

fn summary(row: &PgRow) -> Result<SessionSummary, ApplicationError> {
    Ok(SessionSummary {
        id: SessionId::new(row.try_get::<String, _>("id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?,
        workspace_id: Some(
            WorkspaceId::new(
                row.try_get::<String, _>("workspace_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
        ),
        title: row.try_get("title").map_err(database_error)?,
        created_at: row.try_get("time_created").map_err(database_error)?,
        updated_at: row.try_get("time_updated").map_err(database_error)?,
    })
}

pub(crate) async fn emit(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
    session: &str,
    kind: &str,
    data: Value,
) -> Result<i64, ApplicationError> {
    let sequence: i64 = sqlx_core::query_scalar::query_scalar(
        "UPDATE zuno_enterprise_preview.session SET event_sequence=event_sequence+1
         WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 RETURNING event_sequence",
    )
    .bind(principal.tenant_id().as_str())
    .bind(principal.principal_id().as_str())
    .bind(session)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    let id = format!(
        "evt_{}",
        zuno_orchestration::sha256_json(&json!([principal.owner(), session, sequence]))
    );
    sqlx_core::query::query(
        "INSERT INTO zuno_enterprise_preview.event(tenant_id,principal_id,session_id,id,sequence,type,data)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    ).bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(session)
        .bind(id).bind(sequence).bind(kind).bind(data)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(sequence)
}
