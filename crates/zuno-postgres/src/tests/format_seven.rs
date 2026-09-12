use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_seven_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        let prefix = options
            .url
            .strip_suffix("/postgres")
            .expect("isolated fixture");
        options.url = format!("{prefix}/zuno_format_seven_fixture");
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_seven_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar(
            "SELECT jsonb_build_object(
              'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
              'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
              'parts',(SELECT jsonb_agg(to_jsonb(p)) FROM zuno_enterprise_preview.part p),
              'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
              'waits',(SELECT jsonb_agg(to_jsonb(w)) FROM zuno_enterprise_preview.runtime_wait w),
              'browser',(SELECT jsonb_agg(to_jsonb(b)) FROM zuno_enterprise_preview.browser_session b)
            )",
        ).fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql(
        "CREATE FUNCTION public.zuno_refuse_operation_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
         BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.gateway_operation') IS NOT NULL
           THEN RAISE EXCEPTION 'injected operation migration failure'; END IF; END $$;
         CREATE EVENT TRIGGER zuno_refuse_operation_migration ON ddl_command_start EXECUTE FUNCTION public.zuno_refuse_operation_migration();",
    ).execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER zuno_refuse_operation_migration; DROP FUNCTION public.zuno_refuse_operation_migration()")
        .execute(&admin).await.unwrap();
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        7
    );
    assert!(
        query_scalar::<_, bool>(
            "SELECT to_regclass('zuno_enterprise_preview.gateway_operation') IS NULL"
        )
        .fetch_one(&admin)
        .await
        .unwrap()
    );
    assert_eq!(snapshot(&admin).await, before);
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
