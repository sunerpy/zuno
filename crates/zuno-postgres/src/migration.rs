use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgConnection, PgPool};
use zuno_application::ApplicationError;

use crate::database_error;

pub const PREVIEW_SCHEMA: &str = "zuno_enterprise_preview";
const FORMAT: i32 = 1;
const TABLES: &[&str] = &["workspace", "session", "request_receipt", "input", "event"];
const DDL: &str = include_str!("schema.sql");
const POLICY: &str = "tenant_id=current_setting('zuno.tenant_id',true) AND principal_id=current_setting('zuno.principal_id',true)";

fn invalid(message: &str) -> ApplicationError {
    ApplicationError::Invalid(message.to_owned())
}

fn source_digest() -> String {
    zuno_orchestration::sha256_text(&format!("{FORMAT}\n{DDL}\n{POLICY}"))
}

/// Use a migration credential, not the runtime credential. Schema, constraints,
/// row policies, grants and the format marker commit in one transaction.
pub async fn migrate(admin: &PgPool, runtime_role: &str) -> Result<(), ApplicationError> {
    if runtime_role.is_empty()
        || runtime_role.len() > 63
        || !runtime_role
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || !runtime_role.as_bytes()[0].is_ascii_lowercase()
    {
        return Err(invalid("invalid PostgreSQL runtime role name"));
    }
    let mut tx = admin.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("SET LOCAL search_path=pg_catalog")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::query::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('zuno.enterprise.preview.schema',0))",
    )
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    let role = sqlx_core::query::query(
        "SELECT rolname,rolsuper,rolbypassrls,rolname=current_user AS same_role FROM pg_roles WHERE rolname=$1",
    ).bind(runtime_role).fetch_optional(&mut *tx).await.map_err(database_error)?
        .ok_or_else(||invalid("provision the restricted runtime role before migrating"))?;
    if role
        .try_get::<bool, _>("rolsuper")
        .map_err(database_error)?
        || role
            .try_get::<bool, _>("rolbypassrls")
            .map_err(database_error)?
        || role
            .try_get::<bool, _>("same_role")
            .map_err(database_error)?
    {
        return Err(invalid(
            "migration and runtime roles must be separate; runtime cannot bypass RLS",
        ));
    }
    let exists: bool = sqlx_core::query_scalar::query_scalar(
        "SELECT to_regnamespace('zuno_enterprise_preview') IS NOT NULL",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(database_error)?;
    if exists {
        let objects: i64 = sqlx_core::query_scalar::query_scalar(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
             WHERE n.nspname='zuno_enterprise_preview'",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if objects > 0 {
            validate_schema(&mut tx).await?;
            grant_runtime(&mut tx, runtime_role).await?;
            tx.commit().await.map_err(database_error)?;
            return Ok(());
        }
    } else {
        sqlx_core::raw_sql::raw_sql("CREATE SCHEMA zuno_enterprise_preview")
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(DDL)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in TABLES {
        // Only fixed schema/table/policy constants and the validated ASCII role
        // identifier enter DDL. User data always uses bind parameters.
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table}
               USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        )))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "REVOKE ALL ON SCHEMA {PREVIEW_SCHEMA} FROM PUBLIC;
         CREATE TABLE {PREVIEW_SCHEMA}.schema_format(
           singleton integer PRIMARY KEY CHECK(singleton=1),
           version integer NOT NULL,
           channel text NOT NULL CHECK(channel='enterprise-preview'),
           source_digest text NOT NULL CHECK(length(source_digest)=64),
           manifest jsonb NOT NULL
         );
         REVOKE ALL ON {PREVIEW_SCHEMA}.schema_format FROM PUBLIC;"
    )))
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    grant_runtime(&mut tx, runtime_role).await?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "INSERT INTO zuno_enterprise_preview.schema_format(singleton,version,channel,source_digest,manifest)
         VALUES(1,$1,'enterprise-preview',$2,$3)",
    ).bind(FORMAT).bind(source_digest()).bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

/// Runtime never owns tables, inherits their owner role, creates objects in the
/// schema or connects as a superuser/BYPASSRLS role.
pub(crate) async fn validate_runtime(pool: &PgPool) -> Result<(), ApplicationError> {
    let mut connection = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("SET LOCAL search_path=pg_catalog")
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    let role = sqlx_core::query::query(
        "SELECT rolsuper,rolbypassrls FROM pg_roles WHERE rolname=current_user",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    if role
        .try_get::<bool, _>("rolsuper")
        .map_err(database_error)?
        || role
            .try_get::<bool, _>("rolbypassrls")
            .map_err(database_error)?
    {
        return Err(invalid(
            "the PostgreSQL runtime role must not bypass row security",
        ));
    }
    let dangerous: bool = sqlx_core::query_scalar::query_scalar(
        "SELECT has_schema_privilege(current_user,'zuno_enterprise_preview','CREATE')
         OR EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
           WHERE n.nspname='zuno_enterprise_preview'
           AND c.relkind IN('r','p','v','m','f')
           AND (pg_has_role(current_user,c.relowner,'MEMBER')
             OR has_table_privilege(current_user,c.oid,'TRUNCATE')
             OR has_table_privilege(current_user,c.oid,'TRIGGER')))
         OR EXISTS(SELECT 1 FROM pg_roles r WHERE (r.rolsuper OR r.rolbypassrls)
           AND pg_has_role(current_user,r.oid,'MEMBER'))
         OR has_table_privilege(current_user,'zuno_enterprise_preview.schema_format','UPDATE')
         OR has_table_privilege(current_user,'zuno_enterprise_preview.schema_format','DELETE')",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    if dangerous {
        return Err(invalid(
            "the runtime role must not own or alter the preview schema",
        ));
    }
    validate_schema(&mut connection).await?;
    connection.commit().await.map_err(database_error)
}

