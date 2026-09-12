use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_nine_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_nine_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_nine_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar(
            "SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)-'delegation_depth_limit') FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg((to_jsonb(m)-'execution_job_id')) FROM zuno_enterprise_preview.message m),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)-'deadline_at') FROM zuno_enterprise_preview.runtime_job j),
            'slots',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.runtime_session s),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'policy',(SELECT jsonb_agg(to_jsonb(p)) FROM zuno_enterprise_preview.memory_policy p)
        )",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_child_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='ALTER TABLE' AND EXISTS(SELECT 1 FROM pg_constraint WHERE conname='runtime_job_execution'
          AND connamespace='zuno_enterprise_preview'::regnamespace) THEN RAISE EXCEPTION 'injected child migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_child_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_child_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql(
        "DROP EVENT TRIGGER refuse_child_migration; DROP FUNCTION public.refuse_child_migration();",
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
        9
    );
    assert!(
        !query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_constraint WHERE conname='runtime_job_execution'
        AND connamespace='zuno_enterprise_preview'::regnamespace)"
        )
        .fetch_one(&admin)
        .await
        .unwrap()
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
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
