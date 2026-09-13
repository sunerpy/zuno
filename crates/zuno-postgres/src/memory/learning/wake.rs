//! Recover maintenance wakes from durable Memory state, including changes made
//! while no foreground Agent is running. No provider call occurs in this scan.
use super::*;
use sqlx_postgres::PgConnection;

async fn latest_source(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    grant: &MemoryLearningGrant,
) -> Result<Option<(LearningExecution, ExtractionContext)>, Error> {
    let row = query(
        "SELECT e.*,j.workspace_id,j.session_id
         FROM zuno_enterprise_preview.learning_execution e
         JOIN zuno_enterprise_preview.learning_job j
           ON j.tenant_id=e.tenant_id AND j.principal_id=e.principal_id AND j.id=e.job_id
         JOIN zuno_enterprise_preview.runtime_job r
           ON r.tenant_id=e.tenant_id AND r.principal_id=e.principal_id AND r.job_id=e.source_job_id
         JOIN zuno_enterprise_preview.memory_policy p
           ON p.tenant_id=e.tenant_id AND p.principal_id=e.principal_id
         WHERE e.tenant_id=$1 AND e.principal_id=$2 AND j.workspace_id=$3
           AND e.phase='extraction' AND j.status='completed' AND r.phase='completed'
           AND r.configuration=$4 AND e.configuration=$5
           AND p.automatic_private AND p.generate_private AND r.time_created>=p.automation_since
           AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.session_memory_policy sp
             WHERE sp.tenant_id=e.tenant_id AND sp.principal_id=e.principal_id AND sp.session_id=j.session_id
               AND sp.revision>0 AND (NOT sp.generate_private OR NOT sp.automatic_private))
         ORDER BY e.created_at DESC,e.job_id DESC LIMIT 1",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(grant.workspace.as_str())
    .bind(json!(grant.source))
    .bind(json!(grant.extraction))
    .fetch_optional(connection)
    .await
    .map_err(sql_error)?;
    row.map(|row| {
        let context: ExtractionContext =
            serde_json::from_value(row.try_get("context").map_err(sql_error)?)
                .map_err(decode_error)?;
        if context.maintenance != grant.maintenance
            || json!(context.maintenance_limits) != json!(grant.maintenance_limits)
        {
            return Err(Error::Conflict);
        }
        Ok((store::execution(&row)?, context))
    })
    .transpose()
}

impl PostgresLearningRuntime {
    pub(super) async fn refresh_maintenance(
        &self,
        actor: &PrincipalScope,
        grant: &MemoryLearningGrant,
    ) -> Result<bool, Error> {
        let mut tx = owner_transaction(&self.memory.backend.pool, &actor.owner())
            .await
            .map_err(app_error)?;
        let source = latest_source(&mut tx, &actor.owner(), grant).await?;
        tx.commit().await.map_err(sql_error)?;
        let Some((source, _)) = source else {
            return Ok(false);
        };
        let grant = grant.clone();
        self.memory
            .automate(
                actor.clone(),
                grant.workspace.clone(),
                source.session.clone(),
                move |provider| {
                    // Re-read under the Memory owner lock. Re-enabling consent or
                    // replacing the selected source cannot lend this wake old rights.
                    let current = provider.execute(async |tx| {
                        latest_source(tx, &provider.principal.owner(), &grant).await
                    })?;
                    let Some((current, context)) = current else {
                        return Ok(false);
                    };
                    if current.id != source.id || current.session != source.session {
                        return Ok(false);
                    }
                    provider.schedule_maintenance(&current, &context)
                },
            )
            .await
    }
}
