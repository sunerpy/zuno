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
    }
}
