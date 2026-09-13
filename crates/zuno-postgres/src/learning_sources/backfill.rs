use super::*;
use sqlx_core::raw_sql::raw_sql;
use zuno_types::identity::{PrincipalId, TenantId};

pub(crate) async fn backfill(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    let mut after: Option<(String, String, String)> = None;
    loop {
        raw_sql(
            "ALTER TABLE zuno_enterprise_preview.learning_execution NO FORCE ROW LEVEL SECURITY",
        )
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
        let rows = query(
            "SELECT tenant_id,principal_id,job_id,source_job_id,context
             FROM zuno_enterprise_preview.learning_execution WHERE phase='extraction'
               AND ($1::text IS NULL OR (tenant_id,principal_id,job_id)>($1,$2,$3))
             ORDER BY tenant_id,principal_id,job_id LIMIT 128",
        )
        .bind(after.as_ref().map(|v| v.0.as_str()))
        .bind(after.as_ref().map(|v| v.1.as_str()))
        .bind(after.as_ref().map(|v| v.2.as_str()))
        .fetch_all(&mut *connection)
        .await
        .map_err(database_error)?;
        raw_sql("ALTER TABLE zuno_enterprise_preview.learning_execution FORCE ROW LEVEL SECURITY")
            .execute(&mut *connection)
            .await
            .map_err(database_error)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let tenant: String = row.try_get("tenant_id").map_err(database_error)?;
            let principal: String = row.try_get("principal_id").map_err(database_error)?;
            let job: String = row.try_get("job_id").map_err(database_error)?;
            let root: String = row.try_get("source_job_id").map_err(database_error)?;
            let context: Value = row.try_get("context").map_err(database_error)?;
            after = Some((tenant.clone(), principal.clone(), job.clone()));
            let owner = PrincipalKey {
                tenant_id: TenantId::new(tenant).map_err(ApplicationError::storage)?,
                principal_id: PrincipalId::new(principal).map_err(ApplicationError::storage)?,
            };
            query("SELECT set_config('zuno.tenant_id',$1,true),set_config('zuno.principal_id',$2,true)")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str())
                .execute(&mut *connection).await.map_err(database_error)?;
            // Older fixtures and non-executing preserved learning rows can have
            // no manifest. Actual frozen manifests are validated, never guessed.
            let Some(sources) = context.get("sources") else {
                continue;
            };
            for source in sources.as_array().ok_or(ApplicationError::Conflict)? {
                let origin: zuno_memory::remote::MemoryEvidenceOrigin =
                    serde_json::from_value(source["origin"].clone())
                        .map_err(ApplicationError::storage)?;
                let digest = source["sourceDigest"]
                    .as_str()
                    .ok_or(ApplicationError::Conflict)?;
                claim(
                    connection,
                    &owner,
                    &root,
                    &serde_json::json!(origin),
                    Some(digest),
                    SourceDisposition::Captured,
                    Some(&job),
                )
                .await?;
            }
        }
    }
    // Only identity/status coordinates are copied. Restore forced RLS before
    // the migration transaction can commit or expose the new format marker.
    raw_sql("ALTER TABLE zuno_enterprise_preview.runtime_job NO FORCE ROW LEVEL SECURITY;
        ALTER TABLE zuno_enterprise_preview.session NO FORCE ROW LEVEL SECURITY;
        ALTER TABLE zuno_enterprise_preview.learning_root_scan NO FORCE ROW LEVEL SECURITY;
        INSERT INTO zuno_enterprise_preview.learning_root_scan(tenant_id,principal_id,root_job_id,updated_at)
        SELECT r.tenant_id,r.principal_id,r.job_id,r.time_updated FROM zuno_enterprise_preview.runtime_job r
        JOIN zuno_enterprise_preview.session s ON s.tenant_id=r.tenant_id AND s.principal_id=r.principal_id AND s.id=r.session_id
        WHERE r.phase='completed' AND s.parent_id IS NULL
        ON CONFLICT(tenant_id,principal_id,root_job_id) DO NOTHING;
        ALTER TABLE zuno_enterprise_preview.runtime_job FORCE ROW LEVEL SECURITY;
        ALTER TABLE zuno_enterprise_preview.session FORCE ROW LEVEL SECURITY;
        ALTER TABLE zuno_enterprise_preview.learning_root_scan FORCE ROW LEVEL SECURITY;")
        .execute(connection).await.map_err(database_error)?;
    Ok(())
}
