use serde_json::Value;
use sqlx_core::row::Row;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgConnection, PgPool};
use zuno_application::ApplicationError;

use crate::database_error;

pub const PREVIEW_SCHEMA: &str = "zuno_enterprise_preview";
pub(crate) const FORMAT: i32 = 19;
const TABLES: &[&str] = &["workspace", "session", "request_receipt", "input", "event"];
const RUNTIME_TABLES: &[&str] = &[
    "agent_job",
    "runtime_session",
    "runtime_job",
    "runtime_attempt",
    "runtime_owner_schedule",
];
const DDL: &str = include_str!("schema.sql");
const RUNTIME_DDL: &str = include_str!("schema_runtime.sql");
const AUTHORIZATION_DDL: &str = include_str!("schema_authorization.sql");
const TURN_DDL: &str = include_str!("schema_turn.sql");
const WAIT_DDL: &str = include_str!("schema_wait.sql");
const WAIT_TABLES: &[&str] = &["runtime_wait"];
const CONTEXT_DDL: &str = include_str!("schema_context.sql");
const CONTEXT_TABLES: &[&str] = &["context_usage", "input_execution_receipt"];
const BROWSER_DDL: &str = include_str!("schema_browser.sql");
const BROWSER_TABLES: &[&str] = &["browser_login", "browser_session", "authentication_audit"];
const OPERATION_DDL: &str = include_str!("schema_operation.sql");
const MEMORY_DDL: &str = include_str!("schema_memory.sql");
const CHILD_DDL: &str = include_str!("schema_child.sql");
const CHILD_TABLES: &[&str] = &["runtime_child"];
const CHILD_WORKSPACE_DDL: &str = include_str!("schema_child_workspace.sql");
const CHILD_WORKSPACE_TABLES: &[&str] = &["child_workspace_preparation"];
const CONTROL_DDL: &str = include_str!("schema_control.sql");
const ACTIVITY_DDL: &str = include_str!("schema_activity.sql");
const LIVE_DDL: &str = include_str!("schema_live.sql");
const WORKFLOW_DDL: &str = include_str!("schema_workflow.sql");
const WORKFLOW_TABLES: &[&str] = &["runtime_workflow", "runtime_workflow_node"];
const COUNCIL_DDL: &str = include_str!("schema_council.sql");
const COUNCIL_TABLES: &[&str] = &[
    "runtime_council",
    "runtime_council_seat",
    "runtime_council_attempt",
];
const MERGE_DDL: &str = include_str!("schema_merge.sql");
const IMPORT_DDL: &str = include_str!("schema_import.sql");
const TRANSFER_DDL: &str = include_str!("schema_transfer.sql");
const MERGE_TABLES: &[&str] = &[
    "gateway_merge_operation",
    "gateway_merge_attempt",
    "gateway_merge_cancellation",
];
const LIVE_TABLES: &[&str] = &["live_progress"];
const ACTIVITY_TABLES: &[&str] = &["activity_session", "activity_item", "activity_frame"];
const CONTROL_TABLES: &[&str] = &[
    "runtime_control_request",
    "runtime_stop",
    "runtime_continuation",
    "gateway_cancellation_delivery",
];
const MEMORY_TABLES: &[&str] = &[
    "memory_policy",
    "session_memory_policy",
    "memory_document",
    "memory_revision",
    "memory_candidate",
    "memory_evidence",
    "memory_provenance",
    "memory_retired",
    "learning_job",
    "memory_maintenance_state",
    "memory_request",
    "memory_audit",
];
const TURN_TABLES: &[&str] = &["message", "part", "provider_retry_backoff"];
const AUTHORIZATION_TABLES: &[&str] = &[
    "organization_member",
    "operation_approval",
    "approval_answer_receipt",
];
const POLICY: &str = "tenant_id=current_setting('zuno.tenant_id',true) AND principal_id=current_setting('zuno.principal_id',true)";
const TENANT_POLICY: &str = "tenant_id=current_setting('zuno.tenant_id',true)";

fn invalid(message: &str) -> ApplicationError {
    ApplicationError::Invalid(message.to_owned())
}

