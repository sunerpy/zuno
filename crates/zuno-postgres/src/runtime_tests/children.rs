use super::*;
mod workspace;
use zuno_application::child::{
    ChildDefinitionGrant, ChildDelivery, ChildDispatchStore, ChildInvocation,
};
use zuno_types::identity::InvocationId;

pub(super) async fn execution_binding(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("runtime-child-binding", "owner");
    crate::tests::install_access(admin, &actor).await;
    let parent = session(backend, &actor, "parent").await;
    let child = session(backend, &actor, "child").await;
    query("UPDATE zuno_enterprise_preview.session SET parent_id=$4 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(child.as_str()).bind(parent.as_str())
        .execute(admin).await.unwrap();
    let runtime = backend.runtime(actor.tenant_id().clone());
    let job = runtime
        .submit(&actor, submission(&child, "child-turn", 0))
        .await
        .unwrap();
    let mut tx = scoped_transaction(&backend.pool, &actor).await.unwrap();
    let rebound = query("UPDATE zuno_enterprise_preview.agent_job SET parent_session_id=$4,subject_kind='child-session',
        subject_payload=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job.id.as_str()).bind(parent.as_str())
        .bind(json!({"kind":"childSession","sessionID":child})).execute(&mut *tx).await;
    assert!(
        rebound.is_ok(),
        "a logical child Job must keep its parent while executing in a separate child session: {rebound:?}"
    );
    tx.commit().await.unwrap();
    let claimed = runtime
        .claim(&worker("child-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job.id, job.id);
    assert_eq!(claimed.lease.session_id, child);
    assert_ne!(claimed.lease.session_id, parent);
    let parent_slot: Option<String> = query_scalar(
        "SELECT current_job_id FROM zuno_enterprise_preview.runtime_session
        WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
    )
    .bind(actor.tenant_id().as_str())
    .bind(actor.principal_id().as_str())
    .bind(parent.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert!(
        parent_slot.is_none(),
        "child execution cannot seize its parent's slot"
    );
    foreground_admission(backend, admin).await;
    cancelled_parent_retires_unadmitted_children(backend, admin).await;
    resumed_child_has_one_pending_admission(backend, admin).await;
    for (delivery, cancelled) in [
        (ChildDelivery::NextStep, false),
        (ChildDelivery::NextStep, true),
        (ChildDelivery::Quiet, false),
    ] {
        background_delivery(backend, admin, delivery, cancelled).await;
    }
    workspace::exercise(backend, admin).await;
    parent_depth_limit_is_not_raised_by_child_definition(backend, admin).await;
    cancellation_fences_the_tree_and_preserves_other_sessions(backend, admin).await;
    completed_parent_cancellation_prevents_a_late_next_step(backend, admin, false).await;
    completed_parent_cancellation_prevents_a_late_next_step(backend, admin, true).await;
}

async fn cancellation_fences_the_tree_and_preserves_other_sessions(
    backend: &PostgresBackend,
    admin: &PgPool,
) {
    use zuno_application::control::{CancelJob, RuntimeControl};
    let (actor, root) = parent(backend, admin, "cancel-tree").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    runtime
        .dispatch_child(
            &root.lease,
            invocation(ChildDelivery::NextStep),
            &grant(&root.job),
        )
        .await
        .unwrap();
    let child = runtime
        .claim(&worker("child"), duration())
        .await
        .unwrap()
        .unwrap();
    let staged = runtime
        .dispatch_child(
            &root.lease,
            ChildInvocation {
                invocation_id: InvocationId::new("staged").unwrap(),
                logical_key: "staged".to_owned(),
                ..invocation(ChildDelivery::Foreground)
            },
            &grant(&root.job),
        )
        .await
        .unwrap();
    runtime
        .dispatch_child(
            &child.lease,
            invocation(ChildDelivery::Quiet),
            &grant(&child.job),
        )
        .await
        .unwrap();
    let grandchild = runtime
        .claim(&worker("grandchild"), duration())
        .await
        .unwrap()
        .unwrap();
    let independent = session(backend, &actor, "independent").await;
    let other = runtime
        .submit(&actor, submission(&independent, "independent-turn", 0))
        .await
        .unwrap();
    let request = CancelJob {
        request_id: RequestId::new("cancel-tree").unwrap(),
        expected_turn_id: root.job.turn_id.clone(),
        reason: "Task no longer needed".to_owned(),
    };
    raw_sql(
        "CREATE FUNCTION public.refuse_stop_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.type='runtime.cancellation.requested' AND NEW.tenant_id='cancel-tree'
        THEN RAISE EXCEPTION 'injected stop receipt failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_stop_receipt BEFORE INSERT ON zuno_enterprise_preview.event
        FOR EACH ROW EXECUTE FUNCTION public.refuse_stop_receipt();",
    )
    .execute(admin)
    .await
    .unwrap();
    assert!(
        runtime
            .cancel(&actor, &root.job.id, request.clone())
            .await
            .is_err()
    );
    raw_sql("DROP TRIGGER refuse_stop_receipt ON zuno_enterprise_preview.event; DROP FUNCTION public.refuse_stop_receipt();")
        .execute(admin).await.unwrap();
    for job in [&root, &child, &grandchild] {
        runtime
            .renew(&job.lease, duration())
            .await
            .expect("failed cancellation rolls back every lease fence");
    }
    let (first, repeated) = tokio::join!(
        runtime.cancel(&actor, &root.job.id, request.clone()),
        runtime.cancel(&actor, &root.job.id, request.clone())
    );
    let receipt = first.unwrap();
    assert_eq!(
        repeated.unwrap(),
        receipt,
        "competing cancellations consume one request"
    );
    assert_eq!(receipt.stopped_jobs.len(), 3);
    for job in [&root, &child, &grandchild] {
        assert_eq!(
            runtime
                .get(&actor.owner(), &job.job.id)
                .await
                .unwrap()
                .phase,
            JobPhase::Cancelled
        );
        assert!(matches!(
            runtime.renew(&job.lease, duration()).await,
            Err(ApplicationError::LeaseLost)
        ));
        assert!(matches!(
            runtime
                .finish(&job.lease, JobFinish::Completed { result: json!({}) })
                .await,
            Err(ApplicationError::LeaseLost)
        ));
    }
    assert_eq!(
        runtime.cancel(&actor, &root.job.id, request).await.unwrap(),
        receipt
    );
    assert_eq!(query_scalar::<_,String>("SELECT state FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND job_id=$2")
        .bind(actor.tenant_id().as_str()).bind(staged.job_id.as_str()).fetch_one(admin).await.unwrap(),"cancelled");
    let next = runtime
        .claim(&worker("available"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        next.job.id, other.id,
        "cancelled descendants cannot block an independent session"
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1"
        )
        .bind(actor.tenant_id().as_str())
        .fetch_one(admin)
        .await
        .unwrap(),
        4,
        "cancelled nextStep children cannot silently start a new parent turn"
    );
}

async fn completed_parent_cancellation_prevents_a_late_next_step(
    backend: &PostgresBackend,
    admin: &PgPool,
    published: bool,
) {
    use zuno_application::control::{CancelJob, RuntimeControl};
    let tenant = format!("cancel-completed-parent-{published}");
    let (actor, root) = parent(backend, admin, &tenant).await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    runtime
        .dispatch_child(
            &root.lease,
            invocation(ChildDelivery::NextStep),
            &grant(&root.job),
        )
        .await
        .unwrap();
    let child = runtime
        .claim(&worker("completed-child"), duration())
        .await
        .unwrap()
        .unwrap();
    complete_child(backend, admin, &child).await;
    consume(admin, &root.job).await;
    runtime
        .finish(
            &root.lease,
            JobFinish::Completed {
                result: json!({"done":true}),
            },
        )
        .await
        .unwrap();
    let continuation = if published {
        Some(
            runtime
                .claim(&worker("early-notification"), duration())
                .await
                .unwrap()
                .unwrap(),
        )
    } else {
        None
    };
    let receipt = runtime
        .cancel(
            &actor,
            &root.job.id,
            CancelJob {
                request_id: RequestId::new("close-tree").unwrap(),
                expected_turn_id: root.job.turn_id.clone(),
                reason: "Do not continue this task".to_owned(),
            },
        )
        .await
        .unwrap();
    if let Some(continuation) = continuation {
        assert!(receipt.stopped_jobs.contains(&continuation.job.id));
        assert!(matches!(
            runtime.renew(&continuation.lease, duration()).await,
            Err(ApplicationError::LeaseLost)
        ));
    }
    assert!(
        runtime
            .claim(&worker("next-step"), duration())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime
            .get(&actor.owner(), &child.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Completed
    );
    assert_eq!(
        runtime
            .get(&actor.owner(), &root.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Completed
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1"
        )
        .bind(actor.tenant_id().as_str())
        .fetch_one(admin)
        .await
        .unwrap(),
        2 + i64::from(published)
    );
}

async fn parent_depth_limit_is_not_raised_by_child_definition(
    backend: &PostgresBackend,
    admin: &PgPool,
) {
    let (actor, parent) = parent(backend, admin, "child-inherited-depth").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let mut outer = grant(&parent.job);
    outer.maximum_depth = 1;
    runtime
        .dispatch_child(&parent.lease, invocation(ChildDelivery::Quiet), &outer)
        .await
        .unwrap();
    let child = runtime
        .claim(&worker("one-hop"), duration())
        .await
        .unwrap()
        .unwrap();
    let mut wider = grant(&child.job);
    wider.maximum_depth = 16;
    assert!(
        matches!(
            runtime
                .dispatch_child(&child.lease, invocation(ChildDelivery::Quiet), &wider)
                .await,
            Err(ApplicationError::Forbidden)
        ),
        "a child definition cannot expand the delegation depth authorized by its parent"
    );
}

async fn resumed_child_has_one_pending_admission(backend: &PostgresBackend, admin: &PgPool) {
    let (actor, parent) = parent(backend, admin, "child-resume-reservation").await;
    let store = backend.runtime(actor.tenant_id().clone());
    let definition = grant(&parent.job);
    let ticket = store
        .dispatch_child(&parent.lease, invocation(ChildDelivery::Quiet), &definition)
        .await
        .unwrap();
    let child = store
        .claim(&worker("initial-child"), duration())
        .await
        .unwrap()
        .unwrap();
    complete_child(backend, admin, &child).await;
    let mut first = invocation(ChildDelivery::Foreground);
    first.invocation_id = InvocationId::new("resume-first").unwrap();
    first.logical_key = "first followup".to_owned();
    first.resume_session_id = Some(ticket.session_id);
    store
        .dispatch_child(&parent.lease, first.clone(), &definition)
        .await
        .unwrap();
    let mut duplicate = first;
    duplicate.invocation_id = InvocationId::new("resume-second").unwrap();
    duplicate.logical_key = "different followup".to_owned();
    assert!(
        matches!(
            store
                .dispatch_child(&parent.lease, duplicate, &definition)
                .await,
            Err(ApplicationError::Conflict)
        ),
        "two staged turns must not reserve the same child session before either reaches a checkpoint"
    );
}

async fn cancelled_parent_retires_unadmitted_children(backend: &PostgresBackend, admin: &PgPool) {
    let (actor, first) = parent(backend, admin, "child-abandoned-stage").await;
    let store = backend.runtime(actor.tenant_id().clone());
    store
        .dispatch_child(
            &first.lease,
            invocation(ChildDelivery::Foreground),
            &grant(&first.job),
        )
        .await
        .unwrap();
    store
        .finish(
            &first.lease,
            JobFinish::Cancelled {
                reason: "interrupted before waiting checkpoint".to_owned(),
            },
        )
        .await
        .unwrap();
    let version = store
        .input_version(&actor.owner(), &first.job.session_id)
        .await
        .unwrap();
    let next = store
        .submit(
            &actor,
            submission(&first.job.session_id, "explicit-next-request", version),
        )
        .await
        .unwrap();
    let claim = store
        .claim(&worker("new-parent"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.job.id, next.id);
    let result = store
        .dispatch_child(
            &claim.lease,
            invocation(ChildDelivery::Foreground),
            &grant(&next),
        )
        .await;
    assert!(
        result.is_ok(),
        "an unadmitted child from a cancelled parent cannot block the user's next explicit delegation: {result:?}"
    );
}

fn invocation(delivery: ChildDelivery) -> ChildInvocation {
    ChildInvocation {
        invocation_id: InvocationId::new("delegate-call").unwrap(),
        arguments_sha256: "c".repeat(64),
        logical_key: "delegation:test".to_owned(),
        prompt: "Inspect the project".to_owned(),
        description: "Project inspection".to_owned(),
        delivery,
        resume_session_id: None,
        presentation: json!({"objective":"Inspect the project","deliverable":"Findings"}),
    }
}
fn grant(parent: &RuntimeJob) -> ChildDefinitionGrant {
    ChildDefinitionGrant {
        parent: parent.configuration.clone(),
        child: parent.configuration.clone(),
        selection: zuno_application::runtime::JobInputSelection {
            agent: "explorer".to_owned(),
            model: zuno_application::runtime::JobInputModel {
                provider_id: "fixture".to_owned(),
                model_id: "model".to_owned(),
            },
        },
        maximum_depth: 2,
        maximum_children: 4,
        workspace: zuno_application::child::ChildWorkspacePolicy::ModelOnly,
    }
}
async fn parent(
    backend: &PostgresBackend,
    admin: &PgPool,
    tenant: &str,
) -> (PrincipalScope, zuno_application::runtime::ClaimedJob) {
    let actor = principal(tenant, "owner");
    crate::tests::install_access(admin, &actor).await;
    let parent = session(backend, &actor, "parent").await;
    let store = backend.runtime(actor.tenant_id().clone());
    let job = store
        .submit(&actor, submission(&parent, "parent-input", 0))
        .await
        .unwrap();
    let claimed = store
        .claim(&worker("parent-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, claimed.job.id);
    consume(admin, &job).await;
    (actor, claimed)
}

async fn complete_child(
    backend: &PostgresBackend,
    admin: &PgPool,
    child: &zuno_application::runtime::ClaimedJob,
) {
    let owner = &child.lease.owner;
    let session = &child.job.session_id;
    consume(admin, &child.job).await;
    let id = format!("answer_{}", child.job.id);
    query("INSERT INTO zuno_enterprise_preview.message(tenant_id,principal_id,session_id,id,role,data,time_created,time_updated)
        VALUES($1,$2,$3,$4,'assistant',$5,100,101)")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str()).bind(&id)
        .bind(json!({"id":id,"sessionID":session,"role":"assistant","time":{"created":100,"completed":101}}))
        .execute(admin).await.unwrap();
    for (suffix, kind, text) in [
        ("text", "text", "Authoritative child answer"),
        (
            "thought",
            "reasoning",
            "private reasoning must not be a child result",
        ),
    ] {
        let part = format!("{id}_{suffix}");
        query("INSERT INTO zuno_enterprise_preview.part(tenant_id,principal_id,session_id,message_id,id,kind,data,time_created,time_updated)
            VALUES($1,$2,$3,$4,$5,$6,$7,100,101)")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session.as_str()).bind(&id).bind(&part).bind(kind)
            .bind(json!({"id":part,"sessionID":session,"messageID":id,"type":kind,"text":text})).execute(admin).await.unwrap();
    }
    let outcome = zuno_engine::advance::AdvanceState::Completed {
        outcome: zuno_engine::r#loop::TurnOutcome::Completed {
            assistant_message_id: id,
            steps: 1,
            unresolved_tool_failures: Vec::new(),
        },
    };
    backend
        .runtime(owner.tenant_id.clone())
        .finish(
            &child.lease,
            JobFinish::Completed {
                result: json!(outcome),
            },
        )
        .await
        .unwrap();
}

async fn foreground_admission(backend: &PostgresBackend, admin: &PgPool) {
    use crate::runtime::{children, waiting};
    let (actor, parent) = parent(backend, admin, "child-foreground").await;
    let store = backend.runtime(actor.tenant_id().clone());
    let grant = grant(&parent.job);
    let intent = invocation(ChildDelivery::Foreground);
    let ticket = store
        .dispatch_child(&parent.lease, intent.clone(), &grant)
        .await
        .unwrap();
    assert_eq!(
        store
            .dispatch_child(&parent.lease, intent.clone(), &grant)
            .await
            .unwrap(),
        ticket
    );
    let mut changed = intent;
    changed.prompt.push_str(" different");
    assert!(matches!(
        store.dispatch_child(&parent.lease, changed, &grant).await,
        Err(ApplicationError::Conflict)
    ));
    assert!(matches!(
        store.get(&actor.owner(), &ticket.job_id).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(
        store
            .claim(&worker("early-child"), duration())
            .await
            .unwrap()
            .is_none(),
        "staged foreground work cannot run before the waiting checkpoint"
    );
    raw_sql("CREATE FUNCTION zuno_enterprise_preview.refuse_child_checkpoint() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.type='runtime.checkpoint.committed' THEN RAISE EXCEPTION 'injected checkpoint failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_child_checkpoint BEFORE INSERT ON zuno_enterprise_preview.event FOR EACH ROW
          EXECUTE FUNCTION zuno_enterprise_preview.refuse_child_checkpoint();").execute(admin).await.unwrap();
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
        .is_err()
    );
    tx.rollback().await.unwrap();
    assert!(matches!(
        store.get(&actor.owner(), &ticket.job_id).await,
        Err(ApplicationError::NotFound)
    ));
    raw_sql("DROP TRIGGER refuse_child_checkpoint ON zuno_enterprise_preview.event; DROP FUNCTION zuno_enterprise_preview.refuse_child_checkpoint();")
        .execute(admin).await.unwrap();
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let suspended = crate::runtime::suspend_in(
        &mut tx,
        &parent.lease,
        checkpoint(&parent.job),
        std::slice::from_ref(&ticket.wait),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(suspended.phase, JobPhase::Waiting);
    let child = store
        .claim(&worker("child-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.job.id, ticket.job_id);
    assert_eq!(child.job.session_id, ticket.session_id);
    assert_ne!(child.job.session_id, parent.job.session_id);
    complete_child(backend, admin, &child).await;
    assert_eq!(
        store
            .get(&actor.owner(), &parent.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Waiting,
        "durable result precedes parent delivery"
    );
    let resumed = store
        .claim(&worker("replacement-parent"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.job.id, parent.job.id);
    assert_ne!(resumed.lease.worker, parent.lease.worker);
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let ready = children::ready(&mut tx, &resumed.job, &ticket.wait)
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    store
        .publish_completion(&actor.owner(), &resumed.job.id, &ready)
        .await
        .unwrap();
    store
        .publish_completion(&actor.owner(), &resumed.job.id, &ready)
        .await
        .unwrap();
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let completions =
        waiting::ready_completions(&mut tx, &resumed.job, std::slice::from_ref(&ticket.wait))
            .await
            .unwrap()
            .unwrap();
    let serialized = serde_json::to_string(&completions).unwrap();
    assert!(serialized.contains("Authoritative child answer"));
    assert!(!serialized.contains("private reasoning"));
    waiting::consume(&mut tx, &resumed.job, &completions)
        .await
        .unwrap();
    crate::runtime::checkpoint_in(&mut tx, &resumed.lease, checkpoint(&resumed.job))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    assert!(
        waiting::consume(&mut tx, &resumed.job, &completions)
            .await
            .is_err(),
        "completion consumption is compare-and-set"
    );
    tx.rollback().await.unwrap();
    let count:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.runtime_wait WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='consumed'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(parent.job.id.as_str()).fetch_one(admin).await.unwrap();
    assert_eq!(count, 1);
}

async fn background_delivery(
    backend: &PostgresBackend,
    admin: &PgPool,
    delivery: ChildDelivery,
    cancelled: bool,
) {
    let (actor, parent) = parent(
        backend,
        admin,
        if cancelled {
            "child-cancelled-parent"
        } else if delivery == ChildDelivery::Quiet {
            "child-quiet"
        } else {
            "child-background"
        },
    )
    .await;
    let store = backend.runtime(actor.tenant_id().clone());
    let ticket = store
        .dispatch_child(&parent.lease, invocation(delivery), &grant(&parent.job))
        .await
        .unwrap();
    let child = store
        .claim(&worker("background-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.job.id, ticket.job_id);
    if cancelled {
        store
            .finish(
                &parent.lease,
                JobFinish::Cancelled {
                    reason: "user stopped".to_owned(),
                },
            )
            .await
            .unwrap();
    } else {
        store
            .finish(
                &parent.lease,
                JobFinish::Completed {
                    result: json!({"parent":"yielded"}),
                },
            )
            .await
            .unwrap();
    }
    complete_child(backend, admin, &child).await;
    let followup = store
        .claim(&worker("followup-worker"), duration())
        .await
        .unwrap();
    if cancelled || delivery == ChildDelivery::Quiet {
        assert!(
            followup.is_none(),
            "a quiet completion or cancelled parent cannot schedule another turn"
        );
        assert_eq!(
            store
                .get(&actor.owner(), &parent.job.id)
                .await
                .unwrap()
                .phase,
            if cancelled {
                JobPhase::Cancelled
            } else {
                JobPhase::Completed
            }
        );
    } else {
        let followup = followup.unwrap();
        assert_eq!(followup.job.session_id, parent.job.session_id);
        assert_ne!(followup.job.id, parent.job.id);
        let input:serde_json::Value=query_scalar("SELECT prompt FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(followup.job.input_id.as_str()).fetch_one(admin).await.unwrap();
        assert_eq!(input["kind"], "completion");
        assert_eq!(input["completion"]["source"], "agent_job");
        assert!(
            input["prompt"]["text"]
                .as_str()
                .unwrap()
                .contains("Authoritative child answer")
        );
    }
    let reports:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND type='agent.job.parent_report'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(parent.job.session_id.as_str()).fetch_one(admin).await.unwrap();
    assert_eq!(reports, 1);
}
