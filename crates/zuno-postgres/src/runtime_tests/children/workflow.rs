use super::*;
use zuno_application::workflow::{WorkflowDefinitionGrant, WorkflowInvocation, WorkflowStore};
use zuno_engine::wait::WaitCompletion;
use zuno_orchestration::{WorkflowNodeDescriptor, WorkflowTemplateDescriptor};
use zuno_types::{
    identity::{CompletionId, WaitId},
    wait::{WaitContinuation, WaitRef, WaitTarget},
};

fn definition(parent: &RuntimeJob) -> WorkflowDefinitionGrant {
    let nodes = [
        ("fast", vec![]),
        ("slow", vec![]),
        ("after-fast", vec!["fast".to_owned()]),
    ]
    .into_iter()
    .map(|(id, depends_on)| WorkflowNodeDescriptor {
        id: id.to_owned(),
        depends_on,
        agent: "explorer".to_owned(),
        prompt: Some(format!("Node {id}")),
        description: None,
    })
    .collect::<Vec<_>>();
    WorkflowDefinitionGrant {
        council: None,
        group: grant(parent),
        nodes: nodes
            .iter()
            .map(|node| (node.id.clone(), grant(parent)))
            .collect(),
        template: WorkflowTemplateDescriptor {
            name: "inspection".to_owned(),
            source_id: "fixture:inspection".to_owned(),
            max_parallel: 2,
            max_agents: 3,
            nodes,
        },
    }
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    workspace_authority(backend, admin).await;
    failure(backend, admin, false).await;
    failure(backend, admin, true).await;
    cancellation(backend, admin, false).await;
    cancellation(backend, admin, true).await;
    let (actor, parent) = parent(backend, admin, "workflow-durable").await;
    let store = backend.runtime(actor.tenant_id().clone());
    let grant = definition(&parent.job);
    let request = WorkflowInvocation {
        template: "inspection".to_owned(),
        root: invocation(ChildDelivery::Foreground),
    };
    let staged = store
        .dispatch_workflow(&parent.lease, request.clone(), &grant)
        .await
        .unwrap();
    assert_eq!(
        store
            .dispatch_workflow(&parent.lease, request.clone(), &grant)
            .await
            .unwrap(),
        staged
    );
    let mut changed = request;
    changed.root.prompt.push_str(" changed");
    assert!(matches!(
        store
            .dispatch_workflow(&parent.lease, changed, &grant)
            .await,
        Err(ApplicationError::Conflict)
    ));
    let prepared = store
        .prepare_workflow(&parent.lease, &staged.group.job_id)
        .await
        .unwrap();
    assert!(prepared.prepared);
    assert_eq!(prepared.nodes.len(), 3);
    assert!(
        store
            .claim(&worker("before-wait"), duration())
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store
            .submit(
                &actor,
                submission(&prepared.group.session_id, "forged-group-input", 1)
            )
            .await,
        Err(ApplicationError::Forbidden)
    ));

    raw_sql("CREATE FUNCTION public.refuse_workflow_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.type='runtime.checkpoint.committed' AND NEW.tenant_id='workflow-durable'
        THEN RAISE EXCEPTION 'injected workflow checkpoint failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_workflow_checkpoint BEFORE INSERT ON zuno_enterprise_preview.event FOR EACH ROW
          EXECUTE FUNCTION public.refuse_workflow_checkpoint();").execute(admin).await.unwrap();
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    assert!(
        crate::runtime::suspend_in(
            &mut tx,
            &parent.lease,
            checkpoint(&parent.job),
            std::slice::from_ref(&prepared.group.wait)
        )
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    raw_sql("DROP TRIGGER refuse_workflow_checkpoint ON zuno_enterprise_preview.event; DROP FUNCTION public.refuse_workflow_checkpoint();").execute(admin).await.unwrap();
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND parent_job_id=$2 AND state='active'")
        .bind(actor.tenant_id().as_str()).bind(prepared.group.job_id.as_str()).fetch_one(admin).await.unwrap(), 0);

    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let waiting = crate::runtime::suspend_in(
        &mut tx,
        &parent.lease,
        checkpoint(&parent.job),
        std::slice::from_ref(&prepared.group.wait),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(waiting.phase, JobPhase::Waiting);
    let first_worker = worker("one");
    let second_worker = worker("two");
    let (first, second) = tokio::join!(
        store.claim(&first_worker, duration()),
        store.claim(&second_worker, duration())
    );
    let first = first.unwrap().unwrap();
    let second = second.unwrap().unwrap();
    assert_ne!(first.job.id, second.job.id);
    let fast_id = &prepared.nodes[0].child.job_id;
    let (fast, slow) = if first.job.id == *fast_id {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(fast.job.id, *fast_id);
    assert_eq!(slow.job.id, prepared.nodes[1].child.job_id);
    consume(admin, &slow.job).await;
    let slow_wait = WaitRef {
        id: WaitId::new("workflow-slow-input").unwrap(),
        turn_id: slow.job.turn_id.clone(),
        invocation_id: InvocationId::new("request-input").unwrap(),
        arguments_sha256: "d".repeat(64),
        target: WaitTarget::UserInput {
            request_id: RequestId::new("answer").unwrap(),
        },
        continuation: WaitContinuation::CurrentTurn,
    };
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    crate::runtime::suspend_in(
        &mut tx,
        &slow.lease,
        checkpoint(&slow.job),
        std::slice::from_ref(&slow_wait),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        store
            .claim(&worker("free-slot"), duration())
            .await
            .unwrap()
            .is_none()
    );
    complete_child(backend, admin, &fast).await;
    let after = store
        .claim(&worker("refill"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.job.id, prepared.nodes[2].child.job_id);
    let text: String = query_scalar("SELECT prompt->'prompt'->>'text' FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND id=$2")
        .bind(actor.tenant_id().as_str()).bind(after.job.input_id.as_str()).fetch_one(admin).await.unwrap();
    assert!(text.contains("Authoritative child answer"));
    assert!(text.contains("Workflow dependency results"));
    assert!(!text.contains("private reasoning"));
    complete_child(backend, admin, &after).await;
    assert!(
        store
            .claim(&worker("still-waiting"), duration())
            .await
            .unwrap()
            .is_none()
    );

    let answer = WaitCompletion::tool_result(
        CompletionId::new("human-answer").unwrap(),
        slow_wait.clone(),
        zuno_engine::r#loop::ToolDispatchResult::success(zuno_tool::ToolOutput::text(
            "Answer",
            "Continue the requested inspection",
        )),
    );
    store
        .publish_completion(&actor.owner(), &slow.job.id, &answer)
        .await
        .unwrap();
    let resumed_slow = store
        .claim(&worker("replacement"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed_slow.job.id, slow.job.id);
    assert_ne!(resumed_slow.lease.attempt_id, slow.lease.attempt_id);
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    crate::runtime::waiting::consume(&mut tx, &resumed_slow.job, &[answer])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    complete_child(backend, admin, &resumed_slow).await;
    let mut resumed_parent = None;
    for _ in 0..3 {
        if let Some(claimed) = store
            .claim(&worker("parent-replacement"), duration())
            .await
            .unwrap()
        {
            resumed_parent = Some(claimed);
            break;
        }
    }
    let resumed_parent = resumed_parent.expect("workflow completion wakes the original parent");
    assert_eq!(resumed_parent.job.id, parent.job.id);
    assert_ne!(resumed_parent.lease.worker, parent.lease.worker);
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let completion =
        crate::runtime::children::ready(&mut tx, &resumed_parent.job, &prepared.group.wait)
            .await
            .unwrap()
            .unwrap();
    let raw = serde_json::to_string(&completion).unwrap();
    assert!(raw.contains("after-fast"));
    assert!(!raw.contains("private reasoning"));
    crate::runtime::waiting::consume(
        &mut tx,
        &resumed_parent.job,
        std::slice::from_ref(&completion),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    store
        .publish_completion(&actor.owner(), &parent.job.id, &completion)
        .await
        .unwrap();
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.runtime_attempt WHERE tenant_id=$1 AND job_id=$2")
        .bind(actor.tenant_id().as_str()).bind(prepared.group.job_id.as_str()).fetch_one(admin).await.unwrap(), 0,
        "logical coordination never consumes an Agent Worker attempt");
    assert_eq!(query_scalar::<_,String>("SELECT state FROM zuno_enterprise_preview.runtime_workflow WHERE tenant_id=$1 AND job_id=$2")
        .bind(actor.tenant_id().as_str()).bind(prepared.group.job_id.as_str()).fetch_one(admin).await.unwrap(), "completed");
}

async fn workspace_authority(backend: &PostgresBackend, admin: &PgPool) {
    use zuno_application::child::{
        ChildWorkspaceAssignment, ChildWorkspaceCompletion, ChildWorkspacePolicy,
        ChildWorkspaceReceipt,
    };
    use zuno_application::environment::{Environment, EnvironmentSnapshot, EnvironmentSpec};
    use zuno_types::identity::{EnvironmentId, EnvironmentSnapshotId, GatewayId};
    let (actor, root) = parent(backend, admin, "workflow-workspace-authority").await;
    let store = backend.runtime(actor.tenant_id().clone());
    let mut grant = definition(&root.job);
    grant.group.workspace = ChildWorkspacePolicy::ForkParent;
    for node in grant.nodes.values_mut() {
        node.workspace = ChildWorkspacePolicy::ForkParent;
    }
    let spec = |session: &SessionId| EnvironmentSpec {
        id: EnvironmentId::new(session.as_str()).unwrap(),
        session_id: session.clone(),
        image: format!("image@sha256:{}", "a".repeat(64)),
        memory_bytes: 67108864,
        pids_limit: 32,
        cpu_millis: 500,
    };
    let staged = store
        .dispatch_workflow(
            &root.lease,
            WorkflowInvocation {
                template: "inspection".to_owned(),
                root: invocation(ChildDelivery::Foreground),
            },
            &grant,
        )
        .await
        .unwrap();
    let group_assignment = ChildWorkspaceAssignment {
        child_job_id: staged.group.job_id.clone(),
        gateway_id: GatewayId::new("gateway").unwrap(),
        parent_gateway_id: None,
        parent: spec(&root.job.session_id),
        target: spec(&staged.group.session_id),
        resume: false,
    };
    store
        .admit_child_workspace(&root.lease, &group_assignment)
        .await
        .unwrap();
    store
        .complete_child_workspace(
            &group_assignment.gateway_id,
            &ChildWorkspaceCompletion {
                lease: root.lease.clone(),
                receipt: ChildWorkspaceReceipt {
                    child_job_id: staged.group.job_id.clone(),
                    parent_environment_id: group_assignment.parent.id.clone(),
                    snapshot: Some(EnvironmentSnapshot {
                        id: EnvironmentSnapshotId::new(format!("child-{}", staged.group.job_id))
                            .unwrap(),
                        environment_id: group_assignment.parent.id.clone(),
                        revision: 1,
                        sha256: "b".repeat(64),
                        bytes: 1024,
                    }),
                    target: Environment {
                        owner: actor.owner(),
                        spec: group_assignment.target.clone(),
                        revision: 1,
                    },
                },
            },
        )
        .await
        .unwrap();
    let view = store
        .prepare_workflow(&root.lease, &staged.group.job_id)
        .await
        .unwrap();
    assert!(!view.prepared);
    let node = &view.nodes[0].child;
    let info = store
        .child_workspace(&root.lease, &node.job_id)
        .await
        .unwrap();
    assert_eq!(info.parent_session_id, view.group.session_id);
    assert_ne!(info.parent_session_id, root.job.session_id);
    let assignment = ChildWorkspaceAssignment {
        child_job_id: node.job_id.clone(),
        gateway_id: GatewayId::new("gateway").unwrap(),
        parent_gateway_id: None,
        parent: spec(&view.group.session_id),
        target: spec(&node.session_id),
        resume: false,
    };
    let mut wrong = assignment.clone();
    wrong.parent = spec(&root.job.session_id);
    assert!(matches!(
        store.admit_child_workspace(&root.lease, &wrong).await,
        Err(ApplicationError::Forbidden)
    ));
    store
        .admit_child_workspace(&root.lease, &assignment)
        .await
        .unwrap();
    let other_session = session(backend, &actor, "other-root").await;
    store
        .submit(&actor, submission(&other_session, "other-turn", 0))
        .await
        .unwrap();
    let other = store
        .claim(&worker("other"), duration())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        store.child_workspace(&other.lease, &node.job_id).await,
        Err(ApplicationError::Forbidden)
    ));
    query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(root.job.session_id.as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        store.child_workspace(&root.lease, &node.job_id).await,
        Err(ApplicationError::LeaseLost)
    ));
}

