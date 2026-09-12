use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_six_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        let prefix = options
            .url
            .strip_suffix("/postgres")
            .expect("isolated fixture database");
        options.url = format!("{prefix}/zuno_format_six_fixture");
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_six_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar(
            "SELECT jsonb_build_object(
              'session',(SELECT to_jsonb(s)-'delegation_depth_limit' FROM zuno_enterprise_preview.session s WHERE id='legacy-session'),
              'message',(SELECT (to_jsonb(m)-'execution_job_id') FROM zuno_enterprise_preview.message m WHERE id='legacy-assistant'),
              'part',(SELECT to_jsonb(p) FROM zuno_enterprise_preview.part p WHERE id='legacy-tool'),
              'wait',(SELECT to_jsonb(w) FROM zuno_enterprise_preview.runtime_wait w WHERE id='legacy-wait'),
              'job',(SELECT to_jsonb(j)-'deadline_at' FROM zuno_enterprise_preview.runtime_job j WHERE job_id='legacy-job'),
              'receipt',(SELECT to_jsonb(r) FROM zuno_enterprise_preview.input_execution_receipt r WHERE input_id='legacy-input')
            )",
        ).fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql(
        "CREATE FUNCTION public.zuno_refuse_browser_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.browser_login') IS NOT NULL
           THEN RAISE EXCEPTION 'injected browser migration failure'; END IF; END $$;
         CREATE EVENT TRIGGER zuno_refuse_browser_migration ON ddl_command_start EXECUTE FUNCTION public.zuno_refuse_browser_migration();",
    ).execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER zuno_refuse_browser_migration; DROP FUNCTION public.zuno_refuse_browser_migration()")
        .execute(&admin).await.unwrap();
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        6
    );
    assert!(
        query_scalar::<_, bool>(
            "SELECT to_regclass('zuno_enterprise_preview.browser_login') IS NULL"
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
    assert!(query_scalar::<_,bool>(
        "SELECT relrowsecurity AND relforcerowsecurity FROM pg_class WHERE oid='zuno_enterprise_preview.browser_session'::regclass",
    ).fetch_one(&admin).await.unwrap());
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
