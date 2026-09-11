use super::*;
use zuno_application::authorization::*;
use zuno_application::runtime::{ClaimedJob, ExecutionLease, JobPhase};
use zuno_engine::r#loop::{AvailableTools, DispatchRequest, PreparedToolDispatch, ToolDispatcher};
use zuno_permission::enterprise::{EffectKind, IsolationFact, PreparedEffectFacts};
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

struct ApprovalDispatcher {
    inner: ToolRegistryDispatcher,
    store: crate::PostgresOrganizationStore,
    lease: ExecutionLease,
    proposal: ApprovalProposal,
}
#[async_trait]
impl ToolDispatcher for ApprovalDispatcher {
    fn available_tools(&self) -> AvailableTools {
        self.inner.available_tools()
    }
    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        if request.call.id == "wait" {
            let approval = self
                .store
                .admit(&self.lease, self.proposal.clone())
                .await
                .unwrap();
            if approval.state == ApprovalState::Pending {
                return PreparedToolDispatch::Pending(WaitRef {
                    id: WaitId::new(format!("wait_{}", approval.id)).unwrap(),
                    turn_id: approval.binding.turn_id,
                    invocation_id: approval.binding.invocation_id,
                    arguments_sha256: zuno_orchestration::sha256_json(&request.call.input),
                    target: WaitTarget::Approval {
                        approval_id: approval.id,
                    },
                    continuation: WaitContinuation::CurrentTurn,
                });
            }
            assert_eq!(approval.state, ApprovalState::Approved);
            self.store
                .check_execution(&self.lease, self.proposal.clone())
                .await
                .unwrap();
        }
        self.inner.prepare(request).await
    }
}
fn dispatcher(
    backend: &PostgresBackend,
    claimed: &ClaimedJob,
    calls: &Arc<AtomicUsize>,
    proposal: &ApprovalProposal,
) -> ApprovalDispatcher {
    ApprovalDispatcher {
        inner: ToolRegistryDispatcher::new(
            vec![Arc::new(Inspect(calls.clone()))],
            vec![],
            Arc::new(AllowAll),
            AuthorizationPolicy::Strict,
            McpToolStatus::Ready,
        ),
        store: backend.organizations(claimed.job.principal.tenant_id().clone()),
        lease: claimed.lease.clone(),
        proposal: proposal.clone(),
    }
}

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let actor = PrincipalScope::new(
        TenantId::new("approval-kernel").unwrap(),
        PrincipalId::new("alice").unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("web").unwrap()),
        NonZeroU64::MIN,
    );
    let app = actor.client_id().unwrap().clone();
    bootstrap_organization(
        migrator,
        &OrganizationPolicy {
            tenant_id: actor.tenant_id().clone(),
            revision: NonZeroU64::MIN,
            allowed_apps: [app.clone()].into(),
            approval_apps: [app.clone()].into(),
            auto_read_apps: [app].into(),
            approval_lifetime_seconds: 300,
        },
        &actor.owner(),
    )
    .await
    .unwrap();
    let job = super::waits::seed_job(backend, admin, &actor, "approval").await;
    let runtime = backend.runtime(actor.tenant_id().clone());
    let duration = LeaseDuration::new(300_000).unwrap();
    let first = runtime
        .claim(&WorkerInstanceId::new("first").unwrap(), duration)
        .await
        .unwrap()
        .unwrap();
    let scope = TurnStateScope {
        owner: actor.owner(),
        session_id: job.session_id.to_string(),
    };
    let state = backend
        .turn_state(first.lease.clone(), "/workspace".to_owned())
        .unwrap();
    state.consume_input(&scope,InputMaterialization {
        turn_id:None,input_id:Some(job.input_id.to_string()),
        message:MessageRecord::from_json(json!({
            "id":job.input_id,"sessionID":job.session_id,"role":"user","time":{"created":0},
            "agent":"build","model":{"providerID":"turn-test","modelID":"model"},
        })).unwrap(),
        parts:vec![PartRecord::from_json(json!({
            "id":format!("part_{}",job.input_id),"sessionID":job.session_id,"messageID":job.input_id,
            "type":"text","text":"approval",
        }),0).unwrap()],
    }).await.unwrap();
    let mut tools = Vec::new();
    for id in ["before", "wait", "after"] {
        tools.extend([
            StreamEvent::ToolUseStart {
                id: id.to_owned(),
                name: "inspect".to_owned(),
            },
            StreamEvent::ToolInputDelta {
                id: id.to_owned(),
                delta: "{}".to_owned(),
            },
            StreamEvent::ToolUseEnd { id: id.to_owned() },
        ]);
    }
    tools.extend([
        usage(),
        StreamEvent::MessageEnd {
            stop_reason: Some(FinishReason::ToolCalls),
        },
    ]);
    let script = Arc::new(Script {
        replies: Mutex::new(VecDeque::from([
            tools,
            vec![
                StreamEvent::TextDelta("Complete".to_owned()),
                usage(),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
        ])),
        requests: AtomicUsize::new(0),
    });
    let mut providers = ProviderRegistry::new();
    let shared = script.clone();
    providers.register("turn-test", move |_| shared.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let proposal = ApprovalProposal {
        binding: ApprovalBinding {
            job_id: job.id.clone(),
            session_id: job.session_id.clone(),
            turn_id: job.turn_id.clone(),
            invocation_id: InvocationId::new("wait").unwrap(),
            operation_id: OperationId::new("approved-once").unwrap(),
            arguments_sha256: zuno_orchestration::sha256_json(&json!({})),
            resources_sha256: "b".repeat(64),
            effect: EffectKind::Process,
        },
        facts: PreparedEffectFacts {
            kind: EffectKind::Process,
            resource_authorized: true,
            isolation: IsolationFact::Enforced,
            builtin_handler: true,
            sensitive: false,
            explicit_deny: false,
            mandatory_human: true,
        },
        presentation: json!({"operation":"inspect fixture"}),
    };
    let outcome = super::waits::advance(
        backend,
        &first,
        &providers,
        &dispatcher(backend, &first, &calls, &proposal),
    )
    .await
    .unwrap();
    let AdvanceOutcome::Waiting { waits, .. } = outcome else {
        panic!("approval wait")
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Waiting
    );
    assert!(
        runtime
            .claim(&WorkerInstanceId::new("idle").unwrap(), duration)
            .await
            .unwrap()
            .is_none()
    );
    let WaitTarget::Approval { approval_id } = &waits[0].target else {
        panic!("approval")
    };
    let store = backend.organizations(actor.tenant_id().clone());
    store
        .answer(
            &actor,
            AnswerApproval {
                request_id: RequestId::new("approve").unwrap(),
                approval_id: approval_id.clone(),
                answer: ApprovalAnswer::Approve,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Ready
    );
    let second = runtime
        .claim(&WorkerInstanceId::new("second").unwrap(), duration)
        .await
        .unwrap()
        .unwrap();
    let outcome = super::waits::advance(
        backend,
        &second,
        &providers,
        &dispatcher(backend, &second, &calls, &proposal),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, AdvanceOutcome::Progressed { .. }));
    let pending:serde_json::Value=query_scalar(
        "SELECT data FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND data->>'callID'='wait'",
    ).bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job.session_id.as_str())
        .fetch_one(admin).await.unwrap();
    assert_eq!(pending["state"]["status"], "pending");
    assert!(pending["state"].get("waitRef").is_none() && pending["state"].get("output").is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(script.requests.load(Ordering::SeqCst), 1);
    // Losing the consumption response changes the claimant, not the invocation.
    let third = runtime
        .claim(&WorkerInstanceId::new("third").unwrap(), duration)
        .await
        .unwrap()
        .unwrap();
    assert!(
        backend
            .turn_state(second.lease, "/workspace".to_owned())
            .unwrap()
            .touch(&scope)
            .await
            .is_err()
    );
    let outcome = super::waits::advance(
        backend,
        &third,
        &providers,
        &dispatcher(backend, &third, &calls, &proposal),
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        AdvanceOutcome::Completed { steps: 2, .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(script.requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.get(&actor.owner(), &job.id).await.unwrap().phase,
        JobPhase::Completed
    );
}
