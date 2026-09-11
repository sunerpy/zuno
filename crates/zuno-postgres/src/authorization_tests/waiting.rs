use super::*;
use zuno_engine::wait::{WaitOutcome, decode_completion};
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    for early in [false, true] {
        let f = fixture(
            backend,
            admin,
            migrator,
            if early {
                "approval-early"
            } else {
                "approval-wait"
            },
        )
        .await;
        let proposal = proposal(&f, "waiting", EffectKind::Process);
        let approval = f.authority.admit(&f.lease, proposal.clone()).await.unwrap();
        assert_eq!(approval.state, ApprovalState::Pending);
        let reference = WaitRef {
            id: WaitId::new("approval-wait").unwrap(),
            turn_id: f.job.turn_id.clone(),
            invocation_id: proposal.binding.invocation_id.clone(),
            arguments_sha256: "c".repeat(64),
            target: WaitTarget::Approval {
                approval_id: approval.id.clone(),
            },
            continuation: WaitContinuation::CurrentTurn,
        };
        if early {
            f.authority
                .answer(&f.owner, answer(&approval.id, "decide"))
                .await
                .unwrap();
        }
        let mut tx = crate::owner_transaction(&backend.pool, &f.owner.owner())
            .await
            .unwrap();
        let ready =
            crate::runtime::waiting::register(&mut tx, &f.job, std::slice::from_ref(&reference))
                .await
                .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            ready, early,
            "registration must observe an earlier approval answer"
        );
        if !early {
            sqlx_core::raw_sql::raw_sql(
                "CREATE FUNCTION public.zuno_refuse_approval_wake() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'injected approval wake failure'; END $$;
                 CREATE TRIGGER refuse_approval_wake BEFORE UPDATE ON zuno_enterprise_preview.runtime_wait
                   FOR EACH ROW WHEN(OLD.tenant_id='approval-wait') EXECUTE FUNCTION public.zuno_refuse_approval_wake();",
            ).execute(admin).await.unwrap();
            assert!(
                f.authority
                    .answer(&f.owner, answer(&approval.id, "decide"))
                    .await
                    .is_err()
            );
            assert_eq!(
                f.authority
                    .approval(&f.owner, &approval.id)
                    .await
                    .unwrap()
                    .state,
                ApprovalState::Pending
            );
            sqlx_core::raw_sql::raw_sql(
                "DROP TRIGGER refuse_approval_wake ON zuno_enterprise_preview.runtime_wait; DROP FUNCTION public.zuno_refuse_approval_wake()",
            ).execute(admin).await.unwrap();
            f.authority
                .answer(&f.owner, answer(&approval.id, "decide"))
                .await
                .unwrap();
        }
        let state: String = query_scalar(
            "SELECT state FROM zuno_enterprise_preview.runtime_wait WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(f.owner.tenant_id().as_str()).bind(f.owner.principal_id().as_str()).bind(reference.id.as_str())
            .fetch_one(admin).await.unwrap();
        assert_eq!(
            state, "ready",
            "an approval answer and its wait readiness must commit together"
        );
        let event = query(
            "SELECT e.* FROM zuno_enterprise_preview.event e JOIN zuno_enterprise_preview.runtime_wait w
             ON w.tenant_id=e.tenant_id AND w.principal_id=e.principal_id AND w.completion_event_id=e.id
             WHERE w.tenant_id=$1 AND w.principal_id=$2 AND w.id=$3",
        ).bind(f.owner.tenant_id().as_str()).bind(f.owner.principal_id().as_str()).bind(reference.id.as_str())
            .fetch_one(admin).await.unwrap();
        let completion =
            decode_completion(crate::turn::decode_event(event).unwrap(), &reference).unwrap();
        assert_eq!(completion.outcome, WaitOutcome::RecheckInvocation);
        f.authority
            .answer(&f.owner, answer(&approval.id, "decide"))
            .await
            .unwrap();
        assert_eq!(query_scalar::<_,i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND type='runtime.wait.completed'",
        ).bind(f.owner.tenant_id().as_str()).bind(f.owner.principal_id().as_str()).fetch_one(admin).await.unwrap(),1);
    }
}
