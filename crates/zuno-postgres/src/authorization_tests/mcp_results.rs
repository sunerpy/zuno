use super::*;
use serde_json::Value;
use sqlx_core::raw_sql::raw_sql;
use zuno_application::mcp::*;
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "mcp-results").await;
    let mut proposal = proposal(&f, "mcp", EffectKind::ExternalTool);
    proposal.facts.mandatory_human = true;
    let admission = call(&f, &proposal);
    let operation = admission.operation.clone();
    let store = backend.mcp_operations(admission.gateway_id.clone());
    store.offer(&admission).await.unwrap();
    proposal.binding.arguments_sha256 = admission.arguments_digest();
    proposal.binding.resources_sha256 = admission.resources_digest();
    let approval = f.authority.admit(&f.lease, proposal.clone()).await.unwrap();
    assert_eq!(approval.state, ApprovalState::Pending);
    assert!(
        f.authority
            .check_mcp_execution(proposal.clone(), &admission)
            .await
            .is_err()
    );
    assert!(
        backend
            .mcp_for_approval(&f.outsider, &approval.id)
            .await
            .is_err()
    );
    assert_eq!(
        backend
            .mcp_for_approval(&f.owner, &approval.id)
            .await
            .unwrap()
            .0
            .operation,
        admission.operation
    );
    let mut changed = admission.clone();
    changed.operation.arguments = json!({"value":"different"});
    assert!(
        store.offer(&changed).await.is_err(),
        "an existing operation cannot switch reviewed content"
    );
    f.authority
        .answer(&f.owner, answer(&approval.id, "approve-mcp"))
        .await
        .unwrap();
    raw_sql("CREATE FUNCTION public.refuse_mcp_attempt() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.tenant_id='mcp-results' THEN RAISE EXCEPTION 'injected MCP admission'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_mcp_attempt BEFORE INSERT ON zuno_enterprise_preview.gateway_mcp_attempt
        FOR EACH ROW EXECUTE FUNCTION public.refuse_mcp_attempt();").execute(admin).await.unwrap();
    assert!(
        f.authority
            .check_mcp_execution(proposal.clone(), &admission)
            .await
            .is_err()
    );
    assert!(!query_scalar::<_,bool>("SELECT admitted FROM zuno_enterprise_preview.gateway_mcp_operation WHERE tenant_id='mcp-results'")
        .fetch_one(admin).await.unwrap(),"approval admission and attempt must roll back together");
    raw_sql("DROP TRIGGER refuse_mcp_attempt ON zuno_enterprise_preview.gateway_mcp_attempt; DROP FUNCTION public.refuse_mcp_attempt();")
        .execute(admin).await.unwrap();
    f.authority
        .check_mcp_execution(proposal.clone(), &admission)
        .await
        .unwrap();
    let reference = WaitRef {
        id: WaitId::new("mcp-wait").unwrap(),
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
    let completion = McpCompletion {
        admission: admission.clone(),
        receipt: McpReceipt {
            id: operation.id.clone(),
            state: McpOperationState::Succeeded,
            request_digest: operation.digest(),
            cancellation_requested: false,
            result: Some(json!({"content":[{"type":"text","text":"done"}],"isError":false})),
            failure: None,
        },
    };
    raw_sql("CREATE FUNCTION public.refuse_mcp_ready() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.tenant_id='mcp-results' AND NEW.type='runtime.wait.completed' THEN RAISE EXCEPTION 'injected MCP wake'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_mcp_ready BEFORE INSERT ON zuno_enterprise_preview.event FOR EACH ROW EXECUTE FUNCTION public.refuse_mcp_ready();")
        .execute(admin).await.unwrap();
    assert!(store.complete(&completion).await.is_err());
    assert!(query_scalar::<_,Option<Value>>("SELECT completion FROM zuno_enterprise_preview.gateway_mcp_operation WHERE tenant_id='mcp-results'")
        .fetch_one(admin).await.unwrap().is_none());
    raw_sql("DROP TRIGGER refuse_mcp_ready ON zuno_enterprise_preview.event; DROP FUNCTION public.refuse_mcp_ready();").execute(admin).await.unwrap();
    query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id='mcp-results'").execute(admin).await.unwrap();
    assert!(
        f.authority
            .check_mcp_execution(proposal.clone(), &admission)
            .await
            .is_err(),
        "a released Worker lease cannot admit another submission"
    );
    f.authority
        .check_admitted_mcp(proposal.clone(), &admission)
        .await
        .unwrap();
    store.complete(&completion).await.unwrap();
    assert!(
        f.authority
            .check_admitted_mcp(proposal, &admission)
            .await
            .is_err(),
        "an already completed operation cannot start again"
    );
    store.complete(&completion).await.unwrap();
    let mut forged = completion;
    forged.admission.lease.attempt_id = ExecutionAttemptId::new("unadmitted").unwrap();
    assert!(
        store.complete(&forged).await.is_err(),
        "late facts require an originally admitted attempt"
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.event WHERE tenant_id='mcp-results' AND type='runtime.mcp.completed'")
        .fetch_one(admin).await.unwrap(),1);
    cancellation(backend, admin, migrator).await;
}

async fn cancellation(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "mcp-cancel").await;
    let mut proposal = proposal(&f, "cancel-mcp", EffectKind::ExternalTool);
    proposal.facts.mandatory_human = true;
    let admission = call(&f, &proposal);
    let operation = admission.operation.clone();
    let store = backend.mcp_operations(admission.gateway_id.clone());
    store.offer(&admission).await.unwrap();
    proposal.binding.arguments_sha256 = admission.arguments_digest();
    proposal.binding.resources_sha256 = admission.resources_digest();
    let approval = f.authority.admit(&f.lease, proposal.clone()).await.unwrap();
    f.authority
        .answer(&f.owner, answer(&approval.id, "approve"))
        .await
        .unwrap();
    f.authority
        .check_mcp_execution(proposal.clone(), &admission)
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
                reason: "Stop the MCP operation".to_owned(),
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
            .check_mcp_execution(proposal, &admission)
            .await
            .is_err()
    );
    store
        .complete(&McpCompletion {
            admission,
            receipt: McpReceipt {
                id: operation.id.clone(),
                state: McpOperationState::Succeeded,
                request_digest: operation.digest(),
                cancellation_requested: false,
                result: Some(json!({"content":[{"type":"text","text":"done"}],"isError":false})),
                failure: None,
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

fn call(f: &Fixture, proposal: &ApprovalProposal) -> McpAdmission {
    McpAdmission {
        gateway_id: GatewayId::new("gateway").unwrap(),
        lease: f.lease.clone(),
        operation: McpOperation {
            id: proposal.binding.operation_id.clone(),
            invocation_id: proposal.binding.invocation_id.clone(),
            environment_id: EnvironmentId::new("mcp-env").unwrap(),
            binding: McpToolBinding {
                endpoint: "https://mcp.example/tool".to_owned(),
                connection: zuno_types::activity::ActivityName::new("connection").unwrap(),
                server: zuno_types::activity::ActivityName::new("server").unwrap(),
                tool: zuno_types::activity::ActivityName::new("apply").unwrap(),
                revision: 1,
                definition: json!({"name":"apply","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}),
            },
            arguments: json!({"value":"reviewed"}),
        },
    }
}
