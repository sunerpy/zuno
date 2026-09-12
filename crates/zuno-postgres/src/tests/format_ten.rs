use super::*;

pub(super) async fn upgrade(fixture: &Fixture, admin: &PgPool) {
    raw_sql("CREATE DATABASE zuno_format_ten_fixture OWNER zuno_preview_migrator")
        .execute(admin)
        .await
        .unwrap();
    fn database(mut options: PostgresOptions) -> PostgresOptions {
        options.url = format!(
            "{}/zuno_format_ten_fixture",
            options.url.strip_suffix("/postgres").unwrap()
        );
        options
    }
    let migrator = database(fixture.migration_options())
        .connect()
        .await
        .unwrap();
    let admin = database(fixture.options(true, 2)).connect().await.unwrap();
    migration::install_format_ten_fixture(&migrator, &fixture.runtime_role)
        .await
        .unwrap();
    async fn snapshot(pool: &PgPool) -> Value {
        query_scalar("SELECT jsonb_build_object(
            'sessions',(SELECT jsonb_agg(to_jsonb(s)-'delegation_depth_limit') FROM zuno_enterprise_preview.session s),
            'messages',(SELECT jsonb_agg((to_jsonb(m)-'execution_job_id')) FROM zuno_enterprise_preview.message m),
            'jobs',(SELECT jsonb_agg(to_jsonb(j)) FROM zuno_enterprise_preview.runtime_job j),
            'memory',(SELECT jsonb_agg(to_jsonb(m)) FROM zuno_enterprise_preview.memory_document m),
            'children',(SELECT jsonb_agg(to_jsonb(c)-'workspace_policy'-'workspace_state'-'delegation_depth_limit') FROM zuno_enterprise_preview.runtime_child c)
        )").fetch_one(pool).await.unwrap()
    }
    let before = snapshot(&admin).await;
    raw_sql("CREATE FUNCTION public.refuse_workspace_migration() RETURNS event_trigger LANGUAGE plpgsql AS $$
        BEGIN IF TG_TAG='CREATE TABLE' AND EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='zuno_enterprise_preview'
          AND table_name='runtime_child' AND column_name='workspace_policy') THEN RAISE EXCEPTION 'injected workspace migration failure'; END IF; END $$;
        CREATE EVENT TRIGGER refuse_workspace_migration ON ddl_command_start EXECUTE FUNCTION public.refuse_workspace_migration();")
        .execute(&admin).await.unwrap();
    assert!(migrate(&migrator, &fixture.runtime_role).await.is_err());
    raw_sql("DROP EVENT TRIGGER refuse_workspace_migration; DROP FUNCTION public.refuse_workspace_migration();").execute(&admin).await.unwrap();
    assert_eq!(
        query_scalar::<_, i32>("SELECT version FROM zuno_enterprise_preview.schema_format")
            .fetch_one(&admin)
            .await
            .unwrap(),
        10
    );
    assert!(!query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='zuno_enterprise_preview'
        AND table_name='runtime_child' AND column_name='workspace_policy')").fetch_one(&admin).await.unwrap());
    assert_eq!(snapshot(&admin).await, before);
    migrate(&migrator, &fixture.runtime_role).await.unwrap();
    assert_eq!(snapshot(&admin).await, before);
    assert_eq!(query_scalar::<_,String>("SELECT workspace_state FROM zuno_enterprise_preview.runtime_child WHERE job_id='legacy-child-job'")
        .fetch_one(&admin).await.unwrap(),"model_only");
    assert_eq!(query_scalar::<_,i32>("SELECT delegation_depth_limit FROM zuno_enterprise_preview.session WHERE id='legacy-session'")
        .fetch_one(&admin).await.unwrap(),16);
    assert_eq!(query_scalar::<_,i32>("SELECT delegation_depth_limit FROM zuno_enterprise_preview.session WHERE id='legacy-published-child'")
        .fetch_one(&admin).await.unwrap(),0,"legacy children cannot infer a wider inherited delegation grant");
    PostgresBackend::connect(database(fixture.options(false, 2)))
        .await
        .unwrap();
}
