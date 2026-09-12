use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_twelve_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_twelve_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_twelve_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
          'session',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
          'message',(SELECT jsonb_agg((to_jsonb(m)-'execution_job_id')) FROM zuno_enterprise_preview.message m),
          'part',(SELECT jsonb_agg(to_jsonb(p)) FROM zuno_enterprise_preview.part p),
          'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
          'stop',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.runtime_stop s),
          'delivery',(SELECT jsonb_agg(to_jsonb(d)) FROM zuno_enterprise_preview.gateway_cancellation_delivery d)
        )").fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_activity_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.activity_session') IS NOT NULL
        THEN RAISE EXCEPTION 'injected activity migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_activity_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_activity_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_activity_migration; DROP FUNCTION public.refuse_activity_migration();")
        .execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        12
    );
    assert!(
        query_scalar::<_, Option<String>>(
            "SELECT to_regclass('zuno_enterprise_preview.activity_session')::text"
        )
        .fetch_one(&admin)
        .await
        .unwrap()
        .is_none()
    );
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(
        snapshot(&admin).await,
        before,
        "projection backfill must preserve source bytes and private evidence"
    );
    assert!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.activity_item")
            .fetch_one(&admin)
            .await
            .unwrap()
            > 0
    );
    let public: Vec<Value> =
        query_scalar("SELECT record FROM zuno_enterprise_preview.activity_frame ORDER BY sequence")
            .fetch_all(&admin)
            .await
            .unwrap();
    for record in public {
        serde_json::from_value::<zuno_types::activity::ItemRecord>(record).unwrap();
    }
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
