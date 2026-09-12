use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_fourteen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_fourteen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_fourteen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    query("INSERT INTO zuno_enterprise_preview.live_progress
        (tenant_id,principal_id,job_id,session_id,attempt_id,epoch,generation,sequence,body,body_digest,time_updated)
        VALUES('migration-fixture','owner','legacy-job','legacy-session','legacy-attempt',7,'legacy-generation',1,$1,$2,1003)")
        .bind(json!({"version":1,"retained":"legacy live bytes"})).bind(zuno_orchestration::sha256_json(&json!({"version":1,"retained":"legacy live bytes"})))
        .execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar(
            "SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'frames',(SELECT jsonb_agg(to_jsonb(f)) FROM zuno_enterprise_preview.activity_frame f),
            'live',(SELECT jsonb_agg(to_jsonb(l)) FROM zuno_enterprise_preview.live_progress l))",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_workflow_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE TABLE' AND to_regclass('zuno_enterprise_preview.runtime_workflow') IS NOT NULL
        THEN RAISE EXCEPTION 'injected workflow migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_workflow_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_workflow_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_workflow_migration; DROP FUNCTION public.refuse_workflow_migration();")
        .execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        14
    );
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.runtime_workflow_node"
        )
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
        15
    );
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
