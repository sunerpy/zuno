use super::*;
use serde_json::Value;
use zuno_application::activity::{ActivityPersistence, FrameQuery, HistoryQuery};
use zuno_db::assistant_commit::AssistantCommit;
use zuno_db::message::{MessageRecord, PartRecord};
use zuno_engine::state::{ToolPartCommitKind, TurnPersistence, TurnStateScope};
use zuno_types::activity::{InvocationState, SessionItem};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("activity-history", "alice");
    crate::tests::install_access(admin, &actor).await;
    let session_id = session(backend, &actor, "session").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let job = runtime
        .submit(&actor, submission(&session_id, "input", 0))
        .await
        .unwrap();
    let claim = runtime
        .claim(&worker("activity-worker"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(job.id, claim.job.id);
    let state = backend.worker_state(claim.lease);
    let scope = TurnStateScope {
        owner: actor.owner(),
        session_id: session_id.to_string(),
    };
    let message = MessageRecord::from_json(json!({
        "id":"answer","sessionID":session_id,"role":"assistant","time":{"created":100},
        "privateCredential":"hidden-token","providerOptions":{"private":"hidden-option"}
    }))
    .unwrap();
    let make_part = |id: &str, payload: Value| {
        let mut value = payload;
        value["id"] = json!(id);
        value["sessionID"] = json!(session_id);
        value["messageID"] = json!("answer");
        PartRecord::from_json(value, 100).unwrap()
    };
    let mut parts = (0..8)
        .map(|i| {
            make_part(
                &format!("text-{i}"),
                json!({"type":"text","text":format!("Text {i}")}),
            )
        })
        .collect::<Vec<_>>();
    parts.push(make_part("reasoning",json!({"type":"reasoning","text":"Visible reasoning","metadata":{"signature":"hidden-signature"}})));
    parts.push(make_part("reasoning-capsule",json!({"type":"reasoning","text":"Visible reasoning","metadata":{"providerReasoning":{"encryptedContent":"hidden-ciphertext"}}})));
    let pending = make_part(
        "invocation",
        json!({
            "type":"tool","callID":"read-call","tool":"read","state":{
                "status":"pending","input":{},"dispatchTracked":true,"dispatchedAtMs":101
            }
        }),
    );
    parts.push(pending.clone());
    state
        .commit_assistant(
            &scope,
            &AssistantCommit {
                message,
                parts,
                persisted_at_ms: 102,
                context_limit: None,
                context_usage: None,
            },
        )
        .await
        .unwrap();
    let activity = backend.activity(actor.clone());
    let initial = activity
        .history(&session_id, HistoryQuery::default())
        .await
        .unwrap();
    assert_eq!(
        initial
            .items
            .iter()
            .filter(|item| matches!(item.record.item, SessionItem::Thinking { .. }))
            .count(),
        1,
        "a reasoning capsule cannot duplicate an already visible summary"
    );
    let text = serde_json::to_string(&initial).unwrap();
    for private in [
        "hidden-token",
        "hidden-option",
        "hidden-signature",
        "hidden-ciphertext",
    ] {
        assert!(!text.contains(private));
    }
    let first = activity
        .history(
            &session_id,
            HistoryQuery {
                limit: zuno_application::PageSize::new(3).unwrap(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(first.before.is_some());

    let mut completed = pending;
    completed.data["state"] = json!({"status":"completed","input":{},"output":"Latest result","metadata":{"secret":"hidden-tool-metadata"}});
    state
        .commit_tool_parts(&scope, &[completed], ToolPartCommitKind::Result, 103)
        .await
        .unwrap();
    let old = activity
        .history(
            &session_id,
            HistoryQuery {
                through: Some(initial.through),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        old, initial,
        "a historical snapshot is immutable while the invocation changes"
    );
    let next = activity
        .frames(
            &session_id,
            FrameQuery {
                after: initial.through,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(next.frames.len() == 1);
    let fresh = activity
        .history(&session_id, HistoryQuery::default())
        .await
        .unwrap();
    assert!(fresh.items.iter().any(|item| matches!(&item.record.item,SessionItem::Invocation { invocation } if invocation.state==InvocationState::Succeeded)));

    let mut all = first.items.clone();
    let mut before = first.before;
    while before.is_some() {
        let page = activity
            .history(
                &session_id,
                HistoryQuery {
                    limit: zuno_application::PageSize::new(3).unwrap(),
                    before,
                    through: Some(first.through),
                },
            )
            .await
            .unwrap();
        before = page.before;
        all.extend(page.items);
    }
    assert_eq!(all.len(), initial.items.len());
    assert_eq!(
        all.iter()
            .map(|item| &item.record.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        all.len()
    );
    let stranger = principal("activity-history", "bob");
    crate::tests::install_access(admin, &stranger).await;
    assert!(matches!(
        backend
            .activity(stranger)
            .history(&session_id, HistoryQuery::default())
            .await,
        Err(ApplicationError::NotFound)
    ));

    raw_sql(
        "CREATE FUNCTION public.refuse_public_frame() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.tenant_id='activity-history' AND NEW.item_id='message:rollback-answer'
        THEN RAISE EXCEPTION 'injected projection failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_public_frame BEFORE INSERT ON zuno_enterprise_preview.activity_frame
        FOR EACH ROW EXECUTE FUNCTION public.refuse_public_frame();",
    )
    .execute(admin)
    .await
    .unwrap();
    let rollback = MessageRecord::from_json(json!({
        "id":"rollback-answer","sessionID":session_id,"role":"assistant","time":{"created":200}
    }))
    .unwrap();
    assert!(
        state
            .commit_assistant(
                &scope,
                &AssistantCommit {
                    message: rollback,
                    parts: vec![],
                    persisted_at_ms: 201,
                    context_limit: None,
                    context_usage: None,
                }
            )
            .await
            .is_err()
    );
    raw_sql("DROP TRIGGER refuse_public_frame ON zuno_enterprise_preview.activity_frame; DROP FUNCTION public.refuse_public_frame();")
        .execute(admin).await.unwrap();
    assert_eq!(
        activity
            .history(&session_id, HistoryQuery::default())
            .await
            .unwrap(),
        fresh
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.message WHERE tenant_id='activity-history' AND id='rollback-answer'")
        .fetch_one(admin).await.unwrap(),0,"source and client projection roll back together");
    reused_call_id_keeps_original_job(backend, admin).await;
}

async fn reused_call_id_keeps_original_job(backend: &PostgresBackend, admin: &PgPool) {
    use zuno_application::control::{CancelJob, RuntimeControl};
    use zuno_types::identity::{ApprovalId, InvocationId, WaitId};
    use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};
    let actor = principal("activity-call-scope", "alice");
    crate::tests::install_access(admin, &actor).await;
    let session_id = session(backend, &actor, "session").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let first = runtime
        .submit(&actor, submission(&session_id, "first", 0))
        .await
        .unwrap();
    let old = runtime
        .claim(&worker("old"), duration())
        .await
        .unwrap()
        .unwrap();
    let scope = TurnStateScope {
        owner: actor.owner(),
        session_id: session_id.to_string(),
    };
    let records = |name: &str| {
        AssistantCommit {
        message:MessageRecord::from_json(json!({"id":name,"sessionID":session_id,"role":"assistant","time":{"created":100}})).unwrap(),
        parts:vec![PartRecord::from_json(json!({
            "id":format!("{name}-call"),"sessionID":session_id,"messageID":name,"type":"tool","callID":"reused","tool":"shell",
            "state":{"status":"pending","input":{"command":name},"dispatchedAtMs":101,"dispatchTracked":true}
        }),100).unwrap()],
        persisted_at_ms:101,context_limit:None,context_usage:None,
    }
    };
    backend
        .worker_state(old.lease)
        .commit_assistant(&scope, &records("old"))
        .await
        .unwrap();
    runtime
        .cancel(
            &actor,
            &first.id,
            CancelJob {
                request_id: RequestId::new("cancel-first").unwrap(),
                expected_turn_id: first.turn_id,
                reason: "Stop the old task".to_owned(),
            },
        )
        .await
        .unwrap();
    let version = runtime
        .input_version(&actor.owner(), &session_id)
        .await
        .unwrap();
    let next = runtime
        .submit(&actor, submission(&session_id, "next", version))
        .await
        .unwrap();
    let active = runtime
        .claim(&worker("new"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.job.id, next.id);
    backend
        .worker_state(active.lease)
        .commit_assistant(&scope, &records("new"))
        .await
        .unwrap();
    let reference = WaitRef {
        id: WaitId::new("new-approval-wait").unwrap(),
        turn_id: next.turn_id.clone(),
        invocation_id: InvocationId::new("reused").unwrap(),
        arguments_sha256: "a".repeat(64),
        target: WaitTarget::Approval {
            approval_id: ApprovalId::new("new-approval").unwrap(),
        },
        continuation: WaitContinuation::CurrentTurn,
    };
    // The stored waiting coordinates are sufficient for this projection test;
    // approval authorization is exercised by the separate end-to-end fixtures.
    query("INSERT INTO zuno_enterprise_preview.runtime_wait
        (tenant_id,principal_id,id,job_id,session_id,turn_id,invocation_id,reference,state,time_created,time_updated)
        VALUES($1,$2,$3,$4,$5,$6,'reused',$7,'pending',102,102)")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(reference.id.as_str())
        .bind(next.id.as_str()).bind(session_id.as_str()).bind(next.turn_id.as_str()).bind(json!(reference))
        .execute(admin).await.unwrap();
    let mut tx = scoped_transaction(&backend.pool, &actor).await.unwrap();
    crate::activity::execution_changed(
        &mut tx,
        &actor.owner(),
        session_id.as_str(),
        next.id.as_str(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let history = backend
        .activity(actor)
        .history(&session_id, HistoryQuery::default())
        .await
        .unwrap();
    let mut verified = 0;
    for item in history.items {
        if let SessionItem::Invocation { invocation } = item.record.item {
            if item.record.id == "part:old-call" {
                verified += 1;
                assert_eq!(invocation.state, InvocationState::Uncertain);
                assert!(
                    invocation.waiting_for.is_none(),
                    "a new turn's approval cannot attach to an older call with the same ID"
                );
            } else if item.record.id == "part:new-call" {
                verified += 1;
                assert_eq!(invocation.state, InvocationState::Waiting);
                assert!(
                    matches!(invocation.waiting_for,Some(zuno_types::activity::WaitingFor::Approval {approval_id}) if approval_id.as_str()=="new-approval")
                );
            }
        }
    }
    assert_eq!(
        verified, 2,
        "both original and reused calls must remain visible"
    );
}
