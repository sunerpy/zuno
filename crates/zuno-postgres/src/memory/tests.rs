mod evidence;

use super::*;
use sqlx_core::raw_sql::raw_sql;
use sqlx_postgres::PgPool;
use std::num::NonZeroU64;
use zuno_application::{
    AgentApplication, CreateSession,
    runtime::{ConfigurationRef, JobSubmission, LeaseDuration, RuntimeStore},
};
use zuno_memory::remote::MemoryChange;
use zuno_types::{
    MemoryAction, MemoryCandidateProjection, MemoryCandidateStatus,
    identity::{
        ClientId, ConfigurationId, PrincipalId, PrincipalKind, RequestId, TenantId,
        WorkerInstanceId,
    },
};

fn principal(name: &str) -> PrincipalScope {
    PrincipalScope::new(
        TenantId::new("memory").unwrap(),
        PrincipalId::new(name).unwrap(),
        PrincipalKind::User,
        Some(ClientId::new("enterprise-web").unwrap()),
        NonZeroU64::MIN,
    )
}
fn request(id: &str, command: MemoryCommand) -> MemoryRequest {
    MemoryRequest {
        request_id: RequestId::new(id).unwrap(),
        command,
    }
}
fn change(text: &str) -> MemoryCommand {
    MemoryCommand::Propose {
        change: MemoryChange {
            scope: MemoryScope::Project,
            action: MemoryAction::Add,
            content: Some(text.to_owned()),
            old_text: None,
            reason: "remember the repository convention".to_owned(),
            expected_revision: None,
            confidence: 1.0,
        },
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewedCandidate {
    projection: MemoryCandidateProjection,
    state_digest: String,
}
impl std::ops::Deref for ReviewedCandidate {
    type Target = MemoryCandidateProjection;
    fn deref(&self) -> &Self::Target {
        &self.projection
    }
}
fn candidate(reply: MemoryReply) -> ReviewedCandidate {
    let MemoryReply::Candidate {
        candidate,
        state_digest,
    } = reply
    else {
        panic!("expected candidate");
    };
    ReviewedCandidate {
        projection: candidate,
        state_digest,
    }
}
async fn contents(service: &PostgresMemoryService) -> String {
    let MemoryReply::Snapshot { documents } = service
        .request(request("read", MemoryCommand::Read))
        .await
        .unwrap()
    else {
        panic!("snapshot");
    };
    documents
        .into_iter()
        .map(|document| document.content)
        .collect::<Vec<_>>()
        .join("\n")
}

async fn setup(
    backend: &PostgresBackend,
    admin: &PgPool,
    actor: &PrincipalScope,
    workspace: &WorkspaceId,
) -> SessionId {
    crate::tests::install_access(admin, actor).await;
    backend
        .register_workspace(actor, workspace, "Memory workspace")
        .await
        .unwrap();
    AgentApplication::new(Arc::new(backend.sessions(actor.clone())))
        .create_session(CreateSession {
            request_id: RequestId::new(format!("session-{}", workspace.as_str())).unwrap(),
            workspace_id: workspace.clone(),
            title: "Memory".to_owned(),
        })
        .await
        .unwrap()
        .id
}

pub(crate) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let alice = principal("alice");
    let bob = principal("bob");
    let workspace = WorkspaceId::new("memory-workspace").unwrap();
    let second = WorkspaceId::new("other-workspace").unwrap();
    let alice_session = setup(backend, admin, &alice, &workspace).await;
    setup(backend, admin, &bob, &workspace).await;
    setup(backend, admin, &alice, &second).await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let a = memory.for_user(alice.clone(), workspace.clone());
    let b = memory.for_user(bob.clone(), workspace.clone());
    let other = memory.for_user(alice.clone(), second);
    let MemoryReply::Policy { policy } = a
        .request(request(
            "policy",
            MemoryCommand::Policy { session_id: None },
        ))
        .await
        .unwrap()
    else {
        panic!("policy");
    };
    assert!(!policy.generate_private);
    assert!(policy.use_memories);
    assert_eq!(policy.revision, 0);
    assert!(contents(&a).await.trim().is_empty());
    let staged = candidate(
        a.request(request("first-proposal", change("run cargo test")))
            .await
            .unwrap(),
    );
    assert_eq!(staged.status, MemoryCandidateStatus::Pending);
    assert!(contents(&a).await.trim().is_empty());
    let apply = request(
        "first-apply",
        MemoryCommand::Apply {
            candidate_id: staged.id.clone(),
            expected_state: staged.state_digest.clone(),
        },
    );
    let applied = candidate(a.request(apply.clone()).await.unwrap());
    assert_eq!(applied.status, MemoryCandidateStatus::Applied);
    assert_eq!(
        candidate(a.request(apply).await.unwrap()),
        applied,
        "write receipt is replayable without a second mutation"
    );
    assert!(contents(&a).await.contains("run cargo test"));
    assert!(contents(&b).await.trim().is_empty());
    assert!(contents(&other).await.trim().is_empty());
    query("UPDATE zuno_enterprise_preview.organization_policy SET allowed_apps=allowed_apps||'[\"api-client\"]'::jsonb WHERE tenant_id='memory'")
        .execute(admin).await.unwrap();
    let api_actor = PrincipalScope::new(
        bob.tenant_id().clone(),
        bob.principal_id().clone(),
        PrincipalKind::User,
        Some(ClientId::new("api-client").unwrap()),
        NonZeroU64::MIN,
    );
    let api_memory = memory.for_user(api_actor, workspace.clone());
    assert!(contents(&api_memory).await.trim().is_empty());
    assert!(
        matches!(
            api_memory
                .request(request(
                    "untrusted-consent",
                    MemoryCommand::SetPolicy {
                        session_id: None,
                        expected_revision: 0,
                        use_memories: true,
                        generate_private: true,
                    }
                ))
                .await,
            Err(Error::Denied)
        ),
        "an allowed API client cannot grant Memory consent as a trusted approval surface"
    );
    assert!(matches!(
        b.request(request(
            "foreign",
            MemoryCommand::Apply {
                candidate_id: staged.id.clone(),
                expected_state: staged.state_digest.clone(),
            }
        ))
        .await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        other
            .request(request(
                "foreign",
                MemoryCommand::Apply {
                    candidate_id: staged.id.clone(),
                    expected_state: staged.state_digest.clone(),
                }
            ))
            .await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        a.request(request(
            "first-apply",
            MemoryCommand::Undo {
                candidate_id: staged.id.clone(),
                expected_state: applied.state_digest.clone(),
            }
        ))
        .await,
        Err(Error::Conflict)
    ));
    let audits: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.memory_audit WHERE tenant_id='memory' AND principal_id='alice'")
        .fetch_one(admin).await.unwrap();
    assert_eq!(audits, 2);

    // Both proposals saw the same revision; only one apply can commit.
    let one = candidate(
        a.request(request("race-one", change("run cargo fmt")))
            .await
            .unwrap(),
    );
    let two = candidate(
        a.request(request("race-two", change("run cargo clippy")))
            .await
            .unwrap(),
    );
    let (one, two) = tokio::join!(
        a.request(request(
            "apply-one",
            MemoryCommand::Apply {
                candidate_id: one.id.clone(),
                expected_state: one.state_digest.clone(),
            }
        )),
        a.request(request(
            "apply-two",
            MemoryCommand::Apply {
                candidate_id: two.id.clone(),
                expected_state: two.state_digest.clone(),
            }
        )),
    );
    assert_ne!(one.is_ok(), two.is_ok());
    assert!(matches!(
        one.err().or_else(|| two.err()),
        Some(Error::Conflict)
    ));
    let original = candidate(
        a.request(request("review-original", change("original proposal")))
            .await
            .unwrap(),
    );
    let edited = candidate(
        a.request(request(
            "review-edited",
            MemoryCommand::Edit {
                candidate_id: original.id.clone(),
                expected_state: original.state_digest.clone(),
                content: Some("changed proposal".to_owned()),
                old_text: None,
                reason: "Updated review".to_owned(),
            },
        ))
        .await
        .unwrap(),
    );
    assert!(
        matches!(
            a.request(request(
                "stale-review",
                MemoryCommand::Apply {
                    candidate_id: original.id.clone(),
                    expected_state: original.state_digest,
                }
            ))
            .await,
            Err(Error::Conflict)
        ),
        "a stale review must not apply a candidate edited after the reviewer saw it"
    );
    a.request(request(
        "current-review",
        MemoryCommand::Apply {
            candidate_id: edited.id.clone(),
            expected_state: edited.state_digest,
        },
    ))
    .await
    .unwrap();

    let before = memory_rows(admin, &alice).await;
    raw_sql("CREATE FUNCTION zuno_enterprise_preview.refuse_memory_audit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected memory audit failure'; END $$;
        CREATE TRIGGER refuse_memory_audit BEFORE INSERT ON zuno_enterprise_preview.memory_audit
          FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.refuse_memory_audit();").execute(admin).await.unwrap();
    let retried = request("audit-rollback", change("review the diff"));
    assert!(a.request(retried.clone()).await.is_err());
    assert_eq!(
        memory_rows(admin, &alice).await,
        before,
        "candidate, request receipt and audit must roll back together"
    );
    raw_sql("DROP TRIGGER refuse_memory_audit ON zuno_enterprise_preview.memory_audit; DROP FUNCTION zuno_enterprise_preview.refuse_memory_audit();")
        .execute(admin).await.unwrap();
    a.request(retried).await.unwrap();

    model_policy(backend, admin, &memory, &a, &alice, &alice_session).await;
    evidence::exercise(backend, admin, &memory).await;
    timeout_rollback(backend, admin, &alice, &workspace).await;
    let unscoped: i64 =
        query_scalar("SELECT count(*) FROM zuno_enterprise_preview.memory_document")
            .fetch_one(&backend.pool)
            .await
            .unwrap();
    assert_eq!(
        unscoped, 0,
        "a raw runtime connection without an owner sees no Memory"
    );
}

async fn timeout_rollback(
    backend: &PostgresBackend,
    admin: &PgPool,
    actor: &PrincipalScope,
    workspace: &WorkspaceId,
) {
    let memory = PostgresMemoryBackend::new(
        backend.clone(),
        MemoryStoreLimits {
            concurrent_transactions: 1,
            transaction_timeout: Duration::from_secs(1),
            ..Default::default()
        },
    )
    .unwrap();
    raw_sql("CREATE FUNCTION zuno_enterprise_preview.delay_memory_write() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN PERFORM pg_sleep(2); RETURN NEW; END $$;
        CREATE TRIGGER delay_memory_write BEFORE INSERT ON zuno_enterprise_preview.memory_candidate
          FOR EACH ROW EXECUTE FUNCTION zuno_enterprise_preview.delay_memory_write();")
        .execute(admin).await.unwrap();
    let result = memory
        .for_user(actor.clone(), workspace.clone())
        .request(request("timeout-rollback", change("must roll back")))
        .await;
    assert!(matches!(result, Err(Error::Unavailable)));
    let active: bool = query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database()
        AND usename='zuno_preview_runtime' AND wait_event='PgSleep')",
    )
    .fetch_one(admin)
    .await
    .unwrap();
    assert!(
        !active,
        "Memory returned capacity while its cancelled transaction was still running"
    );
    raw_sql("DROP TRIGGER delay_memory_write ON zuno_enterprise_preview.memory_candidate; DROP FUNCTION zuno_enterprise_preview.delay_memory_write();")
        .execute(admin).await.unwrap();
    assert_eq!(memory.slots.available_permits(), 1);
    let receipts: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.memory_request WHERE tenant_id=$1 AND principal_id=$2 AND request_id='timeout-rollback'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).fetch_one(admin).await.unwrap();
    assert_eq!(receipts, 0);
}

