use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_thirteen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_thirteen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_thirteen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'items',(SELECT jsonb_agg(to_jsonb(i)) FROM zuno_enterprise_preview.activity_item i),
            'frames',(SELECT jsonb_agg(to_jsonb(f)) FROM zuno_enterprise_preview.activity_frame f))")
            .fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_live_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE FUNCTION' AND to_regclass('zuno_enterprise_preview.live_progress') IS NOT NULL
        THEN RAISE EXCEPTION 'injected live migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_live_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_live_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql(
        "DROP EVENT TRIGGER refuse_live_migration; DROP FUNCTION public.refuse_live_migration();",
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
        13
    );
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.live_progress")
            .fetch_one(&admin)
            .await
            .unwrap(),
        0
    );
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