async fn validate_schema(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    let marker_exists: bool = sqlx_core::query_scalar::query_scalar(
        "SELECT to_regclass('zuno_enterprise_preview.schema_format') IS NOT NULL",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    if !marker_exists {
        return Err(invalid(
            "unmarked PostgreSQL preview schema; preserve it for inspection",
        ));
    }
    let rows = sqlx_core::query::query(
        "SELECT version,channel,source_digest,manifest FROM zuno_enterprise_preview.schema_format WHERE singleton=1",
    ).fetch_all(&mut *connection).await.map_err(database_error)?;
    let [marker] = rows.as_slice() else {
        return Err(invalid("invalid PostgreSQL schema marker"));
    };
    if marker
        .try_get::<i32, _>("version")
        .map_err(database_error)?
        != FORMAT
        || marker
            .try_get::<String, _>("channel")
            .map_err(database_error)?
            != "enterprise-preview"
        || marker
            .try_get::<String, _>("source_digest")
            .map_err(database_error)?
            != source_digest()
    {
        return Err(invalid(
            "unsupported PostgreSQL preview schema; no automatic downgrade is allowed",
        ));
    }
    let expected: Value = marker.try_get("manifest").map_err(database_error)?;
    if expected != schema_manifest(connection).await? {
        return Err(invalid("PostgreSQL preview schema or row security changed"));
    }
    Ok(())
}

/// Capture semantic catalog definitions without OIDs or credentials. Only the
/// migration owner can write the stored manifest. Accidental constraint, policy,
/// index and column changes then fail validation before client operations run.
async fn schema_manifest(connection: &mut PgConnection) -> Result<Value, ApplicationError> {
    let row = sqlx_core::query::query(
        "SELECT jsonb_build_object(
          'columns',COALESCE((SELECT jsonb_agg(jsonb_build_array(table_name,column_name,udt_name,is_nullable,column_default)
            ORDER BY table_name,ordinal_position) FROM information_schema.columns
            WHERE table_schema='zuno_enterprise_preview' AND table_name<>'schema_format'),'[]'::jsonb),
          'constraints',COALESCE((SELECT jsonb_agg(jsonb_build_array(c.relname,k.conname,pg_get_constraintdef(k.oid))
            ORDER BY c.relname,k.conname) FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='zuno_enterprise_preview' AND c.relname<>'schema_format'),'[]'::jsonb),
          'indexes',COALESCE((SELECT jsonb_agg(jsonb_build_array(tablename,indexname,indexdef)
            ORDER BY tablename,indexname) FROM pg_indexes
            WHERE schemaname='zuno_enterprise_preview' AND tablename<>'schema_format'),'[]'::jsonb),
          'policies',COALESCE((SELECT jsonb_agg(jsonb_build_array(tablename,policyname,permissive,roles,cmd,qual,with_check)
            ORDER BY tablename,policyname) FROM pg_policies WHERE schemaname='zuno_enterprise_preview'),'[]'::jsonb),
          'triggers',COALESCE((SELECT jsonb_agg(jsonb_build_array(c.relname,t.tgname,pg_get_triggerdef(t.oid))
            ORDER BY c.relname,t.tgname) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid
            JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='zuno_enterprise_preview' AND NOT t.tgisinternal),'[]'::jsonb),
          'functions',COALESCE((SELECT jsonb_agg(jsonb_build_array(p.proname,pg_get_functiondef(p.oid))
            ORDER BY p.proname,pg_get_function_identity_arguments(p.oid)) FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
            WHERE n.nspname='zuno_enterprise_preview'),'[]'::jsonb),
          'tables',COALESCE((SELECT jsonb_agg(jsonb_build_array(c.relname,c.relrowsecurity,c.relforcerowsecurity)
            ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='zuno_enterprise_preview' AND c.relkind='r' AND c.relname<>'schema_format'),'[]'::jsonb)
        ) AS manifest",
    ).fetch_one(connection).await.map_err(database_error)?;
    row.try_get("manifest").map_err(database_error)
}

async fn grant_runtime(connection: &mut PgConnection, role: &str) -> Result<(), ApplicationError> {
    // `role` was validated before entering the migration transaction; identifiers
    // cannot use bind parameters, and no other dynamic text enters this DDL.
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "GRANT USAGE ON SCHEMA {PREVIEW_SCHEMA} TO \"{role}\";
         GRANT SELECT ON {PREVIEW_SCHEMA}.schema_format TO \"{role}\";"
    )))
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    for table in TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\""
        )))
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    }
    Ok(())
}
