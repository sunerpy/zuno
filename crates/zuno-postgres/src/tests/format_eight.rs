use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_eight_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_eight_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_eight_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)-'delegation_depth_limit') FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg((to_jsonb(m)-'execution_job_id')) FROM zuno_enterprise_preview.message m),
            'parts',(SELECT jsonb_agg(to_jsonb(p)) FROM zuno_enterprise_preview.part p),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
            'waits',(SELECT jsonb_agg(to_jsonb(w)) FROM zuno_enterprise_preview.runtime_wait w),
            'browser',(SELECT jsonb_agg(to_jsonb(b)) FROM zuno_enterprise_preview.browser_session b),
            'operations',(SELECT jsonb_agg(to_jsonb(o)) FROM zuno_enterprise_preview.gateway_operation o)
        )").fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_memory_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.memory_document') IS NOT NULL
           THEN RAISE EXCEPTION 'injected memory migration failure'; END IF; END $$;
         CREATE EVENT TRIGGER refuse_memory_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_memory_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_memory_migration; DROP FUNCTION public.refuse_memory_migration()")
        .execute(&admin).await.unwrap();
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        8
    );
    assert!(
        query_scalar::<_, bool>(
            "SELECT to_regclass('zuno_enterprise_preview.memory_document') IS NULL"
        )
        .fetch_one(&admin)
        .await
        .unwrap()
    );
    assert_eq!(snapshot(&admin).await, before);
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        migration::FORMAT
    );
    let policy: bool = query_scalar(
        "SELECT relrowsecurity AND relforcerowsecurity FROM pg_class
        WHERE oid='zuno_enterprise_preview.memory_document'::regclass",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert!(policy);
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
