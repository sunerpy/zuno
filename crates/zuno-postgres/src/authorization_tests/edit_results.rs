use super::*;
use serde_json::Value;
use sqlx_core::raw_sql::raw_sql;
use zuno_application::{
    environment::{Environment, EnvironmentSnapshot, EnvironmentSpec},
    workspace_edit::*,
    workspace_merge::WorkspacePath,
};
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "edit-results").await;
    let environment = Environment {
        owner: f.owner.owner(),
        revision: 1,
        spec: EnvironmentSpec {
            id: EnvironmentId::new("edit-env").unwrap(),
            session_id: f.job.session_id.clone(),
            image: format!("fixture@sha256:{}", "a".repeat(64)),
            memory_bytes: 67108864,
            pids_limit: 32,
            cpu_millis: 1000,
        },
    };
    let mut proposal = proposal(&f, "edit", EffectKind::FileWrite);
    proposal.facts.mandatory_human = true;
    let operation = WorkspaceEditOperation {
        id: proposal.binding.operation_id.clone(),
        invocation_id: proposal.binding.invocation_id.clone(),
        environment_id: environment.spec.id.clone(),
        expected_revision: 1,
        edits: vec![WorkspaceFileEdit {
            path: WorkspacePath::new("new.txt").unwrap(),
            expected: FileExpectation::Absent,
            content: Some("reviewed\n".to_owned()),
        }],
    };
    let admission = WorkspaceEditAdmission {
        gateway_id: GatewayId::new("gateway").unwrap(),
        lease: f.lease.clone(),
        base: EnvironmentSnapshot {
            id: EnvironmentSnapshotId::new("base").unwrap(),
            environment_id: environment.spec.id.clone(),
            revision: 1,
            sha256: "a".repeat(64),
            bytes: 1024,
        },
        environment,
        operation: operation.clone(),
        review: vec![WorkspaceEditReview {
            path: WorkspacePath::new("new.txt").unwrap(),
            before: None,
            after: Some("reviewed\n".to_owned()),
        }],
    };
    let store = backend.workspace_edits(admission.gateway_id.clone());
    store.offer(&admission).await.unwrap();
    proposal.binding.arguments_sha256 = admission.arguments_digest();
    proposal.binding.resources_sha256 = admission.resources_digest();
    let approval = f.authority.admit(&f.lease, proposal.clone()).await.unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    assert!(
        f.authority
            .check_workspace_edit_execution(proposal.clone(), &admission)
            .await
            .is_err()
    );
    assert!(
        backend
            .workspace_edit_for_approval(&f.outsider, &approval.id)
            .await
            .is_err()
    );
    assert_eq!(
        backend
            .workspace_edit_for_approval(&f.owner, &approval.id)
            .await
            .unwrap()
            .0
            .review,
        admission.review
    );
    let mut changed = admission.clone();
    changed.operation.edits[0].content = Some("different".to_owned());
    changed.review[0].after = Some("different".to_owned());
    assert!(
        store.offer(&changed).await.is_err(),
        "an existing operation cannot switch reviewed content"
    );
    f.authority
        .answer(&f.owner, answer(&approval.id, "approve-edit"))
        .await
        .unwrap();
    raw_sql("CREATE FUNCTION public.refuse_edit_attempt() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.tenant_id='edit-results' THEN RAISE EXCEPTION 'injected edit admission'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_edit_attempt BEFORE INSERT ON zuno_enterprise_preview.gateway_edit_attempt
        FOR EACH ROW EXECUTE FUNCTION public.refuse_edit_attempt();").execute(admin).await.unwrap();
    assert!(
        f.authority
            .check_workspace_edit_execution(proposal.clone(), &admission)
            .await
            .is_err()
    );
    assert!(!query_scalar::<_,bool>("SELECT admitted FROM zuno_enterprise_preview.gateway_edit_operation WHERE tenant_id='edit-results'")
        .fetch_one(admin).await.unwrap(),"approval admission and attempt must roll back together");
    raw_sql("DROP TRIGGER refuse_edit_attempt ON zuno_enterprise_preview.gateway_edit_attempt; DROP FUNCTION public.refuse_edit_attempt();")
        .execute(admin).await.unwrap();
    f.authority
        .check_workspace_edit_execution(proposal, &admission)
        .await
        .unwrap();
    let reference = WaitRef {
        id: WaitId::new("edit-wait").unwrap(),
        turn_id: f.job.turn_id.clone(),
        invocation_id: operation.invocation_id.clone(),
        arguments_sha256: "c".repeat(64),
        target: WaitTarget::Operation {
            operation_id: operation.id.clone(),
        },
        continuation: WaitContinuation::CurrentTurn,
    };
    let mut tx = crate::owner_transaction(&backend.pool, &f.owner.owner())
        .await
        .unwrap();
    crate::runtime::waiting::register(&mut tx, &f.job, std::slice::from_ref(&reference))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let completion = WorkspaceEditCompletion {
        admission: admission.clone(),
        receipt: WorkspaceEditReceipt {
            id: operation.id.clone(),
            environment_id: operation.environment_id.clone(),
            state: WorkspaceEditState::Committed,
            request_digest: operation.digest(),
            revision: 2,
        },
    };
    raw_sql("CREATE FUNCTION public.refuse_edit_ready() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.tenant_id='edit-results' AND NEW.type='runtime.wait.completed' THEN RAISE EXCEPTION 'injected edit wake'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_edit_ready BEFORE INSERT ON zuno_enterprise_preview.event FOR EACH ROW EXECUTE FUNCTION public.refuse_edit_ready();")
        .execute(admin).await.unwrap();
    assert!(store.complete(&completion).await.is_err());
    assert!(query_scalar::<_,Option<Value>>("SELECT completion FROM zuno_enterprise_preview.gateway_edit_operation WHERE tenant_id='edit-results'")
        .fetch_one(admin).await.unwrap().is_none());
    raw_sql("DROP TRIGGER refuse_edit_ready ON zuno_enterprise_preview.event; DROP FUNCTION public.refuse_edit_ready();").execute(admin).await.unwrap();
    query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id='edit-results'").execute(admin).await.unwrap();
    store.complete(&completion).await.unwrap();
    store.complete(&completion).await.unwrap();
    let mut forged = completion;
    forged.admission.lease.attempt_id = ExecutionAttemptId::new("unadmitted").unwrap();
    assert!(
        store.complete(&forged).await.is_err(),
        "late facts require an originally admitted attempt"
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id='edit-results' AND type='runtime.workspace_edit.completed'")
        .fetch_one(admin).await.unwrap(),1);
    cancellation(backend, admin, migrator).await;
}

