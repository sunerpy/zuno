use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_fifteen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_fifteen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_fifteen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    let plan = json!({"retained":"original immutable workflow definition"});
    query("INSERT INTO zuno_enterprise_preview.runtime_workflow
        (tenant_id,principal_id,run_id,job_id,parent_job_id,parent_session_id,plan,plan_digest,state,time_created,time_updated)
        VALUES('migration-fixture','owner','preserved-run','legacy-child-job','legacy-job','legacy-session',$1,$2,'preparing',1010,1010)")
        .bind(&plan).bind(zuno_orchestration::sha256_json(&plan)).execute(&admin).await.unwrap();
    query("INSERT INTO zuno_enterprise_preview.runtime_workflow_node
        (tenant_id,principal_id,run_id,node_run_id,node_id,position,child_job_id,state,input_prompt,input_digest,input_sources,time_updated)
        VALUES('migration-fixture','owner','preserved-run','preserved-node','inspect',0,'legacy-child-job','pending','original node input',$1,'[]',1010)")
        .bind(zuno_orchestration::sha256_json(&json!(["preserved-run","legacy-child-job","original node input",[]])))
        .execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'frames',(SELECT jsonb_agg(to_jsonb(f)) FROM zuno_enterprise_preview.activity_frame f),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)-'deadline_at') FROM zuno_enterprise_preview.runtime_job j),
            'workflow',(SELECT jsonb_agg(to_jsonb(w)) FROM zuno_enterprise_preview.runtime_workflow w),
            'nodes',(SELECT jsonb_agg(to_jsonb(n)) FROM zuno_enterprise_preview.runtime_workflow_node n))")
            .fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_council_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.runtime_council') IS NOT NULL
        THEN RAISE EXCEPTION 'injected Council migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_council_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_council_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_council_migration; DROP FUNCTION public.refuse_council_migration();")
        .execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        15
    );
    assert!(!query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM information_schema.columns
        WHERE table_schema='zuno_enterprise_preview' AND table_name='runtime_job' AND column_name='deadline_at')")
        .fetch_one(&admin).await.unwrap());
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        migration::FORMAT
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.runtime_council")
            .fetch_one(&admin)
            .await
            .unwrap(),
        0
    );
    assert!(
        query_scalar::<_, bool>(
            "SELECT bool_and(deadline_at IS NULL) FROM zuno_enterprise_preview.runtime_job"
        )
        .fetch_one(&admin)
        .await
        .unwrap()
    );
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
