use super::*;
use zuno_application::{
    child::{
        ChildWorkspaceAssignment, ChildWorkspaceCompletion, ChildWorkspacePolicy,
        ChildWorkspaceState,
    },
    environment::{Environment, EnvironmentSnapshot, EnvironmentSpec},
};
use zuno_types::identity::{EnvironmentId, EnvironmentSnapshotId, GatewayId, PrincipalId};

fn spec(session: &SessionId) -> EnvironmentSpec {
    EnvironmentSpec {
        id: EnvironmentId::new(session.as_str()).unwrap(),
        session_id: session.clone(),
        image: format!("image@sha256:{}", "a".repeat(64)),
        memory_bytes: 67108864,
        pids_limit: 32,
        cpu_millis: 500,
    }
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    for late in [false, true] {
        let (actor, parent) = parent(
            backend,
            admin,
            if late {
                "workspace-late"
            } else {
                "workspace-ready"
            },
        )
        .await;
        let store = backend.runtime(actor.tenant_id().clone());
        let mut definition = grant(&parent.job);
        definition.workspace = ChildWorkspacePolicy::ForkParent;
        let intent = invocation(ChildDelivery::Foreground);
        let ticket = store
            .dispatch_child(&parent.lease, intent.clone(), &definition)
            .await
            .unwrap();
        assert_eq!(ticket.workspace, ChildWorkspaceState::Pending);
        let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
            .await
            .unwrap();
        assert!(
            crate::runtime::suspend_in(
                &mut tx,
                &parent.lease,
                checkpoint(&parent.job),
                std::slice::from_ref(&ticket.wait)
            )
            .await
            .is_err(),
            "the parent checkpoint cannot activate a child whose workspace is unprepared"
        );
        tx.rollback().await.unwrap();
        assert!(matches!(
            store.get(&actor.owner(), &ticket.job_id).await,
            Err(ApplicationError::NotFound)
        ));
        let info = store
            .child_workspace(&parent.lease, &ticket.job_id)
            .await
            .unwrap();
        assert_eq!(info.child_session_id, ticket.session_id);
        let assignment = ChildWorkspaceAssignment {
            child_job_id: ticket.job_id.clone(),
            gateway_id: GatewayId::new("gateway").unwrap(),
            parent: spec(&parent.job.session_id),
            target: spec(&ticket.session_id),
            resume: false,
        };
        store
            .admit_child_workspace(&parent.lease, &assignment)
            .await
            .unwrap();
        let mut changed = assignment.clone();
        changed.target.memory_bytes *= 2;
        assert!(matches!(
            store.admit_child_workspace(&parent.lease, &changed).await,
            Err(ApplicationError::Conflict)
        ));
        let completion = ChildWorkspaceCompletion {
            lease: parent.lease.clone(),
            receipt: zuno_application::child::ChildWorkspaceReceipt {
                child_job_id: ticket.job_id.clone(),
                parent_environment_id: assignment.parent.id.clone(),
                snapshot: Some(EnvironmentSnapshot {
                    id: EnvironmentSnapshotId::new(format!("child-{}", ticket.job_id)).unwrap(),
                    environment_id: assignment.parent.id.clone(),
                    revision: 1,
                    sha256: "b".repeat(64),
                    bytes: 1024,
                }),
                target: Environment {
                    owner: actor.owner(),
                    spec: assignment.target.clone(),
                    revision: 1,
                },
            },
        };
        assert!(matches!(
            store
                .complete_child_workspace(&GatewayId::new("wrong").unwrap(), &completion)
                .await,
            Err(ApplicationError::Forbidden)
        ));
        let mut wrong = completion.clone();
        wrong.receipt.target.owner.principal_id = PrincipalId::new("another-user").unwrap();
        assert!(matches!(
            store
                .complete_child_workspace(&assignment.gateway_id, &wrong)
                .await,
            Err(ApplicationError::Forbidden)
        ));
        if late {
            query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
                .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(parent.job.session_id.as_str()).execute(admin).await.unwrap();
        }
        store
            .complete_child_workspace(&assignment.gateway_id, &completion)
            .await
            .unwrap();
        store
            .complete_child_workspace(&assignment.gateway_id, &completion)
            .await
            .unwrap();
        let mut conflicting = completion.clone();
        conflicting.receipt.snapshot.as_mut().unwrap().sha256 = "c".repeat(64);
        assert!(matches!(
            store
                .complete_child_workspace(&assignment.gateway_id, &conflicting)
                .await,
            Err(ApplicationError::Conflict)
        ));
        if late {
            assert!(
                matches!(
                    store
                        .dispatch_child(&parent.lease, intent, &definition)
                        .await,
                    Err(ApplicationError::LeaseLost)
                ),
                "a late truthful receipt does not restore the old Worker's lease"
            );
            continue;
        }
        let prepared = store
            .dispatch_child(&parent.lease, intent, &definition)
            .await
            .unwrap();
        assert_eq!(prepared.workspace, ChildWorkspaceState::Ready);
        let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
            .await
            .unwrap();
        crate::runtime::suspend_in(
            &mut tx,
            &parent.lease,
            checkpoint(&parent.job),
            std::slice::from_ref(&prepared.wait),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let child = store
            .claim(&worker("prepared-child"), duration())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.job.id, ticket.job_id);
        let (_, receipt) = store
            .execution_workspace(&child.lease)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt, completion.receipt);
        assert!(
            store
                .workspace_merge_source(&child.lease, &child.job.id)
                .await
                .is_err()
        );
        complete_child(backend, admin, &child).await;
        let resumed = store
            .claim(&worker("merge-parent"), duration())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed.job.id, parent.job.id);
        let source = store
            .workspace_merge_source(&resumed.lease, &child.job.id)
            .await
            .unwrap();
        assert_eq!(source.base, completion.receipt.snapshot.clone().unwrap());
        assert_eq!(source.source_environment, assignment.target);
        assert_eq!(source.child_session_id, child.job.session_id);
        assert_eq!(source.gateway_id, assignment.gateway_id);
        merge_authorization(backend, admin, &resumed, &assignment, &source).await;
        assert!(
            store
                .workspace_merge_source(&resumed.lease, &resumed.job.id)
                .await
                .is_err()
        );
        query(
            "UPDATE zuno_enterprise_preview.runtime_session SET input_version=input_version+1
            WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
        )
        .bind(actor.tenant_id().as_str())
        .bind(actor.principal_id().as_str())
        .bind(child.job.session_id.as_str())
        .execute(admin)
        .await
        .unwrap();
        assert!(matches!(
            store
                .workspace_merge_source(&resumed.lease, &child.job.id)
                .await,
            Err(ApplicationError::Conflict)
        ));
    }
}

