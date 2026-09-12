use super::*;
use sqlx_core::raw_sql::raw_sql;
use zuno_application::environment::{
    CommandOperation, Environment, EnvironmentSpec, OperationAdmission, OperationCompletion,
    OperationOutput, OperationPhase, OperationReceipt, OutputChannel,
};
use zuno_engine::wait::WaitOutcome;
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    for early in [false, true] {
        let tenant = if early {
            "operation-early"
        } else {
            "operation-wait"
        };
        let f = fixture(backend, admin, migrator, tenant).await;
        let environment = Environment {
            owner: f.owner.owner(),
            spec: EnvironmentSpec {
                id: EnvironmentId::new("environment").unwrap(),
                session_id: f.job.session_id.clone(),
                image: format!("fixture@sha256:{}", "a".repeat(64)),
                memory_bytes: 64 * 1024 * 1024,
                pids_limit: 32,
                cpu_millis: 1000,
            },
            revision: 1,
        };
        let mut proposal = proposal(&f, "operation", EffectKind::Process);
        let operation = CommandOperation {
            id: proposal.binding.operation_id.clone(),
            invocation_id: proposal.binding.invocation_id.clone(),
            environment_id: environment.spec.id.clone(),
            expected_revision: 1,
            argv: vec!["inspect".to_owned()],
        };
        proposal.binding.arguments_sha256 = zuno_orchestration::sha256_json(&json!(operation.argv));
        proposal.binding.resources_sha256 = zuno_orchestration::sha256_json(&json!([
            environment.owner,
            environment.spec,
            environment.revision,
        ]));
        let approval = f.authority.admit(&f.lease, proposal.clone()).await.unwrap();
        f.authority
            .answer(&f.owner, answer(&approval.id, "approve"))
            .await
            .unwrap();
        let gateway = GatewayId::new("gateway").unwrap();
        f.authority
            .check_gateway_execution(
                proposal,
                &OperationAdmission {
                    gateway_id: gateway.clone(),
                    lease: f.lease.clone(),
                    environment,
                    operation: operation.clone(),
                },
            )
            .await
            .unwrap();
        let result = OperationCompletion {
            lease: f.lease.clone(),
            operation: operation.clone(),
            receipt: OperationReceipt {
                id: operation.id.clone(),
                environment_id: operation.environment_id.clone(),
                phase: OperationPhase::Completed,
                exit_code: Some(0),
                cancellation_requested: false,
            },
            output: vec![OperationOutput {
                channel: OutputChannel::Stdout,
                bytes: b"durable result".to_vec(),
            }],
            output_truncated: false,
        };
        let reference = WaitRef {
            id: WaitId::new("operation-wait").unwrap(),
            turn_id: f.job.turn_id.clone(),
            invocation_id: operation.invocation_id.clone(),
            arguments_sha256: "c".repeat(64),
            target: WaitTarget::Operation {
                operation_id: operation.id.clone(),
            },
            continuation: WaitContinuation::CurrentTurn,
        };
        let store = backend.gateway_operations(gateway);
        if early {
            store.complete(&result).await.unwrap();
        }
        let mut tx = crate::owner_transaction(&backend.pool, &f.owner.owner())
            .await
            .unwrap();
        let ready =
            crate::runtime::waiting::register(&mut tx, &f.job, std::slice::from_ref(&reference))
                .await
                .unwrap();
        assert_eq!(ready, early);
        tx.commit().await.unwrap();
        if !early {
            raw_sql(
                "CREATE FUNCTION public.zuno_refuse_operation_ready() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN IF NEW.tenant_id='operation-wait' AND NEW.type='runtime.wait.completed'
                   THEN RAISE EXCEPTION 'injected completion wake failure'; END IF; RETURN NEW; END $$;
                 CREATE TRIGGER refuse_operation_ready BEFORE INSERT ON zuno_enterprise_preview.event
                   FOR EACH ROW EXECUTE FUNCTION public.zuno_refuse_operation_ready();",
            ).execute(admin).await.unwrap();
            assert!(store.complete(&result).await.is_err());
            assert!(
                store
                    .completion(&f.owner.owner(), &operation.id)
                    .await
                    .unwrap()
                    .is_none()
            );
            let state: String = query_scalar(
                "SELECT state FROM zuno_enterprise_preview.runtime_wait WHERE tenant_id=$1 AND id=$2",
            ).bind(tenant).bind(reference.id.as_str()).fetch_one(admin).await.unwrap();
            assert_eq!(
                state, "pending",
                "receipt and readiness must roll back together"
            );
            raw_sql("DROP TRIGGER refuse_operation_ready ON zuno_enterprise_preview.event; DROP FUNCTION public.zuno_refuse_operation_ready()")
                .execute(admin).await.unwrap();
            // The factual completion is valid even after the old Worker loses authority.
            query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id=$1")
                .bind(tenant).execute(admin).await.unwrap();
            store.complete(&result).await.unwrap();
        }
        store.complete(&result).await.unwrap();
        let mut tx = crate::owner_transaction(&backend.pool, &f.owner.owner())
            .await
            .unwrap();
        let completions = crate::runtime::waiting::ready_completions(&mut tx, &f.job, &[reference])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completions.len(), 1);
        let WaitOutcome::ToolResult { result } = &completions[0].outcome else {
            panic!("actual operation result")
        };
        assert!(!result.is_error);
        assert!(result.output.output.contains("durable result"));
        tx.rollback().await.unwrap();
        let count: i64 = query_scalar(
            "SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND type='runtime.wait.completed'",
        ).bind(tenant).fetch_one(admin).await.unwrap();
        assert_eq!(count, 1);
    }
}