async fn memory_rows(admin: &PgPool, actor: &PrincipalScope) -> Value {
    query_scalar("SELECT jsonb_build_object(
      'documents',(SELECT jsonb_agg(data ORDER BY key) FROM zuno_enterprise_preview.memory_document WHERE tenant_id=$1 AND principal_id=$2),
      'candidates',(SELECT jsonb_agg(data ORDER BY id) FROM zuno_enterprise_preview.memory_candidate WHERE tenant_id=$1 AND principal_id=$2),
      'receipts',(SELECT jsonb_agg(to_jsonb(r) ORDER BY request_id) FROM zuno_enterprise_preview.memory_request r WHERE tenant_id=$1 AND principal_id=$2),
      'audit',(SELECT jsonb_agg(to_jsonb(a) ORDER BY id) FROM zuno_enterprise_preview.memory_audit a WHERE tenant_id=$1 AND principal_id=$2))")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).fetch_one(admin).await.unwrap()
}

async fn model_policy(
    backend: &PostgresBackend,
    admin: &PgPool,
    memory: &PostgresMemoryBackend,
    human: &PostgresMemoryService,
    actor: &PrincipalScope,
    session: &SessionId,
) {
    let runtime = backend.runtime(actor.tenant_id().clone());
    let job = runtime
        .submit(
            actor,
            JobSubmission {
                session_id: session.clone(),
                request_id: RequestId::new("model-memory").unwrap(),
                expected_input_version: 0,
                text: "Remember our conventions".to_owned(),
                configuration: ConfigurationRef {
                    id: ConfigurationId::new("memory-config").unwrap(),
                    version: 1,
                    sha256: "a".repeat(64),
                },
                selection: None,
            },
        )
        .await
        .unwrap();
    let claimed = runtime
        .claim(
            &WorkerInstanceId::new("memory-worker").unwrap(),
            LeaseDuration::new(30_000).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job.id, job.id);
    let model = memory.for_worker(claimed.lease.clone());
    assert!(matches!(
        model
            .request(request("no-consent", change("not authorized")))
            .await,
        Err(Error::Denied)
    ));
    human
        .request(request(
            "consent",
            MemoryCommand::SetPolicy {
                session_id: None,
                expected_revision: 0,
                use_memories: true,
                generate_private: true,
            },
        ))
        .await
        .unwrap();
    let applied = candidate(
        model
            .request(request(
                "model-approved",
                change("keep releases reproducible"),
            ))
            .await
            .unwrap(),
    );
    assert_eq!(applied.status, MemoryCandidateStatus::Applied);
    assert_eq!(applied.source, MemorySource::Tool);
    assert!(
        contents(&model)
            .await
            .contains("keep releases reproducible")
    );
    assert!(matches!(
        model
            .request(request(
                "escalate",
                MemoryCommand::SetPolicy {
                    session_id: None,
                    expected_revision: 1,
                    use_memories: true,
                    generate_private: true,
                }
            ))
            .await,
        Err(Error::Denied)
    ));
    human
        .request(request(
            "disable-generation",
            MemoryCommand::SetPolicy {
                session_id: None,
                expected_revision: 1,
                use_memories: true,
                generate_private: false,
            },
        ))
        .await
        .unwrap();
    assert!(
        contents(&model)
            .await
            .contains("keep releases reproducible"),
        "turning off future generation does not forget existing notes"
    );
    assert!(matches!(
        model
            .request(request("revoked", change("not authorized")))
            .await,
        Err(Error::Denied)
    ));
    human
        .request(request(
            "disable-session-read",
            MemoryCommand::SetPolicy {
                session_id: Some(session.clone()),
                expected_revision: 0,
                use_memories: false,
                generate_private: false,
            },
        ))
        .await
        .unwrap();
    assert!(contents(&model).await.is_empty());
    let mut stale = claimed.lease.clone();
    stale.epoch += 1;
    assert!(matches!(
        memory
            .for_worker(stale)
            .request(request("stale", MemoryCommand::Read))
            .await,
        Err(Error::Conflict)
    ));
    query("UPDATE zuno_enterprise_preview.runtime_session SET lease_expires=0 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(session.as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        model.request(request("expired", MemoryCommand::Read)).await,
        Err(Error::Conflict)
    ));
}
