//! Authorized client reads. Worker lease reads use their separate fenced port.

use crate::{PostgresBackend, database_error, runtime, scoped_transaction};
use sqlx_core::{query::query, query_scalar::query_scalar, row::Row};
use zuno_application::{ApplicationError, runtime::RuntimeJob};
use zuno_types::identity::{JobId, OperationId, PrincipalScope, SessionId};
use zuno_types::wait::WaitRef;

/// Internal read model; public protocols select their own safe fields.
pub struct ClientJobState {
    pub job: RuntimeJob,
    pub waits: Vec<WaitRef>,
    pub stop_requested: bool,
    pub pending_operations: Vec<OperationId>,
}

impl PostgresBackend {
    pub async fn client_workflow(
        &self,
        principal: &PrincipalScope,
        id: &JobId,
    ) -> Result<zuno_application::workflow::WorkflowRunView, ApplicationError> {
        use zuno_application::workflow::{NodeRunView, WorkflowRunView};
        use zuno_types::{
            activity::InvocationState,
            identity::{NodeRunId, WorkflowRunId},
        };
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let row = query("SELECT run_id,plan->'template'->>'name' AS name,plan->'template'->'nodes' AS nodes,state
            FROM zuno_enterprise_preview.runtime_workflow WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        let run_id =
            WorkflowRunId::new(row.try_get::<String, _>("run_id").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        let definition: Vec<zuno_orchestration::WorkflowNodeDescriptor> =
            serde_json::from_value(row.try_get("nodes").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        let rows = query("SELECT n.node_run_id,n.node_id,n.child_job_id,COALESCE(r.phase,n.state) AS state,n.position,r.turn_id
            FROM zuno_enterprise_preview.runtime_workflow_node n
            LEFT JOIN zuno_enterprise_preview.runtime_job r ON r.tenant_id=n.tenant_id AND r.principal_id=n.principal_id AND r.job_id=n.child_job_id
            WHERE n.tenant_id=$1 AND n.principal_id=$2 AND n.run_id=$3 ORDER BY n.position LIMIT 65")
            .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(run_id.as_str())
            .fetch_all(&mut *tx).await.map_err(database_error)?;
        if rows.len() > 64 {
            return Err(ApplicationError::Conflict);
        }
        let mut nodes = Vec::new();
        for node in rows {
            let job = JobId::new(
                node.try_get::<String, _>("child_job_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?;
            let position: i32 = node.try_get("position").map_err(database_error)?;
            let configured = usize::try_from(position)
                .ok()
                .and_then(|position| definition.get(position))
                .ok_or(ApplicationError::Conflict)?;
            let node_id: String = node.try_get("node_id").map_err(database_error)?;
            if configured.id != node_id {
                return Err(ApplicationError::Conflict);
            }
            let phase: String = node.try_get("state").map_err(database_error)?;
            let state = match phase.as_str() {
                "pending" | "ready" => InvocationState::Queued,
                "running" => InvocationState::Running,
                "waiting" | "paused" => InvocationState::Waiting,
                "completed" => InvocationState::Succeeded,
                "failed" => InvocationState::Failed,
                "cancelled" => InvocationState::Cancelled,
                "uncertain" => InvocationState::Uncertain,
                _ => return Err(ApplicationError::Conflict),
            };
            let raw: Vec<serde_json::Value> = query_scalar("SELECT reference FROM zuno_enterprise_preview.runtime_wait
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state IN('pending','ready') ORDER BY id LIMIT 65")
                .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(job.as_str())
                .fetch_all(&mut *tx).await.map_err(database_error)?;
            if raw.len() > 64 {
                return Err(ApplicationError::Conflict);
            }
            let turn: Option<String> = node.try_get("turn_id").map_err(database_error)?;
            let waits = raw
                .into_iter()
                .map(|value| {
                    let reference: WaitRef =
                        serde_json::from_value(value).map_err(ApplicationError::storage)?;
                    reference
                        .validate()
                        .map_err(|value| ApplicationError::Invalid(value.to_owned()))?;
                    if turn.as_deref() != Some(reference.turn_id.as_str()) {
                        return Err(ApplicationError::Conflict);
                    }
                    Ok(zuno_application::api::JobWaitView {
                        invocation_id: reference.invocation_id,
                        target: reference.target,
                    })
                })
                .collect::<Result<Vec<_>, ApplicationError>>()?;
            nodes.push(NodeRunView {
                id: NodeRunId::new(
                    node.try_get::<String, _>("node_run_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
                node_id,
                job_id: job,
                state,
                depends_on: configured.depends_on.clone(),
                waits,
            });
        }
        let view = WorkflowRunView {
            id: run_id,
            job_id: id.clone(),
            name: row.try_get("name").map_err(database_error)?,
            state: serde_json::from_value(serde_json::Value::String(
                row.try_get("state").map_err(database_error)?,
            ))
            .map_err(ApplicationError::storage)?,
            nodes,
        };
        tx.commit().await.map_err(database_error)?;
        Ok(view)
    }

    pub async fn client_submission(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
        request: &zuno_types::identity::RequestId,
    ) -> Result<ClientJobState, ApplicationError> {
        let id = JobId::new(format!(
            "job_{}",
            runtime::request_key(principal, session, request)
        ))
        .map_err(ApplicationError::storage)?;
        let state = self.client_job(principal, &id).await?;
        if state.job.session_id != *session {
            return Err(ApplicationError::Conflict);
        }
        Ok(state)
    }

    pub async fn client_job(
        &self,
        principal: &PrincipalScope,
        id: &JobId,
    ) -> Result<ClientJobState, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        let job = runtime::read_job(&mut tx, &principal.owner(), id.as_str()).await?;
        let rows = query(
            "SELECT reference FROM zuno_enterprise_preview.runtime_wait
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state IN ('pending','ready')
             ORDER BY id LIMIT 65",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(database_error)?;
        if rows.len() > 64 {
            return Err(ApplicationError::Invalid(
                "too many active invocation waits".to_owned(),
            ));
        }
        let mut waits = Vec::with_capacity(rows.len());
        for row in rows {
            let reference: WaitRef =
                serde_json::from_value(row.try_get("reference").map_err(database_error)?)
                    .map_err(ApplicationError::storage)?;
            reference
                .validate()
                .map_err(|message| ApplicationError::Invalid(message.to_owned()))?;
            if reference.turn_id != job.turn_id {
                return Err(ApplicationError::Invalid(
                    "wait belongs to another turn".to_owned(),
                ));
            }
            waits.push(reference);
        }
        let stop_requested: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_stop
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3)",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        let pending: Vec<String> = query_scalar(
            "SELECT o.operation_id FROM zuno_enterprise_preview.gateway_operation o
             JOIN zuno_enterprise_preview.runtime_stop s
               ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.job_id=o.job_id
             WHERE o.tenant_id=$1 AND o.principal_id=$2 AND (s.root_job_id=$3 OR s.job_id=$3)
               AND o.completion IS NULL ORDER BY o.operation_id",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(database_error)?;
        let pending_operations = pending
            .into_iter()
            .map(|id| OperationId::new(id).map_err(ApplicationError::storage))
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await.map_err(database_error)?;
        Ok(ClientJobState {
            job,
            waits,
            stop_requested,
            pending_operations,
        })
    }

    pub async fn client_input_version(
        &self,
        principal: &PrincipalScope,
        session: &SessionId,
    ) -> Result<u64, ApplicationError> {
        let mut tx = scoped_transaction(&self.pool, principal).await?;
        crate::session::read_session(&mut tx, principal, session.as_str(), false).await?;
        let version: i64 = query_scalar(
            "SELECT input_version FROM zuno_enterprise_preview.runtime_session
             WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
        )
        .bind(principal.tenant_id().as_str())
        .bind(principal.principal_id().as_str())
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        let version = u64::try_from(version).map_err(ApplicationError::storage)?;
        tx.commit().await.map_err(database_error)?;
        Ok(version)
    }
}