fn source_digest(version: i32) -> String {
    if version == 1 {
        zuno_orchestration::sha256_text(&format!("1\n{DDL}\n{POLICY}"))
    } else if version == 2 {
        zuno_orchestration::sha256_text(&format!("2\n{DDL}\n{RUNTIME_DDL}\n{POLICY}"))
    } else if version == 3 {
        zuno_orchestration::sha256_text(&format!(
            "3\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 4 {
        zuno_orchestration::sha256_text(&format!(
            "4\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 5 {
        zuno_orchestration::sha256_text(&format!(
            "5\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 6 {
        zuno_orchestration::sha256_text(&format!(
            "6\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 7 {
        zuno_orchestration::sha256_text(&format!(
            "7\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 8 {
        zuno_orchestration::sha256_text(&format!(
            "8\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 9 {
        zuno_orchestration::sha256_text(&format!(
            "9\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 10 {
        zuno_orchestration::sha256_text(&format!(
            "10\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 11 {
        zuno_orchestration::sha256_text(&format!(
            "11\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 12 {
        zuno_orchestration::sha256_text(&format!(
            "12\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 13 {
        zuno_orchestration::sha256_text(&format!(
            "13\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 14 {
        zuno_orchestration::sha256_text(&format!(
            "14\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{LIVE_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 15 {
        zuno_orchestration::sha256_text(&format!(
            "15\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{LIVE_DDL}\n{WORKFLOW_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 16 {
        zuno_orchestration::sha256_text(&format!(
            "16\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{LIVE_DDL}\n{WORKFLOW_DDL}\n{COUNCIL_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 17 {
        zuno_orchestration::sha256_text(&format!(
            "17\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{LIVE_DDL}\n{WORKFLOW_DDL}\n{COUNCIL_DDL}\n{MERGE_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else if version == 18 {
        zuno_orchestration::sha256_text(&format!(
            "18\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{LIVE_DDL}\n{WORKFLOW_DDL}\n{COUNCIL_DDL}\n{MERGE_DDL}\n{IMPORT_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    } else {
        zuno_orchestration::sha256_text(&format!(
            "{FORMAT}\n{DDL}\n{RUNTIME_DDL}\n{AUTHORIZATION_DDL}\n{TURN_DDL}\n{WAIT_DDL}\n{CONTEXT_DDL}\n{BROWSER_DDL}\n{OPERATION_DDL}\n{MEMORY_DDL}\n{CHILD_DDL}\n{CHILD_WORKSPACE_DDL}\n{CONTROL_DDL}\n{ACTIVITY_DDL}\n{LIVE_DDL}\n{WORKFLOW_DDL}\n{COUNCIL_DDL}\n{MERGE_DDL}\n{IMPORT_DDL}\n{TRANSFER_DDL}\n{POLICY}\n{TENANT_POLICY}"
        ))
    }
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
        let is_owner: bool = sqlx_core::query_scalar::query_scalar(
            "SELECT nspowner=(SELECT oid FROM pg_roles WHERE rolname=current_user)
             FROM pg_namespace WHERE nspname='zuno_enterprise_preview'",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if !is_owner {
            return Err(invalid("run preview migrations as the schema owner"));
        }
        let objects: i64 = sqlx_core::query_scalar::query_scalar(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
             WHERE n.nspname='zuno_enterprise_preview'",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if objects > 0 {
            let version = validate_schema(&mut tx).await?;
            if version < FORMAT {
                if version < 2 {
                    install_runtime(&mut tx).await?;
                }
                if version < 3 {
                    install_authorization(&mut tx).await?;
                }
                if version < 4 {
                    install_turn(&mut tx).await?;
                }
                if version < 5 {
                    install_waits(&mut tx).await?;
                }
                if version < 6 {
                    install_context(&mut tx).await?;
                }
                if version < 7 {
                    install_browser(&mut tx).await?;
                }
                if version < 8 {
                    install_operations(&mut tx).await?;
                }
                if version < 9 {
                    install_memory(&mut tx).await?;
                }
                if version < 10 {
                    install_children(&mut tx).await?;
                }
                if version < 11 {
                    install_child_workspaces(&mut tx).await?;
                }
                if version < 12 {
                    install_control(&mut tx).await?;
                }
                if version < 13 {
                    install_activity(&mut tx).await?;
                }
                if version < 14 {
                    install_live(&mut tx).await?;
                }
                if version < 15 {
                    install_workflow(&mut tx).await?;
                }
                if version < 16 {
                    install_council(&mut tx).await?;
                }
                if version < 17 {
                    install_merge(&mut tx).await?;
                }
                if version < 18 {
                    install_import(&mut tx).await?;
                }
                install_transfers(&mut tx).await?;
                if version < 13 {
                    crate::activity::backfill(&mut tx).await?;
                }
                grant_runtime(&mut tx, runtime_role).await?;
                let manifest = schema_manifest(&mut tx).await?;
                let changed = sqlx_core::query::query(
                    "UPDATE zuno_enterprise_preview.schema_format SET version=$1,source_digest=$2,manifest=$3 WHERE singleton=1 AND version=$4",
                ).bind(FORMAT).bind(source_digest(FORMAT)).bind(manifest).bind(version)
                    .execute(&mut *tx).await.map_err(database_error)?.rows_affected();
                if changed != 1 {
                    return Err(invalid("PostgreSQL migration marker changed concurrently"));
                }
            } else {
                grant_runtime(&mut tx, runtime_role).await?;
            }
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
    install_runtime(&mut tx).await?;
    install_authorization(&mut tx).await?;
    install_turn(&mut tx).await?;
    install_waits(&mut tx).await?;
    install_context(&mut tx).await?;
    install_browser(&mut tx).await?;
    install_operations(&mut tx).await?;
    install_memory(&mut tx).await?;
    install_children(&mut tx).await?;
    install_child_workspaces(&mut tx).await?;
    install_control(&mut tx).await?;
    install_activity(&mut tx).await?;
    install_live(&mut tx).await?;
    install_workflow(&mut tx).await?;
    install_council(&mut tx).await?;
    install_merge(&mut tx).await?;
    install_import(&mut tx).await?;
    install_transfers(&mut tx).await?;
    crate::activity::backfill(&mut tx).await?;
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
    ).bind(FORMAT).bind(source_digest(FORMAT)).bind(manifest)
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
    if validate_schema(&mut connection).await? != FORMAT {
        return Err(invalid(
            "upgrade the PostgreSQL preview schema with the migration credential",
        ));
    }
    let public_execute: bool = sqlx_core::query_scalar::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace,
         LATERAL aclexplode(COALESCE(p.proacl,acldefault('f',p.proowner))) acl
         WHERE n.nspname='zuno_enterprise_preview' AND p.prosecdef
           AND acl.grantee=0 AND acl.privilege_type='EXECUTE')",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(database_error)?;
    if public_execute {
        return Err(invalid(
            "preview scheduling helpers must not be executable by PUBLIC",
        ));
    }
    connection.commit().await.map_err(database_error)
}

async fn validate_schema(connection: &mut PgConnection) -> Result<i32, ApplicationError> {
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
    let version: i32 = marker.try_get("version").map_err(database_error)?;
    if !(1..=FORMAT).contains(&version)
        || marker
            .try_get::<String, _>("channel")
            .map_err(database_error)?
            != "enterprise-preview"
        || marker
            .try_get::<String, _>("source_digest")
            .map_err(database_error)?
            != source_digest(version)
    {
        return Err(invalid(
            "unsupported PostgreSQL preview schema; no automatic downgrade is allowed",
        ));
    }
    let expected: Value = marker.try_get("manifest").map_err(database_error)?;
    if expected != schema_manifest(connection).await? {
        return Err(invalid("PostgreSQL preview schema or row security changed"));
    }
    Ok(version)
}

async fn install_runtime(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(RUNTIME_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    // Validate deferred backfill references before ALTER TABLE changes the RLS
    // catalog. The next transaction again uses each constraint's declared mode.
    sqlx_core::raw_sql::raw_sql("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in RUNTIME_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table}
               USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        )))
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
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
    for table in TABLES
        .iter()
        .chain(RUNTIME_TABLES)
        .chain(AUTHORIZATION_TABLES)
        .chain(TURN_TABLES)
        .chain(WAIT_TABLES)
        .chain(CONTEXT_TABLES)
        .chain(BROWSER_TABLES)
        .chain(MEMORY_TABLES)
        .chain(CHILD_TABLES)
        .chain(CHILD_WORKSPACE_TABLES)
        .chain(CONTROL_TABLES)
        .chain(ACTIVITY_TABLES)
        .chain(LIVE_TABLES)
        .chain(WORKFLOW_TABLES)
        .chain(COUNCIL_TABLES)
        .chain(MERGE_TABLES)
        .chain(["workspace_import"].iter())
        .chain(["workspace_snapshot_transfer"].iter())
        .chain(["gateway_operation", "gateway_operation_attempt"].iter())
        .chain(["organization_policy", "organization_audit"].iter())
    {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\""
        )))
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.clear_live_progress() TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.create_activity_session() TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.gateway_cancellations(text,text,integer) TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.gateway_merge_cancellations(text,text,integer) TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.dispatch_owners(text) TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.create_runtime_session() TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.advance_input_version() TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.create_input_execution_receipt() TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.approval_coordinates(text,text) TO \"{role}\";"
    )))
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    Ok(())
}

async fn install_turn(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(TURN_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in TURN_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table}
               USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        )))
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    }
    Ok(())
}

async fn install_transfers(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(TRANSFER_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.workspace_snapshot_transfer ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.workspace_snapshot_transfer FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.workspace_snapshot_transfer USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.workspace_snapshot_transfer FROM PUBLIC;"
    ))).execute(&mut *connection).await.map_err(database_error)?;
    Ok(())
}

async fn install_import(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(IMPORT_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.workspace_import ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.workspace_import FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.workspace_import USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.workspace_import FROM PUBLIC;"
    ))).execute(&mut *connection).await.map_err(database_error)?;
    Ok(())
}

async fn install_merge(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(MERGE_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in MERGE_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_council(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(COUNCIL_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in COUNCIL_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_workflow(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(WORKFLOW_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in WORKFLOW_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_live(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(LIVE_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in LIVE_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_activity(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(ACTIVITY_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in ACTIVITY_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_control(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(CONTROL_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in CONTROL_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_child_workspaces(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(CHILD_WORKSPACE_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in CHILD_WORKSPACE_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_children(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(CHILD_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in CHILD_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_memory(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(MEMORY_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in MEMORY_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_waits(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(WAIT_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in WAIT_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_context(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(CONTEXT_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in CONTEXT_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_browser(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(BROWSER_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in BROWSER_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY tenant_scope ON {PREVIEW_SCHEMA}.{table} USING ({TENANT_POLICY}) WITH CHECK ({TENANT_POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_operations(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(OPERATION_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in ["gateway_operation", "gateway_operation_attempt"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    Ok(())
}

async fn install_authorization(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(AUTHORIZATION_DDL)
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    for table in AUTHORIZATION_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        ))).execute(&mut *connection).await.map_err(database_error)?;
    }
    for table in ["organization_policy", "organization_audit"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY tenant_scope ON {PREVIEW_SCHEMA}.{table}
               USING({TENANT_POLICY}) WITH CHECK({TENANT_POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;"
        )))
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
    }
    Ok(())
}

/// Build the exact old DDL and representative rows, not a current schema whose
/// marker was changed. Only the isolated PostgreSQL test fixture can call this.
#[cfg(test)]
pub(crate) async fn install_format_one_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    assert!(role.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(
        "SET LOCAL search_path=pg_catalog; CREATE SCHEMA zuno_enterprise_preview",
    )
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(DDL)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format1-data.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "REVOKE ALL ON SCHEMA {PREVIEW_SCHEMA} FROM PUBLIC;
         CREATE TABLE {PREVIEW_SCHEMA}.schema_format(
           singleton integer PRIMARY KEY CHECK(singleton=1),version integer NOT NULL,
           channel text NOT NULL CHECK(channel='enterprise-preview'),
           source_digest text NOT NULL CHECK(length(source_digest)=64),manifest jsonb NOT NULL);
         REVOKE ALL ON {PREVIEW_SCHEMA}.schema_format FROM PUBLIC;
         GRANT SELECT ON {PREVIEW_SCHEMA}.schema_format TO \"{role}\";
         GRANT USAGE ON SCHEMA {PREVIEW_SCHEMA} TO \"{role}\";"
    )))
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "INSERT INTO zuno_enterprise_preview.schema_format(singleton,version,channel,source_digest,manifest)
         VALUES(1,1,'enterprise-preview',$1,$2)",
    ).bind(source_digest(1)).bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_two_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_one_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("SET LOCAL search_path=pg_catalog")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    install_runtime(&mut tx).await?;
    // The old schema owner still obeys FORCE RLS when seeding owned state.
    sqlx_core::query::query("SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true)")
        .execute(&mut *tx).await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format2-data.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in RUNTIME_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\""
        )))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.dispatch_owners(text) TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.create_runtime_session() TO \"{role}\";
         GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.advance_input_version() TO \"{role}\";"
    )))
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=2,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind(source_digest(2)).bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_three_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_two_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("SET LOCAL search_path=pg_catalog")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    // Captured from preview commit 1afbd60c, with the original source digest.
    // Neither a current installer nor a changed current DDL can redefine v3.
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format3-authorization.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in [
        "organization_member",
        "operation_approval",
        "approval_answer_receipt",
    ] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    for table in ["organization_policy", "organization_audit"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY tenant_scope ON {PREVIEW_SCHEMA}.{table} USING ({TENANT_POLICY}) WITH CHECK ({TENANT_POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.approval_coordinates(text,text) TO \"{role}\""
    )))
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(
        "SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true);
         INSERT INTO zuno_enterprise_preview.organization_policy(
           tenant_id,revision,allowed_apps,approval_apps,auto_read_apps,approval_lifetime_seconds)
         VALUES('migration-fixture',1,'[\"enterprise-web\"]','[\"enterprise-web\"]','[]',300);
         INSERT INTO zuno_enterprise_preview.organization_member(tenant_id,principal_id,role,active)
         VALUES('migration-fixture','owner','administrator',true);
         INSERT INTO zuno_enterprise_preview.organization_audit(tenant_id,id,actor,type,data,time_created)
         VALUES('migration-fixture','legacy-audit','{\"principalId\":\"owner\"}','organization.bootstrapped','{\"preserved\":true}',1000);",
    ).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "UPDATE zuno_enterprise_preview.schema_format SET version=3,source_digest=$1,manifest=$2 WHERE singleton=1",
    ).bind("825b8b1de2fe3924eb9114125a0e2b1c4c92768ab18577ad4ad605ba19588dd9")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_four_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_three_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Turn DDL and digest captured from preview commit 510290b5.
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format4-turn.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in ["message", "part", "provider_retry_backoff"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(
        "SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true);
         INSERT INTO zuno_enterprise_preview.message(tenant_id,principal_id,session_id,id,role,data,time_created,time_updated)
         VALUES('migration-fixture','owner','legacy-session','legacy-assistant','assistant',
           '{\"id\":\"legacy-assistant\",\"sessionID\":\"legacy-session\",\"role\":\"assistant\",\"time\":{\"created\":1004},\"providerID\":\"fixture\",\"modelID\":\"model\"}',1004,1004);
         INSERT INTO zuno_enterprise_preview.part(tenant_id,principal_id,session_id,message_id,id,kind,data,time_created,time_updated)
         VALUES('migration-fixture','owner','legacy-session','legacy-assistant','legacy-tool','tool',
           '{\"id\":\"legacy-tool\",\"sessionID\":\"legacy-session\",\"messageID\":\"legacy-assistant\",\"type\":\"tool\",\"callID\":\"legacy-call\",\"tool\":\"inspect\",\"state\":{\"status\":\"completed\",\"input\":{},\"output\":\"preserved result\"},\"metadata\":{\"thoughtSignature\":\"preserved-signature\"}}',1004,1004);
         UPDATE zuno_enterprise_preview.session SET context_epoch=3,tokens_known=true,tokens_input=123,tokens_output=9
           WHERE tenant_id='migration-fixture' AND principal_id='owner' AND id='legacy-session';",
    ).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "UPDATE zuno_enterprise_preview.schema_format SET version=4,source_digest=$1,manifest=$2 WHERE singleton=1",
    ).bind("563173ce50200fcd2a4dbbfb4a99270d51687a80b9b6e46609de2e04f8cfdeb2")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_five_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_four_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format5-wait.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.runtime_wait ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.runtime_wait FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.runtime_wait USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.runtime_wait FROM PUBLIC;
         GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.runtime_wait TO \"{role}\";"
    ))).execute(&mut *tx).await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(
        "SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true);
         UPDATE zuno_enterprise_preview.runtime_job SET phase='waiting' WHERE job_id='legacy-job';
         INSERT INTO zuno_enterprise_preview.runtime_wait(
           tenant_id,principal_id,id,job_id,session_id,turn_id,invocation_id,reference,state,time_created,time_updated)
         VALUES('migration-fixture','owner','legacy-wait','legacy-job','legacy-session','legacy-turn','legacy-pending',
           '{\"id\":\"legacy-wait\",\"turnId\":\"legacy-turn\",\"invocationId\":\"legacy-pending\",\"argumentsSha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"target\":{\"kind\":\"operation\",\"operation_id\":\"legacy-operation\"},\"continuation\":\"current_turn\"}',
           'pending',1005,1005);",
    ).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "UPDATE zuno_enterprise_preview.schema_format SET version=5,source_digest=$1,manifest=$2 WHERE singleton=1",
    ).bind("ee9af544a130315c5963acc55727f4e299cd352887c63f0cd113ad3e66a2c18a")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_six_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_five_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Captured from the merged main synchronization, commit 410faf43.
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format6-context.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in ["context_usage", "input_execution_receipt"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "GRANT EXECUTE ON FUNCTION {PREVIEW_SCHEMA}.create_input_execution_receipt() TO \"{role}\""
    )))
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "UPDATE zuno_enterprise_preview.schema_format SET version=6,source_digest=$1,manifest=$2 WHERE singleton=1",
    ).bind("2565cfd11e0788374407875f0490cdde315df809a6e36d027058fecba1378396")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_seven_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_six_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format7-browser.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in BROWSER_TABLES {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY tenant_scope ON {PREVIEW_SCHEMA}.{table} USING ({TENANT_POLICY}) WITH CHECK ({TENANT_POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql("SELECT set_config('zuno.tenant_id','migration-fixture',true);")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    let identity = zuno_identity::login_state::BrowserSessionRecord {
        token_hash: [0xab; 32],
        issuer: "https://identity.example".to_owned(),
        tenant_id: zuno_types::identity::TenantId::new("migration-fixture").expect("fixture"),
        principal_id: zuno_types::identity::PrincipalId::new("owner").expect("fixture"),
        client_id: zuno_types::identity::ClientId::new("enterprise-web").expect("fixture"),
        oauth_client_id: "enterprise-web".to_owned(),
        expires_at: 4102444800,
    };
    sqlx_core::query::query(
        "INSERT INTO zuno_enterprise_preview.browser_session(tenant_id,token_hash,principal_id,expires_at,identity)
         VALUES('migration-fixture',$1,'owner',4102444800,$2)",
    ).bind(identity.token_hash.as_slice()).bind(serde_json::json!(identity))
        .execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query(
        "UPDATE zuno_enterprise_preview.schema_format SET version=7,source_digest=$1,manifest=$2 WHERE singleton=1",
    ).bind("586f151d533db57c4e742ef5c828bff69edbf80d94f14747e9f0536fa8e779e1")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_eight_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_seven_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Captured from enterprise process baseline 7d74412a; never edit this DDL
    // when changing the current format.
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format8-operation.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in ["gateway_operation", "gateway_operation_attempt"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(
        "SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true);",
    ).execute(&mut *tx).await.map_err(database_error)?;
    use zuno_application::{environment::*, runtime::ExecutionLease};
    use zuno_types::identity::*;
    let owner = PrincipalKey {
        tenant_id: TenantId::new("migration-fixture").unwrap(),
        principal_id: PrincipalId::new("owner").unwrap(),
    };
    let admission=OperationAdmission {
        gateway_id:GatewayId::new("legacy-gateway").unwrap(),
        lease:ExecutionLease {
            owner:owner.clone(),job_id:JobId::new("legacy-job").unwrap(),session_id:SessionId::new("legacy-session").unwrap(),
            attempt_id:ExecutionAttemptId::new("legacy-attempt").unwrap(),worker:WorkerInstanceId::new("legacy-worker").unwrap(),
            epoch:7,checkpoint_version:2,expires_at_ms:4102444800000,
        },
        environment:Environment {
            owner:owner.clone(),revision:1,spec:EnvironmentSpec {
                id:EnvironmentId::new("legacy-session").unwrap(),session_id:SessionId::new("legacy-session").unwrap(),
                image:"fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                memory_bytes:67108864,pids_limit:32,cpu_millis:500,
            },
        },
        operation:CommandOperation {
            id:OperationId::new("legacy-operation").unwrap(),invocation_id:InvocationId::new("legacy-pending").unwrap(),
            environment_id:EnvironmentId::new("legacy-session").unwrap(),expected_revision:1,
            argv:vec!["printf".to_owned(),"preserved".to_owned()],
        },
    };
    let digest = zuno_orchestration::sha256_json(&serde_json::json!([
        admission.gateway_id,
        owner,
        admission.lease.job_id,
        admission.lease.session_id,
        admission.environment,
        admission.operation,
    ]));
    sqlx_core::query::query(
        "INSERT INTO zuno_enterprise_preview.gateway_operation(
           tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,admission,admission_digest,time_admitted)
         VALUES('migration-fixture','owner','legacy-operation','legacy-gateway','legacy-job','legacy-session',
           'legacy-pending',$1,$2,1007)",
    ).bind(serde_json::json!(admission)).bind(digest).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=8,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("9a9d1f8ab3aa8879f0a2a4ac10888acba4a91aa44c7ad3365ea1498a56af0583")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_nine_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_eight_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Frozen from e6311b66, independently of later Memory or runtime DDL.
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format9-memory.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in [
        "memory_policy",
        "session_memory_policy",
        "memory_document",
        "memory_revision",
        "memory_candidate",
        "memory_evidence",
        "memory_provenance",
        "memory_retired",
        "learning_job",
        "memory_maintenance_state",
        "memory_request",
        "memory_audit",
    ] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql("SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true)")
        .execute(&mut *tx).await.map_err(database_error)?;
    let entries = serde_json::json!(["preserved private Memory"]);
    let document = serde_json::json!({
        "path":"global","scope":"global","revision":1,"entries":entries,
        "contentDigest":zuno_orchestration::sha256_json(&entries),"projectedRevision":0,
        "projectionError":null,"timeCreated":1008,"timeUpdated":1008
    });
    sqlx_core::query::query("INSERT INTO zuno_enterprise_preview.memory_document(tenant_id,principal_id,key,scope,revision,data)
        VALUES('migration-fixture','owner','global','global',1,$1)")
        .bind(&document).execute(&mut *tx).await.map_err(database_error)?;
    sqlx_core::query::query("INSERT INTO zuno_enterprise_preview.memory_revision(tenant_id,principal_id,key,revision,entries,content_digest,operation,time_created)
        VALUES('migration-fixture','owner','global',1,$1,$2,'adopt',1008)")
        .bind(&entries).bind(document["contentDigest"].as_str().unwrap()).execute(&mut *tx).await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("INSERT INTO zuno_enterprise_preview.memory_policy(tenant_id,principal_id,revision,use_memories,generate_private)
        VALUES('migration-fixture','owner',2,true,false)").execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=9,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("c983398eaec017b83ff3d1c4d970b9e80c3421eae47eacaef4fba4d8a2a49883")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_ten_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_nine_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format10-child.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.runtime_child ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.runtime_child FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.runtime_child USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.runtime_child FROM PUBLIC;
         GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.runtime_child TO \"{role}\";"
    ))).execute(&mut *tx).await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true)")
        .execute(&mut *tx).await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql("INSERT INTO zuno_enterprise_preview.session(tenant_id,principal_id,id,workspace_id,title,parent_id,time_created,time_updated)
        VALUES('migration-fixture','owner','legacy-published-child','legacy-workspace','Preserved child','legacy-session',1008,1008)")
        .execute(&mut *tx).await.map_err(database_error)?;
    let invocation = serde_json::json!({
        "invocationId":"legacy-child-invocation","argumentsSha256":"c".repeat(64),"logicalKey":"legacy-child",
        "prompt":"Preserved child request","description":"Preserved child","delivery":"foreground","resumeSessionId":null,"presentation":{}
    });
    let reference = serde_json::json!({
        "id":"legacy-child-wait","turnId":"legacy-turn","invocationId":"legacy-child-invocation","argumentsSha256":"c".repeat(64),
        "target":{"kind":"child","job_id":"legacy-child-job"},"continuation":"current_turn"
    });
    sqlx_core::query::query("INSERT INTO zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id,parent_job_id,parent_session_id,
        child_session_id,invocation_id,logical_key,request_digest,invocation,definition,selection,reference,delivery,state,time_created,time_updated)
        VALUES('migration-fixture','owner','legacy-child-job','legacy-job','legacy-session','legacy-child-session','legacy-child-invocation',
        'legacy-child',$1,$2,$3,$4,$5,'foreground','staged',1009,1009)")
        .bind("d".repeat(64)).bind(invocation)
        .bind(serde_json::json!({"id":"child-config","version":1,"sha256":"a".repeat(64)}))
        .bind(serde_json::json!({"agent":"build","model":{"providerId":"fixture","modelId":"model"}})).bind(reference)
        .execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=10,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("84169c7d17c85cb8b6a8e18f168fdfdaf6211193a7a361c4249ab350a19a36d1").bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_eleven_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_ten_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    // Exact DDL from 4ebc99c2, independent of the current migration.
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format11-workspace.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.child_workspace_preparation ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.child_workspace_preparation FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.child_workspace_preparation USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.child_workspace_preparation FROM PUBLIC;
         GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.child_workspace_preparation TO \"{role}\";"
    ))).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=11,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("2075a43ccbd5d9f3c8e70c0d9bbbc87f337438c2a88d350cc12cf20b656156eb")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_twelve_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_eleven_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format12-control.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in [
        "runtime_control_request",
        "runtime_stop",
        "runtime_continuation",
        "gateway_cancellation_delivery",
    ] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(
        "SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true);
         UPDATE zuno_enterprise_preview.runtime_job SET phase='cancelled',active_attempt_id=NULL WHERE job_id='legacy-job';
         UPDATE zuno_enterprise_preview.agent_job SET status='cancelled',error='Cancelled',time_completed=1010 WHERE id='legacy-job';
         UPDATE zuno_enterprise_preview.runtime_session SET current_job_id=NULL,lease_job_id=NULL,lease_attempt_id=NULL,lease_worker_id=NULL,lease_expires=NULL;
         UPDATE zuno_enterprise_preview.runtime_attempt SET state='lost',finished_at=1010 WHERE job_id='legacy-job';
         INSERT INTO zuno_enterprise_preview.runtime_stop(tenant_id,principal_id,job_id,root_job_id,time_requested)
           VALUES('migration-fixture','owner','legacy-job','legacy-job',1010);
         INSERT INTO zuno_enterprise_preview.gateway_cancellation_delivery(tenant_id,principal_id,operation_id,time_polled)
           VALUES('migration-fixture','owner','legacy-operation',1011);"
    ).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=12,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("18ba9887175488c8303e1ddf5145d3ce5b867156d691b2243c531c73781a2c1a")
        .bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_thirteen_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_twelve_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format13-activity.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in ["activity_session", "activity_item", "activity_frame"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    sqlx_core::raw_sql::raw_sql(r#"SELECT set_config('zuno.tenant_id','migration-fixture',true),set_config('zuno.principal_id','owner',true);
        UPDATE zuno_enterprise_preview.activity_session SET sequence=1 WHERE session_id='legacy-session';
        INSERT INTO zuno_enterprise_preview.activity_item(tenant_id,principal_id,session_id,id,position,revision,record)
        VALUES('migration-fixture','owner','legacy-session','message:preserved',1,1,
          '{"id":"message:preserved","parentId":null,"createdAt":"1000","actions":[],"item":{"kind":"thinking","text":"Preserved public summary","collapsed":true,"truncated":false}}');
        INSERT INTO zuno_enterprise_preview.activity_frame(tenant_id,principal_id,session_id,sequence,item_id,version,record)
        SELECT tenant_id,principal_id,session_id,revision,id,1,record FROM zuno_enterprise_preview.activity_item;"#)
        .execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=13,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("6387ea3ee6663b640fc8ad83bbc798356e9dc20ce77f839337a3235b47fb00e3").bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_fourteen_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_thirteen_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format14-live.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.live_progress ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.live_progress FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.live_progress USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.live_progress FROM PUBLIC;
         GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.live_progress TO \"{role}\";"
    ))).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=14,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("ef6f3cedfec964d194adebfe095906e77c33e8d4f8715fe49cd14a281d14aad6").bind(manifest).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_fifteen_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_fourteen_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format15-workflow.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in ["runtime_workflow", "runtime_workflow_node"] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=15,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("412f668e781e311d6ddf76fd51a1314ace1b6423f1b33c67ceaac053ce40661c").bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_sixteen_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_fifteen_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format16-council.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in [
        "runtime_council",
        "runtime_council_seat",
        "runtime_council_attempt",
    ] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=16,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("b539bf863f4192d81cc8189ec65f2900b4fd589e7e8e00e34d1ff2e77d1464f1").bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_seventeen_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_sixteen_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format17-merge.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    for table in [
        "gateway_merge_operation",
        "gateway_merge_attempt",
        "gateway_merge_cancellation",
    ] {
        sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
            "ALTER TABLE {PREVIEW_SCHEMA}.{table} ENABLE ROW LEVEL SECURITY;
             ALTER TABLE {PREVIEW_SCHEMA}.{table} FORCE ROW LEVEL SECURITY;
             CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.{table} USING ({POLICY}) WITH CHECK ({POLICY});
             REVOKE ALL ON {PREVIEW_SCHEMA}.{table} FROM PUBLIC;
             GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.{table} TO \"{role}\";"
        ))).execute(&mut *tx).await.map_err(database_error)?;
    }
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=17,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("0283f316583aeff2c9da642747f54430c9fd1035a5183355d9a2b86deb3aeebc").bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}

#[cfg(test)]
pub(crate) async fn install_format_eighteen_fixture(
    pool: &PgPool,
    role: &str,
) -> Result<(), ApplicationError> {
    install_format_seventeen_fixture(pool, role).await?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(include_str!("fixtures/format18-import.sql"))
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    sqlx_core::raw_sql::raw_sql(AssertSqlSafe(format!(
        "ALTER TABLE {PREVIEW_SCHEMA}.workspace_import ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {PREVIEW_SCHEMA}.workspace_import FORCE ROW LEVEL SECURITY;
         CREATE POLICY owner_scope ON {PREVIEW_SCHEMA}.workspace_import USING ({POLICY}) WITH CHECK ({POLICY});
         REVOKE ALL ON {PREVIEW_SCHEMA}.workspace_import FROM PUBLIC;
         GRANT SELECT,INSERT,UPDATE,DELETE ON {PREVIEW_SCHEMA}.workspace_import TO \"{role}\";"
    ))).execute(&mut *tx).await.map_err(database_error)?;
    let manifest = schema_manifest(&mut tx).await?;
    sqlx_core::query::query("UPDATE zuno_enterprise_preview.schema_format SET version=18,source_digest=$1,manifest=$2 WHERE singleton=1")
        .bind("d7fc4601474c99b0d759cd26c5818b891af72d4e6e751fc9edd4c90614e02e14").bind(manifest)
        .execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)
}
