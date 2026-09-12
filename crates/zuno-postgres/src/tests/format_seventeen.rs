use super::*;
pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_seventeen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_seventeen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_seventeen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    let offer = json!({"preserved":"merge offer"});
    query("INSERT INTO zuno_enterprise_preview.gateway_merge_operation
        (tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,child_job_id,offer,offer_digest,time_created)
        VALUES('migration-fixture','owner','legacy-merge','gateway','legacy-job','legacy-session','merge-call','legacy-job',$1,$2,1010)")
        .bind(&offer).bind(zuno_orchestration::sha256_json(&offer)).execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
            'merge',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.gateway_merge_operation m))")
            .fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_import_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE INDEX' AND to_regclass('zuno_enterprise_preview.workspace_import') IS NOT NULL
        THEN RAISE EXCEPTION 'injected import migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_import_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_import_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_import_migration; DROP FUNCTION public.refuse_import_migration();").execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        17
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
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.workspace_import")
            .fetch_one(&admin)
            .await
            .unwrap(),
        0
    );
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
