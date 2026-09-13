use super::*;
pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_nineteen_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_nineteen_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_nineteen_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    query("INSERT INTO zuno_enterprise_preview.memory_policy
        (tenant_id,principal_id,revision,use_memories,generate_private)
        VALUES('migration-fixture','owner',7,true,true)
        ON CONFLICT(tenant_id,principal_id) DO UPDATE SET revision=7,use_memories=true,generate_private=true")
        .execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar(
            "SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
            'policy',(SELECT jsonb_agg(jsonb_build_object('tenant',tenant_id,'owner',principal_id,
                'revision',revision,'use',use_memories,'generate',generate_private))
                FROM zuno_enterprise_preview.memory_policy))",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_learning_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='ALTER TABLE' AND EXISTS(SELECT 1 FROM pg_attribute
          WHERE attrelid='zuno_enterprise_preview.memory_policy'::regclass AND attname='automatic_private' AND NOT attisdropped)
        THEN RAISE EXCEPTION 'injected learning migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_learning_migration ON ddl_command_end EXECUTE FUNCTION public.refuse_learning_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_learning_migration; DROP FUNCTION public.refuse_learning_migration();")
        .execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        19
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
    let policy: Value = query_scalar(
        "SELECT to_jsonb(p) FROM zuno_enterprise_preview.memory_policy p
        WHERE tenant_id='migration-fixture' AND principal_id='owner'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        policy["automatic_private"], false,
        "existing foreground consent cannot become automatic authority"
    );
    assert!(policy["automation_actor"].is_null());
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