async fn cancellation(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "edit-cancel").await;
    let environment = Environment {
        owner: f.owner.owner(),
        revision: 1,
        spec: EnvironmentSpec {
            id: EnvironmentId::new("edit-env").unwrap(),
            session_id: f.job.session_id.clone(),
            image: format!("fixture@sha256:{}", "a".repeat(64)),
            memory_bytes: 67108864,
            pids_limit: 32,
            cpu_millis: 1000,
        },
    };
    let mut proposal = proposal(&f, "cancel-edit", EffectKind::FileWrite);
    proposal.facts.mandatory_human = true;
    let operation = WorkspaceEditOperation {
        id: proposal.binding.operation_id.clone(),
        invocation_id: proposal.binding.invocation_id.clone(),
        environment_id: environment.spec.id.clone(),
        expected_revision: 1,
        edits: vec![WorkspaceFileEdit {
            path: WorkspacePath::new("file").unwrap(),
            expected: FileExpectation::Absent,
            content: Some("new".to_owned()),
        }],
    };
    let admission = WorkspaceEditAdmission {
        gateway_id: GatewayId::new("gateway").unwrap(),
        lease: f.lease.clone(),
        base: EnvironmentSnapshot {
            id: EnvironmentSnapshotId::new("base").unwrap(),
            environment_id: environment.spec.id.clone(),
            revision: 1,
            sha256: "b".repeat(64),
            bytes: 1024,
        },
        environment,
        operation: operation.clone(),
        review: vec![WorkspaceEditReview {
            path: WorkspacePath::new("file").unwrap(),
            before: None,
            after: Some("new".to_owned()),
        }],
    };
    let store = backend.workspace_edits(admission.gateway_id.clone());
    store.offer(&admission).await.unwrap();
    proposal.binding.arguments_sha256 = admission.arguments_digest();
    proposal.binding.resources_sha256 = admission.resources_digest();
    let approval = f.authority.admit(&f.lease, proposal.clone()).await.unwrap();
    f.authority
        .answer(&f.owner, answer(&approval.id, "approve"))
        .await
        .unwrap();
    f.authority
        .check_workspace_edit_execution(proposal.clone(), &admission)
        .await
        .unwrap();
    use zuno_application::control::{CancelJob, RuntimeControl};
    let cancelled = f
        .runtime
        .cancel(
            &f.owner,
            &f.job.id,
            CancelJob {
                request_id: RequestId::new("cancel").unwrap(),
                expected_turn_id: f.job.turn_id.clone(),
                reason: "Stop editing".to_owned(),
            },
        )
        .await
        .unwrap();
    assert!(cancelled.pending_operations.contains(&operation.id));
    let pending = store.cancellations(f.owner.tenant_id(), 32).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].lease, admission.lease,
        "cancellation carries an admitted attempt rather than an old preview lease"
    );
    assert!(
        f.authority
            .check_workspace_edit_execution(proposal, &admission)
            .await
            .is_err()
    );
    store
        .complete(&WorkspaceEditCompletion {
            admission,
            receipt: WorkspaceEditReceipt {
                id: operation.id.clone(),
                environment_id: operation.environment_id.clone(),
                state: WorkspaceEditState::Committed,
                request_digest: operation.digest(),
                revision: 2,
            },
        })
        .await
        .unwrap();
    assert_eq!(
        f.runtime
            .get(&f.owner.owner(), &f.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Cancelled
    );
    assert!(
        store
            .cancellations(f.owner.tenant_id(), 32)
            .await
            .unwrap()
            .is_empty()
    );
}
