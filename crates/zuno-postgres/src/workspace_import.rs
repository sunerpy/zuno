//! Initial workspace admission serializes with the first input. No Worker can
//! start against an incompletely restored or differently configured workspace.
use crate::{
    PostgresBackend, database_error, database_time, owner_transaction, scoped_transaction,
};
use serde_json::{Value, json};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row, transaction::Transaction};
use sqlx_postgres::{PgRow, Postgres};
use zuno_application::{
    ApplicationError, environment::EnvironmentSpec, runtime::ConfigurationRef, workspace_import::*,
};
use zuno_types::{
    activity::Counter,
    identity::{GatewayId, PrincipalKey, PrincipalScope, RequestId, SessionId, WorkspaceImportId},
};

fn id(principal: &PrincipalScope, session: &SessionId, request: &RequestId) -> WorkspaceImportId {
    WorkspaceImportId::new(format!(
        "import_{}",
        zuno_orchestration::sha256_json(&json!([
            principal.owner(),
            principal.client_id(),
            session,
            request
        ]))
    ))
    .expect("derived identity")
}
async fn empty_session(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
    session: &SessionId,
) -> Result<(), ApplicationError> {
    crate::session::read_session(tx, principal, session.as_str(), true).await?;
    let valid:bool=query_scalar("SELECT s.parent_id IS NULL AND r.input_version=0 AND r.current_job_id IS NULL
        AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_job j WHERE j.tenant_id=s.tenant_id AND j.principal_id=s.principal_id AND j.session_id=s.id)
        FROM zuno_enterprise_preview.session s JOIN zuno_enterprise_preview.runtime_session r
          ON r.tenant_id=s.tenant_id AND r.principal_id=s.principal_id AND r.session_id=s.id
        WHERE s.tenant_id=$1 AND s.principal_id=$2 AND s.id=$3")
        .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(session.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    if !valid {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}
async fn human(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
) -> Result<(), ApplicationError> {
    let access = crate::authorization::access_in(tx, &principal.owner()).await?;
    if !zuno_permission::enterprise::can_approve(
        &access.policy,
        zuno_permission::enterprise::ApprovalAudience::Requester,
        &principal.owner(),
        principal,
        &access.member,
    ) {
        return Err(ApplicationError::Forbidden);
    }
    Ok(())
}
async fn read(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    session: &SessionId,
    id: &WorkspaceImportId,
) -> Result<PgRow, ApplicationError> {
    query("SELECT * FROM zuno_enterprise_preview.workspace_import WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4 FOR UPDATE")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str()).bind(id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)
}
fn assignment(row: &PgRow) -> Result<WorkspaceImportAssignment, ApplicationError> {
    let value: Value = row.try_get("assignment").map_err(database_error)?;
    if zuno_orchestration::sha256_json(&value)
        != row
            .try_get::<String, _>("assignment_digest")
            .map_err(database_error)?
    {
        return Err(ApplicationError::Conflict);
    }
    let value: WorkspaceImportAssignment =
        serde_json::from_value(value).map_err(ApplicationError::storage)?;
    value.configuration.validate()?;
    value.environment.validate()?;
    if value.id.as_str() != row.try_get::<String, _>("id").map_err(database_error)?
        || value.session_id.as_str()
            != row
                .try_get::<String, _>("session_id")
                .map_err(database_error)?
        || value.principal.tenant_id().as_str()
            != row
                .try_get::<String, _>("tenant_id")
                .map_err(database_error)?
        || value.principal.principal_id().as_str()
            != row
                .try_get::<String, _>("principal_id")
                .map_err(database_error)?
        || value.environment.session_id != value.session_id
    {
        return Err(ApplicationError::Conflict);
    }
    Ok(value)
}
fn view(row: &PgRow) -> Result<WorkspaceImportView, ApplicationError> {
    let assignment = assignment(row)?;
    Ok(WorkspaceImportView {
        id: assignment.id,
        session_id: assignment.session_id,
        state: serde_json::from_value(json!(
            row.try_get::<String, _>("state").map_err(database_error)?
        ))
        .map_err(ApplicationError::storage)?,
        sha256: assignment.sha256,
        bytes: Counter(assignment.bytes),
        created_at: Counter(
            u64::try_from(
                row.try_get::<i64, _>("time_created")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
        ),
    })
}
#[async_trait::async_trait]
impl WorkspaceImportStore for PostgresBackend {
    async fn begin_import(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        request: BeginWorkspaceImport,
        configuration: ConfigurationRef,
        gateway: GatewayId,
        environment: EnvironmentSpec,
    ) -> Result<WorkspaceImportView, ApplicationError> {
        request.validate()?;
        configuration.validate()?;
        environment.validate()?;
        if environment.session_id != *session {
            return Err(ApplicationError::Forbidden);
        }
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        human(&mut tx, principal).await?;
        crate::session::read_session(&mut tx, principal, session.as_str(), true).await?;
        let id = id(principal, session, &request.request_id);
        let digest =
            zuno_orchestration::sha256_json(&json!([request, configuration, gateway, environment]));
        match read(&mut tx, &principal.owner(), session, &id).await {
            Ok(row) => {
                if row
                    .try_get::<String, _>("request_digest")
                    .map_err(database_error)?
                    != digest
                {
                    return Err(ApplicationError::Conflict);
                }
                let value = view(&row)?;
                tx.commit().await.map_err(database_error)?;
                return Ok(value);
            }
            Err(ApplicationError::NotFound) => {}
            Err(error) => return Err(error),
        }
        empty_session(&mut tx, principal, session).await?;
        let active:bool=query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.workspace_import WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND state<>'cancelled')")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(session.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        if active {
            return Err(ApplicationError::Conflict);
        }
        let assigned = WorkspaceImportAssignment {
            id: id.clone(),
            principal: principal.clone(),
            session_id: session.clone(),
            configuration,
            gateway_id: gateway,
            environment,
            sha256: request.sha256,
            bytes: request.bytes.0,
        };
        let encoded = json!(assigned);
        let now = database_time(&mut tx).await?;
        query("INSERT INTO zuno_enterprise_preview.workspace_import(tenant_id,principal_id,id,session_id,request_digest,assignment,assignment_digest,state,time_created,time_updated)
            VALUES($1,$2,$3,$4,$5,$6,$7,'uploading',$8,$8)")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(id.as_str()).bind(session.as_str()).bind(digest)
            .bind(&encoded).bind(zuno_orchestration::sha256_json(&encoded)).bind(now).execute(&mut *tx).await.map_err(database_error)?;
        crate::session::emit(
            &mut tx,
            principal,
            session.as_str(),
            "session.workspace.import.created",
            json!({"importId":id,"sha256":assigned.sha256,"bytes":assigned.bytes}),
        )
        .await?;
        let value = view(&read(&mut tx, &principal.owner(), session, &id).await?)?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn import_view(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        id: &WorkspaceImportId,
    ) -> Result<WorkspaceImportView, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let value = view(&read(&mut tx, &principal.owner(), session, id).await?)?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn cancel_import(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        id: &WorkspaceImportId,
    ) -> Result<WorkspaceImportView, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        human(&mut tx, principal).await?;
        empty_session(&mut tx, principal, session).await?;
        let row = read(&mut tx, &principal.owner(), session, id).await?;
        if !matches!(
            row.try_get::<&str, _>("state").map_err(database_error)?,
            "uploading" | "cancelled"
        ) {
            return Err(ApplicationError::Conflict);
        }
        if row.try_get::<&str, _>("state").map_err(database_error)? == "cancelled" {
            let value = view(&row)?;
            tx.commit().await.map_err(database_error)?;
            return Ok(value);
        }
        query("UPDATE zuno_enterprise_preview.workspace_import SET state='cancelled',time_updated=$5 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(session.as_str()).bind(id.as_str()).bind(database_time(&mut tx).await?)
            .execute(&mut *tx).await.map_err(database_error)?;
        crate::session::emit(
            &mut tx,
            principal,
            session.as_str(),
            "session.workspace.import.cancelled",
            json!({"importId":id}),
        )
        .await?;
        let value = view(&read(&mut tx, &principal.owner(), session, id).await?)?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
}

impl PostgresBackend {
    pub async fn complete_workspace_import(
        &self,
        gateway: &GatewayId,
        owner: &PrincipalKey,
        session: &SessionId,
        receipt: &WorkspaceImportReceipt,
    ) -> Result<(), ApplicationError> {
        let mut tx = owner_transaction(&self.pool, owner).await?;
        query("SELECT id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 FOR UPDATE")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str()).fetch_one(&mut *tx).await.map_err(database_error)?;
        let row = read(&mut tx, owner, session, &receipt.import_id).await?;
        let assigned = assignment(&row)?;
        if gateway != &assigned.gateway_id {
            return Err(ApplicationError::Forbidden);
        }
        assigned.validate_receipt(receipt)?;
        if row.try_get::<&str, _>("state").map_err(database_error)? == "ready" {
            let prior: Value = row.try_get("receipt").map_err(database_error)?;
            if prior != json!(receipt)
                || row
                    .try_get::<String, _>("receipt_digest")
                    .map_err(database_error)?
                    != zuno_orchestration::sha256_json(&prior)
            {
                return Err(ApplicationError::Conflict);
            }
        } else {
            if row.try_get::<&str, _>("state").map_err(database_error)? != "initializing" {
                return Err(ApplicationError::Conflict);
            }
            let raw = json!(receipt);
            query("UPDATE zuno_enterprise_preview.workspace_import SET state='ready',receipt=$4,receipt_digest=$5,time_updated=$6
                WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(receipt.import_id.as_str()).bind(&raw)
                .bind(zuno_orchestration::sha256_json(&raw)).bind(database_time(&mut tx).await?)
                .execute(&mut *tx).await.map_err(database_error)?;
            crate::session::emit(&mut tx,&assigned.principal,session.as_str(),"session.workspace.import.ready",
                json!({"importId":assigned.id,"archiveSha256":assigned.sha256,"snapshot":receipt.snapshot})).await?;
        }
        tx.commit().await.map_err(database_error)
    }
    pub async fn imported_workspace(
        &self,
        owner: &PrincipalKey,
        session: &SessionId,
    ) -> Result<Option<(WorkspaceImportAssignment, WorkspaceImportReceipt)>, ApplicationError> {
        let mut tx = owner_transaction(&self.pool, owner).await?;
        let row=query("SELECT * FROM zuno_enterprise_preview.workspace_import WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND state<>'cancelled'")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str()).fetch_optional(&mut *tx).await.map_err(database_error)?;
        let result = if let Some(row) = row {
            if row.try_get::<&str, _>("state").map_err(database_error)? != "ready" {
                return Err(ApplicationError::Conflict);
            }
            let assigned = assignment(&row)?;
            let raw: Value = row.try_get("receipt").map_err(database_error)?;
            if row
                .try_get::<String, _>("receipt_digest")
                .map_err(database_error)?
                != zuno_orchestration::sha256_json(&raw)
            {
                return Err(ApplicationError::Conflict);
            }
            let receipt: WorkspaceImportReceipt =
                serde_json::from_value(raw).map_err(ApplicationError::storage)?;
            assigned.validate_receipt(&receipt)?;
            Some((assigned, receipt))
        } else {
            None
        };
        tx.commit().await.map_err(database_error)?;
        Ok(result)
    }
    pub async fn import_assignment(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        id: &WorkspaceImportId,
    ) -> Result<WorkspaceImportAssignment, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        human(&mut tx, principal).await?;
        let row = read(&mut tx, &principal.owner(), session, id).await?;
        if row.try_get::<&str, _>("state").map_err(database_error)? == "cancelled" {
            return Err(ApplicationError::Conflict);
        }
        let value = assignment(&row)?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    pub async fn authorize_workspace_initialization(
        &self,
        gateway: &GatewayId,
        assigned: &WorkspaceImportAssignment,
    ) -> Result<Option<WorkspaceImportReceipt>, ApplicationError> {
        let mut tx = owner_transaction(&self.pool, &assigned.principal.owner()).await?;
        human(&mut tx, &assigned.principal).await?;
        crate::session::read_session(
            &mut tx,
            &assigned.principal,
            assigned.session_id.as_str(),
            true,
        )
        .await?;
        let row = read(
            &mut tx,
            &assigned.principal.owner(),
            &assigned.session_id,
            &assigned.id,
        )
        .await?;
        if assignment(&row)? != *assigned || gateway != &assigned.gateway_id {
            return Err(ApplicationError::Forbidden);
        }
        if row.try_get::<&str, _>("state").map_err(database_error)? == "ready" {
            let raw: Value = row.try_get("receipt").map_err(database_error)?;
            if row
                .try_get::<String, _>("receipt_digest")
                .map_err(database_error)?
                != zuno_orchestration::sha256_json(&raw)
            {
                return Err(ApplicationError::Conflict);
            }
            let receipt: WorkspaceImportReceipt =
                serde_json::from_value(raw).map_err(ApplicationError::storage)?;
            assigned.validate_receipt(&receipt)?;
            tx.commit().await.map_err(database_error)?;
            return Ok(Some(receipt));
        }
        if !matches!(
            row.try_get::<&str, _>("state").map_err(database_error)?,
            "uploading" | "initializing"
        ) {
            return Err(ApplicationError::Forbidden);
        }
        empty_session(&mut tx, &assigned.principal, &assigned.session_id).await?;
        query("UPDATE zuno_enterprise_preview.workspace_import SET state='initializing',time_updated=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(assigned.principal.tenant_id().as_str()).bind(assigned.principal.principal_id().as_str()).bind(assigned.id.as_str()).bind(database_time(&mut tx).await?)
            .execute(&mut *tx).await.map_err(database_error)?;
        tx.commit().await.map_err(database_error)?;
        Ok(None)
    }
}

pub(crate) async fn require_initialized(
    tx: &mut Transaction<'_, Postgres>,
    principal: &PrincipalScope,
    session: &SessionId,
    configuration: &ConfigurationRef,
) -> Result<(), ApplicationError> {
    let row=query("SELECT * FROM zuno_enterprise_preview.workspace_import WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND state<>'cancelled'")
        .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(session.as_str()).fetch_optional(&mut **tx).await.map_err(database_error)?;
    if let Some(row) = row {
        let assigned = assignment(&row)?;
        if row.try_get::<&str, _>("state").map_err(database_error)? != "ready"
            || assigned.configuration != *configuration
        {
            return Err(ApplicationError::Conflict);
        }
    }
    Ok(())
}