async fn active(
    backend: &PostgresBackend,
    admin: &PgPool,
    name: &str,
    activate: bool,
) -> (
    PrincipalScope,
    zuno_application::runtime::ClaimedJob,
    zuno_application::workflow::WorkflowDispatch,
) {
    let (actor, root) = parent(backend, admin, name).await;
    let store = backend.runtime(actor.tenant_id().clone());
    let staged = store
        .dispatch_workflow(
            &root.lease,
            WorkflowInvocation {
                template: "inspection".to_owned(),
                root: invocation(ChildDelivery::Foreground),
            },
            &definition(&root.job),
        )
        .await
        .unwrap();
    let prepared = store
        .prepare_workflow(&root.lease, &staged.group.job_id)
        .await
        .unwrap();
    if activate {
        let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
            .await
            .unwrap();
        crate::runtime::suspend_in(
            &mut tx,
            &root.lease,
            checkpoint(&root.job),
            std::slice::from_ref(&prepared.group.wait),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    (actor, root, prepared)
}

async fn failure(backend: &PostgresBackend, admin: &PgPool, uncertain: bool) {
    let (actor, root, prepared) = active(
        backend,
        admin,
        &format!("workflow-failure-{uncertain}"),
        true,
    )
    .await;
    let store = backend.runtime(actor.tenant_id().clone());
    let first = store
        .claim(&worker("failed-node"), duration())
        .await
        .unwrap()
        .unwrap();
    let second = store
        .claim(&worker("sibling-node"), duration())
        .await
        .unwrap()
        .unwrap();
    consume(admin, &first.job).await;
    let outcome = if uncertain {
        JobFinish::Uncertain {
            reason: "external state needs inspection".to_owned(),
        }
    } else {
        JobFinish::Failed {
            code: "node_failed".to_owned(),
        }
    };
    store.finish(&first.lease, outcome).await.unwrap();
    let mut resumed = None;
    for _ in 0..3 {
        if let Some(job) = store
            .claim(&worker("parent-recovery"), duration())
            .await
            .unwrap()
        {
            resumed = Some(job);
            break;
        }
    }
    let resumed = resumed.expect("terminal workflow result must be delivered");
    assert_eq!(resumed.job.id, root.job.id);
    assert_eq!(
        store
            .get(&actor.owner(), &second.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Cancelled
    );
    assert!(matches!(
        store.renew(&second.lease, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    assert!(matches!(
        store
            .get(&actor.owner(), &prepared.nodes[2].child.job_id)
            .await,
        Err(ApplicationError::NotFound)
    ));
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let completion = crate::runtime::children::ready(&mut tx, &resumed.job, &prepared.group.wait)
        .await
        .unwrap()
        .unwrap();
    completion.validate().unwrap();
    let zuno_engine::wait::WaitOutcome::ToolResult { result } = completion.outcome else {
        panic!("workflow result")
    };
    assert!(result.is_error);
    assert_eq!(result.uncertain.is_some(), uncertain);
    tx.commit().await.unwrap();
    let outsider = principal(actor.tenant_id().as_str(), "other");
    crate::tests::install_access(admin, &outsider).await;
    assert!(matches!(
        backend
            .client_workflow(&outsider, &prepared.group.job_id)
            .await,
        Err(ApplicationError::NotFound)
    ));
}

async fn cancellation(backend: &PostgresBackend, admin: &PgPool, activated: bool) {
    use zuno_application::control::{CancelJob, RuntimeControl};
    let (actor, root, prepared) = active(
        backend,
        admin,
        &format!("workflow-cancel-{activated}"),
        activated,
    )
    .await;
    let store = backend.runtime(actor.tenant_id().clone());
    let request = CancelJob {
        request_id: RequestId::new("stop-workflow").unwrap(),
        expected_turn_id: root.job.turn_id.clone(),
        reason: "task was cancelled".to_owned(),
    };
    let receipt = store
        .cancel(&actor, &root.job.id, request.clone())
        .await
        .unwrap();
    assert_eq!(
        store.cancel(&actor, &root.job.id, request).await.unwrap(),
        receipt
    );
    assert!(
        store
            .claim(&worker("after-cancel"), duration())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get(&actor.owner(), &prepared.group.job_id)
            .await
            .unwrap()
            .phase,
        JobPhase::Cancelled
    );
    assert_eq!(
        backend
            .client_workflow(&actor, &prepared.group.job_id)
            .await
            .unwrap()
            .state,
        zuno_application::workflow::WorkflowState::Cancelled
    );
}
