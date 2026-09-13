use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_twenty_one_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_twenty_one_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_twenty_one_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    raw_sql("INSERT INTO zuno_enterprise_preview.learning_job
        (tenant_id,principal_id,id,workspace_id,session_id,kind,status,payload,time_updated)
        VALUES('migration-fixture','owner','learning-old','legacy-workspace','legacy-session','extraction','skipped','{}',1010);
        INSERT INTO zuno_enterprise_preview.learning_execution
        (tenant_id,principal_id,job_id,source_job_id,phase,principal,configuration,input,input_digest,limits,context,ready_at,created_at)
        VALUES('migration-fixture','owner','learning-old','legacy-job','extraction','{}','{}','{}',repeat('a',64),
            '{\"totalTokens\":10000}',
            '{\"sources\":[{\"origin\":{\"kind\":\"user_input\",\"sessionId\":\"legacy-session\",\"inputId\":\"legacy-input\"},\"sourceDigest\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}]}',
            1010,1010);").execute(&admin).await.unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)) FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.message m),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'learning',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.learning_job j),
            'execution',(SELECT jsonb_agg(to_jsonb(e)) FROM zuno_enterprise_preview.learning_execution e))")
            .fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_learning_sources() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE INDEX' AND to_regclass('zuno_enterprise_preview.learning_source_claim') IS NOT NULL
        THEN RAISE EXCEPTION 'injected learning source failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_learning_sources ON ddl_command_start EXECUTE FUNCTION public.refuse_learning_sources();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_learning_sources; DROP FUNCTION public.refuse_learning_sources();")
        .execute(&admin).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        21
    );
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    let claims: Value = query_scalar("SELECT jsonb_agg(jsonb_build_object('root',root_job_id,'job',learning_job_id,'state',disposition))
        FROM zuno_enterprise_preview.learning_source_claim").fetch_one(&admin).await.unwrap();
    assert_eq!(
        claims,
        json!([{"root":"legacy-job","job":"learning-old","state":"captured"}])
    );
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.learning_source_claim"
        )
        .fetch_one(&admin)
        .await
        .unwrap(),
        1
    );
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
