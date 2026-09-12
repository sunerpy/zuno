use super::*;
pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_sixteen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_sixteen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_sixteen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    let plan = json!({"retained":"Council definition"});
    query("INSERT INTO zuno_enterprise_preview.runtime_workflow
        (tenant_id,principal_id,run_id,job_id,parent_job_id,parent_session_id,plan,plan_digest,state,time_created,time_updated)
        VALUES('migration-fixture','owner','preserved-council','legacy-child-job','legacy-job','legacy-session',$1,$2,'active',1010,1010)")
        .bind(&plan).bind(zuno_orchestration::sha256_json(&plan)).execute(&admin).await.unwrap();
    raw_sql("INSERT INTO zuno_enterprise_preview.runtime_council(tenant_id,principal_id,run_id,state,started_at,seat_deadline_at,deadline_at)
        VALUES('migration-fixture','owner','preserved-council','seats',1000,2000,3000);
        UPDATE zuno_enterprise_preview.runtime_job SET deadline_at=3000 WHERE job_id='legacy-job';")
        .execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'frames',(SELECT jsonb_agg(to_jsonb(f)) FROM zuno_enterprise_preview.activity_frame f),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
            'councils',(SELECT jsonb_agg(to_jsonb(c)) FROM zuno_enterprise_preview.runtime_council c))").fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_merge_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.gateway_merge_operation') IS NOT NULL
        THEN RAISE EXCEPTION 'injected merge migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_merge_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_merge_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql(
        "DROP EVENT TRIGGER refuse_merge_migration; DROP FUNCTION public.refuse_merge_migration();",
    )
    .execute(&admin)
    .await
    .unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        16
    );
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
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.gateway_merge_operation"
        )
        .fetch_one(&admin)
        .await
        .unwrap(),
        0
    );
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