async fn merge_authorization(
    backend: &PostgresBackend,
    admin: &PgPool,
    parent: &zuno_application::runtime::ClaimedJob,
    assignment: &ChildWorkspaceAssignment,
    source: &zuno_application::workspace_merge::WorkspaceMergeSource,
) {
    use zuno_application::{authorization::*, workspace_merge::*};
    use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};
    use zuno_types::{
        activity::Counter,
        identity::{OperationId, WaitId},
        wait::{WaitContinuation, WaitRef, WaitTarget},
    };
    let actor = &parent.job.principal;
    let base = WorkspaceTree::new();
    let target: WorkspaceTree = [(
        WorkspacePath::new("result.txt").unwrap(),
        WorkspaceEntry::File {
            mode: 0o644,
            uid: 0,
            gid: 0,
            sha256: zuno_orchestration::sha256_text("result"),
            bytes: Counter(6),
        },
    )]
    .into();
    let operation = WorkspaceMergeOperation {
        id: OperationId::new("merge-test").unwrap(),
        invocation_id: InvocationId::new("merge-call").unwrap(),
        child_job_id: source.child_job_id.clone(),
        environment_id: assignment.parent.id.clone(),
        expected_revision: 1,
        base: source.base.clone(),
        parent: EnvironmentSnapshot {
            id: EnvironmentSnapshotId::new("merge-parent").unwrap(),
            environment_id: assignment.parent.id.clone(),
            revision: 1,
            sha256: "c".repeat(64),
            bytes: 1024,
        },
        child: EnvironmentSnapshot {
            id: EnvironmentSnapshotId::new("merge-child").unwrap(),
            environment_id: assignment.target.id.clone(),
            revision: 2,
            sha256: "d".repeat(64),
            bytes: 1024,
        },
        plan: plan(&base, &base, &target).unwrap(),
    };
    let admission = WorkspaceMergeAdmission {
        gateway_id: assignment.gateway_id.clone(),
        lease: parent.lease.clone(),
        environment: Environment {
            owner: actor.owner(),
            spec: assignment.parent.clone(),
            revision: 1,
        },
        source: source.clone(),
        operation: operation.clone(),
    };
    let merge = backend.workspace_merges(assignment.gateway_id.clone());
    merge.offer(&admission).await.unwrap();
    merge.offer(&admission).await.unwrap();
    let proposal = ApprovalProposal {
        binding: ApprovalBinding {
            job_id: parent.job.id.clone(),
            session_id: parent.job.session_id.clone(),
            turn_id: parent.job.turn_id.clone(),
            invocation_id: operation.invocation_id.clone(),
            operation_id: operation.id.clone(),
            arguments_sha256: admission.arguments_digest(),
            resources_sha256: admission.resources_digest(),
            effect: EffectKind::FileWrite,
        },
        facts: PreparedEffectFacts {
            kind: EffectKind::FileWrite,
            resource_authorized: true,
            isolation: IsolationFact::Enforced,
            builtin_handler: true,
            sensitive: false,
            explicit_deny: false,
            mandatory_human: true,
        },
        presentation: json!({"kind":"workspace_merge","planDigest":operation.plan.digest()}),
    };
    let authority = backend.organizations(actor.tenant_id().clone());
    let approval = authority
        .admit(&parent.lease, proposal.clone())
        .await
        .unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    assert!(
        authority
            .check_workspace_merge_execution(proposal.clone(), &admission)
            .await
            .is_err()
    );
    authority
        .answer(
            actor,
            AnswerApproval {
                request_id: RequestId::new("approve-merge").unwrap(),
                approval_id: approval.id,
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    raw_sql("CREATE FUNCTION public.refuse_merge_attempt() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected merge admission failure'; END $$;
        CREATE TRIGGER refuse_merge_attempt BEFORE INSERT ON zuno_enterprise_preview.gateway_merge_attempt
          FOR EACH ROW EXECUTE FUNCTION public.refuse_merge_attempt();").execute(admin).await.unwrap();
    assert!(
        authority
            .check_workspace_merge_execution(proposal.clone(), &admission)
            .await
            .is_err()
    );
    assert!(!query_scalar::<_,bool>("SELECT admitted FROM zuno_enterprise_preview.gateway_merge_operation WHERE tenant_id=$1 AND operation_id=$2")
        .bind(actor.tenant_id().as_str()).bind(operation.id.as_str()).fetch_one(admin).await.unwrap());
    raw_sql("DROP TRIGGER refuse_merge_attempt ON zuno_enterprise_preview.gateway_merge_attempt; DROP FUNCTION public.refuse_merge_attempt();")
        .execute(admin).await.unwrap();
    authority
        .check_workspace_merge_execution(proposal.clone(), &admission)
        .await
        .unwrap();
    let mut changed = admission.clone();
    changed.operation.plan.changes[0].choice = MergeChoice::Parent;
    assert!(
        authority
            .check_workspace_merge_execution(proposal, &changed)
            .await
            .is_err()
    );
    let reference = WaitRef {
        id: WaitId::new("merge-wait").unwrap(),
        turn_id: parent.job.turn_id.clone(),
        invocation_id: operation.invocation_id.clone(),
        arguments_sha256: "e".repeat(64),
        target: WaitTarget::Operation {
            operation_id: operation.id.clone(),
        },
        continuation: WaitContinuation::CurrentTurn,
    };
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    assert!(
        !crate::runtime::waiting::register(&mut tx, &parent.job, std::slice::from_ref(&reference))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    let completion = WorkspaceMergeCompletion {
        lease: parent.lease.clone(),
        operation: operation.clone(),
        receipt: WorkspaceMergeReceipt {
            id: operation.id.clone(),
            environment_id: operation.environment_id.clone(),
            state: WorkspaceMergeState::Committed,
            plan_digest: operation.plan.digest(),
            revision: 2,
        },
    };
    merge.complete(&completion).await.unwrap();
    merge.complete(&completion).await.unwrap();
    assert_eq!(
        merge
            .completion(&actor.owner(), &operation.id)
            .await
            .unwrap(),
        Some(completion.clone())
    );
    assert!(
        backend
            .workspace_merges(GatewayId::new("wrong").unwrap())
            .complete(&completion)
            .await
            .is_err()
    );
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let ready = crate::runtime::waiting::ready_completions(
        &mut tx,
        &parent.job,
        std::slice::from_ref(&reference),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ready.len(), 1);
    assert!(serde_json::to_string(&ready).unwrap().contains("committed"));
    tx.commit().await.unwrap();
}
