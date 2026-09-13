use super::*;
use zuno_application::{
    council::{CouncilDefinitionGrant, CouncilInvocation, CouncilRules, CouncilStore},
    workflow::{WorkflowDispatch, WorkflowStore},
};
use zuno_orchestration::{
    CouncilPresetDescriptor, CouncilRetryPolicyDescriptor, CouncilSeatDescriptor,
    CouncilSynthesisPolicyDescriptor,
};
use zuno_types::{
    identity::{CompletionId, WaitId},
    wait::{WaitContinuation, WaitRef, WaitTarget},
};

fn definition(parent: &RuntimeJob, seats: usize, retries: usize) -> CouncilDefinitionGrant {
    let mut child = grant(parent);
    child.maximum_children = 16;
    let preset = CouncilPresetDescriptor {
        name: "inspect".to_owned(),
        source_id: "fixture:council".to_owned(),
        seats: (0..seats)
            .map(|index| CouncilSeatDescriptor {
                id: format!("seat-{index}"),
                agent: "explorer".to_owned(),
                instruction: format!("Inspect concern {index}"),
            })
            .collect(),
        quorum: 2,
        max_parallel: 2,
        deadline_ms: 60_000,
        retry_policy: CouncilRetryPolicyDescriptor {
            max_retries: retries,
        },
        synthesis_policy: CouncilSynthesisPolicyDescriptor {
            timeout_ms: 10_000,
            max_input_bytes: 65536,
        },
        seat_output_bytes: 8192,
    };
    CouncilDefinitionGrant {
        group: child.clone(),
        synthesis: child.clone(),
        seats: preset
            .seats
            .iter()
            .map(|seat| (seat.id.clone(), child.clone()))
            .collect(),
        rules: CouncilRules {
            repairs: preset
                .seats
                .iter()
                .map(|seat| (seat.id.clone(), child.clone()))
                .collect(),
            preset,
        },
    }
}
fn answer(verdict: &str) -> String {
    json!({"verdict":verdict,"confidence":0.8,"evidence":["observed source"],"risks":["unverified external state"],"recommendation":"Inspect the remaining state"}).to_string()
}
async fn start(
    backend: &PostgresBackend,
    admin: &PgPool,
    tenant: &str,
    seats: usize,
    retries: usize,
    short: bool,
) -> (
    PrincipalScope,
    zuno_application::runtime::ClaimedJob,
    WorkflowDispatch,
) {
    let (actor, parent) = parent(backend, admin, tenant).await;
    let store = backend.runtime(actor.tenant_id().clone());
    let mut grant = definition(&parent.job, seats, retries);
    if short {
        grant.rules.preset.quorum = 1;
        grant.rules.preset.deadline_ms = 4000;
        grant.rules.preset.synthesis_policy.timeout_ms = 2000;
    }
    let request = CouncilInvocation {
        preset: "inspect".to_owned(),
        root: invocation(ChildDelivery::Foreground),
    };
    let staged = store
        .dispatch_council(&parent.lease, request.clone(), &grant)
        .await
        .unwrap();
    assert_eq!(
        store
            .dispatch_council(&parent.lease, request.clone(), &grant)
            .await
            .unwrap(),
        staged
    );
    let mut changed = request;
    changed.root.prompt.push_str(" changed");
    assert!(matches!(
        store.dispatch_council(&parent.lease, changed, &grant).await,
        Err(ApplicationError::Conflict)
    ));
    let prepared = store
        .prepare_workflow(&parent.lease, &staged.group.job_id)
        .await
        .unwrap();
    assert!(prepared.prepared);
    assert_eq!(prepared.nodes.len(), seats + 1);
    assert!(
        store
            .claim(&worker("before-wait"), duration())
            .await
            .unwrap()
            .is_none()
    );
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    crate::runtime::suspend_in(
        &mut tx,
        &parent.lease,
        checkpoint(&parent.job),
        std::slice::from_ref(&prepared.group.wait),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (actor, parent, prepared)
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    Box::pin(repair_and_quorum(backend, admin)).await;
    Box::pin(deadline(backend, admin, true)).await;
    Box::pin(deadline(backend, admin, false)).await;
    Box::pin(cancelled(backend, admin)).await;
    Box::pin(operation_deadline(backend, admin, true)).await;
    Box::pin(operation_deadline(backend, admin, false)).await;
    Box::pin(synthesis_timeout(backend, admin)).await;
    Box::pin(repair_exhaustion(backend, admin)).await;
}

async fn synthesis_timeout(backend: &PostgresBackend, admin: &PgPool) {
    let (actor, parent, prepared) =
        start(backend, admin, "council-synthesis-timeout", 2, 0, true).await;
    let store = backend.runtime(actor.tenant_id().clone());
    for index in 0..2 {
        let seat = store
            .claim(&worker(&format!("seat-{index}")), duration())
            .await
            .unwrap()
            .unwrap();
        complete_child_with_text(backend, admin, &seat, &answer("validated")).await;
    }
    let synthesis = store
        .claim(&worker("synthesis"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(synthesis.job.id, prepared.nodes[2].child.job_id);
    let now: i64 = query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint")
        .fetch_one(admin)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(
        (synthesis.lease.expires_at_ms - now).max(0) as u64 + 20,
    ))
    .await;
    let mut resumed = None;
    for _ in 0..3 {
        if let Some(job) = store.claim(&worker("parent"), duration()).await.unwrap() {
            resumed = Some(job);
            break;
        }
    }
    assert_eq!(resumed.unwrap().job.id, parent.job.id);
    assert_eq!(
        store
            .get(&actor.owner(), &prepared.group.job_id)
            .await
            .unwrap()
            .phase,
        JobPhase::Failed
    );
    assert!(matches!(
        store.renew(&synthesis.lease, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    assert_eq!(
        store
            .get(&actor.owner(), &synthesis.job.id)
            .await
            .unwrap()
            .phase,
        JobPhase::Cancelled
    );
}

async fn repair_exhaustion(backend: &PostgresBackend, admin: &PgPool) {
    let (actor, parent, prepared) = start(backend, admin, "council-invalid", 2, 1, false).await;
    let store = backend.runtime(actor.tenant_id().clone());
    let valid = store
        .claim(&worker("valid"), duration())
        .await
        .unwrap()
        .unwrap();
    let invalid = store
        .claim(&worker("invalid"), duration())
        .await
        .unwrap()
        .unwrap();
    complete_child_with_text(backend, admin, &valid, &answer("validated")).await;
    complete_child_with_text(backend, admin, &invalid, "invalid response").await;
    let limit = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let repair = loop {
        if let Some(job) = store.claim(&worker("repair"), duration()).await.unwrap() {
            break job;
        }
        assert!(tokio::time::Instant::now() < limit);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    complete_child_with_text(backend, admin, &repair, "still invalid").await;
    let mut resumed = None;
    for _ in 0..3 {
        if let Some(job) = store.claim(&worker("parent"), duration()).await.unwrap() {
            resumed = Some(job);
            break;
        }
    }
    assert_eq!(resumed.unwrap().job.id, parent.job.id);
    assert_eq!(
        store
            .get(&actor.owner(), &prepared.group.job_id)
            .await
            .unwrap()
            .phase,
        JobPhase::Failed
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.runtime_council_attempt WHERE tenant_id=$1")
        .bind(actor.tenant_id().as_str()).fetch_one(admin).await.unwrap(),3);
    assert_eq!(query_scalar::<_,String>("SELECT state FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND job_id=$2")
        .bind(actor.tenant_id().as_str()).bind(prepared.nodes[2].child.job_id.as_str()).fetch_one(admin).await.unwrap(),"cancelled");
}

async fn operation_deadline(backend: &PostgresBackend, admin: &PgPool, receipt_before_grace: bool) {
    use zuno_application::environment::{
        CommandOperation, Environment, EnvironmentSpec, OperationAdmission, OperationCompletion,
        OperationPhase, OperationReceipt,
    };
    use zuno_types::identity::{EnvironmentId, GatewayId, OperationId};
    let (actor, parent, prepared) = start(
        backend,
        admin,
        &format!("council-operation-{receipt_before_grace}"),
        2,
        0,
        true,
    )
    .await;
    let store = backend.runtime(actor.tenant_id().clone());
    let voter = store
        .claim(&worker("voter"), duration())
        .await
        .unwrap()
        .unwrap();
    let executing = store
        .claim(&worker("executing"), duration())
        .await
        .unwrap()
        .unwrap();
    complete_child_with_text(backend, admin, &voter, &answer("validated")).await;
    let gateway = GatewayId::new("test-gateway").unwrap();
    let operation = CommandOperation {
        id: OperationId::new("pending-council-operation").unwrap(),
        invocation_id: InvocationId::new("command").unwrap(),
        environment_id: EnvironmentId::new("council-environment").unwrap(),
        expected_revision: 1,
        argv: vec!["inspect".to_owned()],
    };
    let admission = OperationAdmission {
        gateway_id: gateway.clone(),
        lease: executing.lease.clone(),
        operation: operation.clone(),
        environment: Environment {
            owner: actor.owner(),
            revision: 1,
            spec: EnvironmentSpec {
                id: operation.environment_id.clone(),
                session_id: executing.job.session_id.clone(),
                image: format!("fixture@sha256:{}", "a".repeat(64)),
                memory_bytes: 64 * 1024 * 1024,
                pids_limit: 32,
                cpu_millis: 1000,
            },
        },
    };
    // Seed the already-approved operation at the storage boundary. Native
    // gateway tests separately exercise its authoritative approval producer.
    let raw = json!(admission);
    query("INSERT INTO zuno_enterprise_preview.gateway_operation
        (tenant_id,principal_id,operation_id,gateway_id,job_id,session_id,invocation_id,admission,admission_digest,time_admitted)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,1000)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(operation.id.as_str()).bind(gateway.as_str())
        .bind(executing.job.id.as_str()).bind(executing.job.session_id.as_str()).bind(operation.invocation_id.as_str())
        .bind(&raw).bind(zuno_orchestration::sha256_json(&raw)).execute(admin).await.unwrap();
    query("INSERT INTO zuno_enterprise_preview.gateway_operation_attempt
        (tenant_id,principal_id,operation_id,attempt_id,worker_id,epoch,checkpoint_version,lease,time_admitted)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,1000)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(operation.id.as_str())
        .bind(executing.lease.attempt_id.as_str()).bind(executing.lease.worker.as_str())
        .bind(executing.lease.epoch as i64).bind(executing.lease.checkpoint_version as i64).bind(json!(executing.lease))
        .execute(admin).await.unwrap();
    let seat_deadline = executing.lease.expires_at_ms;
    async fn until(admin: &PgPool, deadline: i64) {
        let now: i64 =
            query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint")
                .fetch_one(admin)
                .await
                .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(
            (deadline - now).max(0) as u64 + 20,
        ))
        .await;
    }
    until(admin, seat_deadline).await;
    assert!(
        store
            .claim(&worker("deadline"), duration())
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
        JobPhase::Waiting
    );
    let known = OperationCompletion {
        lease: executing.lease.clone(),
        operation: operation.clone(),
        output: Vec::new(),
        output_truncated: false,
        receipt: OperationReceipt {
            id: operation.id.clone(),
            environment_id: operation.environment_id.clone(),
            phase: OperationPhase::Completed,
            exit_code: Some(0),
            cancellation_requested: true,
        },
    };
    let operations = backend.gateway_operations(gateway);
    if receipt_before_grace {
        operations.complete(&known).await.unwrap();
        let synthesis = store
            .claim(&worker("synthesis-after-receipt"), duration())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(synthesis.job.id, prepared.nodes[2].child.job_id);
        complete_child_with_text(
            backend,
            admin,
            &synthesis,
            "Synthesis after inspecting the late receipt",
        )
        .await;
    } else {
        let deadline:i64=query_scalar("SELECT stop_deadline_at FROM zuno_enterprise_preview.runtime_council WHERE tenant_id=$1")
            .bind(actor.tenant_id().as_str()).fetch_one(admin).await.unwrap();
        until(admin, deadline).await;
    }
    let mut resumed = None;
    for _ in 0..3 {
        if let Some(job) = store.claim(&worker("parent"), duration()).await.unwrap() {
            resumed = Some(job);
            break;
        }
    }
    assert_eq!(resumed.unwrap().job.id, parent.job.id);
    assert_eq!(
        store
            .get(&actor.owner(), &prepared.group.job_id)
            .await
            .unwrap()
            .phase,
        if receipt_before_grace {
            JobPhase::Completed
        } else {
            JobPhase::Uncertain
        }
    );
    operations.complete(&known).await.unwrap();
    assert!(
        operations
            .completion(&actor.owner(), &operation.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .claim(&worker("late-receipt"), duration())
            .await
            .unwrap()
            .is_none()
    );
}
async fn repair_and_quorum(backend: &PostgresBackend, admin: &PgPool) {
    let (actor, parent, prepared) = start(backend, admin, "council-repair", 3, 1, false).await;
    let store = backend.runtime(actor.tenant_id().clone());
    let worker_one = worker("one");
    let worker_two = worker("two");
    // A non-blocking claim may see no candidate while the other transaction
    // advances the coordinator or locks a session. Keep both claimers racing,
    // but do not require every individual poll to find work.
    async fn claim_seat(
        store: &crate::PostgresRuntimeStore,
        admin: &PgPool,
        actor: &PrincipalScope,
        worker: &zuno_types::identity::WorkerInstanceId,
    ) -> zuno_application::runtime::ClaimedJob {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if let Some(job) = store.claim(worker, duration()).await.unwrap() {
                return job;
            }
            if tokio::time::Instant::now() >= deadline {
                let state: serde_json::Value = query_scalar(
                    "SELECT jsonb_build_object(
                        'jobs',(SELECT jsonb_agg(jsonb_build_object('job',job_id,'phase',phase,
                            'session',session_id,'readyAt',ready_at))
                            FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1 AND principal_id=$2),
                        'sessions',(SELECT jsonb_agg(to_jsonb(s))
                            FROM zuno_enterprise_preview.runtime_session s WHERE tenant_id=$1 AND principal_id=$2),
                        'clockMs',floor(extract(epoch FROM clock_timestamp())*1000)::bigint)"
                ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str())
                    .fetch_one(admin).await.unwrap();
                panic!("Council seat remained unclaimable: {state}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    let (one, two) = tokio::join!(
        claim_seat(&store, admin, &actor, &worker_one),
        claim_seat(&store, admin, &actor, &worker_two)
    );
    assert_ne!(one.job.id, two.job.id);
    complete_child_with_text(backend, admin, &one, "not valid structured data").await;
    complete_child_with_text(backend, admin, &two, &answer("agree")).await;
    let next = store
        .claim(&worker("third"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.job.id, prepared.nodes[2].child.job_id);
    complete_child_with_text(backend, admin, &next, &answer("dissent")).await;
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let repair = loop {
        if let Some(job) = store.claim(&worker("repair"), duration()).await.unwrap() {
            break job;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(
        !prepared
            .nodes
            .iter()
            .any(|node| node.child.job_id == repair.job.id)
    );
    assert_eq!(query_scalar::<_,String>("SELECT workspace_policy FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND job_id=$2")
        .bind(actor.tenant_id().as_str()).bind(repair.job.id.as_str()).fetch_one(admin).await.unwrap(),"model_only");
    let input:String=query_scalar("SELECT prompt->'prompt'->>'text' FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND id=$2")
        .bind(actor.tenant_id().as_str()).bind(repair.job.input_id.as_str()).fetch_one(admin).await.unwrap();
    assert!(input.contains("not valid structured data"));
    assert!(!input.contains("private reasoning"));
    assert!(input.contains("without repeating its work"));
    complete_child_with_text(backend, admin, &repair, &answer("corrected")).await;
    let synthesis = store
        .claim(&worker("synthesis"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(synthesis.job.id, prepared.nodes[3].child.job_id);
    let input:String=query_scalar("SELECT prompt->'prompt'->>'text' FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND id=$2")
        .bind(actor.tenant_id().as_str()).bind(synthesis.job.input_id.as_str()).fetch_one(admin).await.unwrap();
    for expected in ["corrected", "agree", "dissent", "unverified external state"] {
        assert!(input.contains(expected), "{input}");
    }
    assert!(!input.contains("private reasoning"));
    complete_child_with_text(backend, admin, &synthesis, "Synthesis retaining dissent").await;
    let mut resumed = None;
    for _ in 0..3 {
        if let Some(job) = store
            .claim(&worker("replacement-parent"), duration())
            .await
            .unwrap()
        {
            resumed = Some(job);
            break;
        }
    }
    let resumed = resumed.expect("Council completion wakes its original parent");
    assert_eq!(resumed.job.id, parent.job.id);
    assert_ne!(resumed.lease.attempt_id, parent.lease.attempt_id);
    let view = backend
        .client_workflow(&actor, &prepared.group.job_id)
        .await
        .unwrap();
    let council = view.council.unwrap();
    assert_eq!(
        council.phase,
        zuno_application::council::CouncilPhase::Completed
    );
    assert_eq!(
        council.seats.iter().map(|seat| seat.attempts).sum::<u32>(),
        4
    );
    assert_eq!(
        council
            .seats
            .iter()
            .map(|seat| seat.id.as_str())
            .collect::<Vec<_>>(),
        ["seat-0", "seat-1", "seat-2"]
    );
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    let completion = crate::runtime::children::ready(&mut tx, &resumed.job, &prepared.group.wait)
        .await
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_string(&completion)
            .unwrap()
            .contains("Synthesis retaining dissent")
    );
    crate::runtime::waiting::consume(&mut tx, &resumed.job, std::slice::from_ref(&completion))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Repeated terminal observation never produces another repair, synthesis or vote.
    assert!(
        store
            .claim(&worker("duplicate-observer"), duration())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.runtime_council_attempt WHERE tenant_id=$1")
        .bind(actor.tenant_id().as_str()).fetch_one(admin).await.unwrap(),4);
    assert!(
        backend
            .client_workflow(
                &principal("council-repair", "other"),
                &prepared.group.job_id
            )
            .await
            .is_err()
    );
}

async fn deadline(backend: &PostgresBackend, admin: &PgPool, has_quorum: bool) {
    let name = format!("council-deadline-{has_quorum}");
    let (actor, parent, prepared) = start(backend, admin, &name, 2, 0, true).await;
    let store = backend.runtime(actor.tenant_id().clone());
    let one = store
        .claim(&worker("one"), duration())
        .await
        .unwrap()
        .unwrap();
    let two = store
        .claim(&worker("two"), duration())
        .await
        .unwrap()
        .unwrap();
    let deadline: i64 = query_scalar(
        "SELECT seat_deadline_at FROM zuno_enterprise_preview.runtime_council WHERE tenant_id=$1",
    )
    .bind(actor.tenant_id().as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    assert_eq!(one.lease.expires_at_ms, deadline);
    assert_eq!(
        store
            .renew(&one.lease, duration())
            .await
            .unwrap()
            .expires_at_ms,
        deadline
    );
    if has_quorum {
        complete_child_with_text(backend, admin, &one, &answer("validated")).await;
    }
    consume(admin, &two.job).await;
    let wait = WaitRef {
        id: WaitId::new("council-human").unwrap(),
        turn_id: two.job.turn_id.clone(),
        invocation_id: InvocationId::new("human-input").unwrap(),
        arguments_sha256: "d".repeat(64),
        target: WaitTarget::UserInput {
            request_id: RequestId::new("human").unwrap(),
        },
        continuation: WaitContinuation::CurrentTurn,
    };
    let mut tx = crate::owner_transaction(&backend.pool, &actor.owner())
        .await
        .unwrap();
    crate::runtime::suspend_in(
        &mut tx,
        &two.lease,
        checkpoint(&two.job),
        std::slice::from_ref(&wait),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        store
            .claim(&worker("waiting-releases-worker"), duration())
            .await
            .unwrap()
            .is_none()
    );
    let now: i64 = query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint")
        .fetch_one(admin)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(
        (deadline - now).max(0) as u64 + 20,
    ))
    .await;
    let next = store
        .claim(&worker("deadline-coordinator"), duration())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        store.renew(&two.lease, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    assert!(matches!(
        store
            .finish(&two.lease, JobFinish::Completed { result: json!({}) })
            .await,
        Err(ApplicationError::LeaseLost)
    ));
    if has_quorum {
        assert_eq!(next.job.id, prepared.nodes[2].child.job_id);
        complete_child_with_text(
            backend,
            admin,
            &next,
            "Synthesis explicitly notes the timed out seat",
        )
        .await;
        let mut resumed = None;
        for _ in 0..3 {
            if let Some(job) = store.claim(&worker("resume"), duration()).await.unwrap() {
                resumed = Some(job);
                break;
            }
        }
        assert_eq!(resumed.unwrap().job.id, parent.job.id);
    } else {
        assert_eq!(next.job.id, parent.job.id);
    }
    let view = backend
        .client_workflow(&actor, &prepared.group.job_id)
        .await
        .unwrap();
    let council = view.council.unwrap();
    assert_eq!(
        council.phase,
        if has_quorum {
            zuno_application::council::CouncilPhase::Completed
        } else {
            zuno_application::council::CouncilPhase::Failed
        }
    );
    assert!(
        council
            .seats
            .iter()
            .any(|seat| seat.state == zuno_application::council::CouncilSeatState::TimedOut)
    );
    // A late answer is preserved by ordinary cancellation semantics and cannot wake the Council.
    let completion = zuno_engine::wait::WaitCompletion::tool_result(
        CompletionId::new("late-human").unwrap(),
        wait,
        zuno_engine::r#loop::ToolDispatchResult::success(zuno_tool::ToolOutput::text(
            "Human", "too late",
        )),
    );
    let _ = store
        .publish_completion(&actor.owner(), &two.job.id, &completion)
        .await;
    assert!(
        store
            .claim(&worker("late-observer"), duration())
            .await
            .unwrap()
            .is_none()
    );
}

async fn cancelled(backend: &PostgresBackend, admin: &PgPool) {
    use zuno_application::control::{CancelJob, RuntimeControl};
    let (actor, parent, prepared) = start(backend, admin, "council-cancel", 2, 1, false).await;
    let store = backend.runtime(actor.tenant_id().clone());
    let child = store
        .claim(&worker("seat"), duration())
        .await
        .unwrap()
        .unwrap();
    store
        .cancel(
            &actor,
            &parent.job.id,
            CancelJob {
                request_id: RequestId::new("stop").unwrap(),
                expected_turn_id: parent.job.turn_id.clone(),
                reason: "Question withdrawn".to_owned(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        store.renew(&child.lease, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
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
}
