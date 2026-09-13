use super::*;
pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_twenty_five_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_twenty_five_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_twenty_five_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    raw_sql("INSERT INTO zuno_enterprise_preview.learning_root_scan(tenant_id,principal_id,root_job_id,source_version,scanned_version,updated_at)
        VALUES('migration-fixture','owner','legacy-job',3,2,1010);").execute(&admin).await.unwrap();
    raw_sql("INSERT INTO zuno_enterprise_preview.gateway_edit_operation
        (tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,offer,offer_digest,time_created)
        SELECT tenant_id,principal_id,'legacy-edit','gateway',job_id,session_id,'legacy-invocation','{}',repeat('e',64),1000
        FROM zuno_enterprise_preview.runtime_job WHERE job_id='legacy-job';")
        .execute(&admin).await.unwrap();
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.gateway_edit_operation"
        )
        .fetch_one(&admin)
        .await
        .unwrap(),
        1
    );
    raw_sql("INSERT INTO zuno_enterprise_preview.gateway_mcp_operation
        (tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,offer,offer_digest,time_created)
        SELECT tenant_id,principal_id,'legacy-mcp','gateway',job_id,session_id,'legacy-mcp-call','{}',repeat('c',64),1000
        FROM zuno_enterprise_preview.runtime_job WHERE job_id='legacy-job';")
        .execute(&admin).await.unwrap();
    query("INSERT INTO zuno_enterprise_preview.shared_memory_space
        (tenant_id,id,workspace_id,title,enabled,policy_revision,document_revision,entries,content_digest,character_limit)
        VALUES('migration-fixture','legacy-space','workspace','Retained shared Memory',true,2,3,$1,$2,3000)")
        .bind(json!(["retained shared note"]))
        .bind(zuno_orchestration::sha256_json(&json!(["retained shared note"])))
        .execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'scans',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.learning_root_scan s),
            'edits',(SELECT jsonb_agg(to_jsonb(e)) FROM zuno_enterprise_preview.gateway_edit_operation e),
            'mcp',(SELECT jsonb_agg(to_jsonb(o)) FROM zuno_enterprise_preview.gateway_mcp_operation o),
            'shared',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.shared_memory_space s))")
            .fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_skill_schema() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.skill_candidate') IS NOT NULL
        THEN RAISE EXCEPTION 'injected Skill migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_skill_schema ON ddl_command_start EXECUTE FUNCTION public.refuse_skill_schema();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_skill_schema; DROP FUNCTION public.refuse_skill_schema();")
        .execute(&admin)
        .await
        .unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        25
    );
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.skill_candidate")
            .fetch_one(&admin)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        migration::FORMAT
    );
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
