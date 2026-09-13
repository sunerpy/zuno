use super::*;
pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_eighteen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_eighteen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_eighteen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    let assignment = json!({"preserved":"initial project"});
    query("INSERT INTO zuno_enterprise_preview.workspace_import
        (tenant_id,principal_id,id,session_id,request_digest,assignment,assignment_digest,state,time_created,time_updated)
        VALUES('migration-fixture','owner','legacy-import','legacy-session',$1,$2,$1,'uploading',1010,1010)")
        .bind(zuno_orchestration::sha256_json(&assignment)).bind(&assignment).execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
            'imports',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.workspace_import m))")
            .fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_transfer_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE INDEX' AND to_regclass('zuno_enterprise_preview.workspace_snapshot_transfer') IS NOT NULL
        THEN RAISE EXCEPTION 'injected transfer migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_transfer_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_transfer_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_transfer_migration; DROP FUNCTION public.refuse_transfer_migration();")
        .execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        18
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
            "SELECT count(*) FROM zuno_enterprise_preview.workspace_snapshot_transfer"
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
