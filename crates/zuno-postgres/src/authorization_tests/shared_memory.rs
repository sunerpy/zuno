use super::*;
use sqlx_core::raw_sql::raw_sql;
use zuno_application::shared_memory::*;
use zuno_types::{activity::Counter, identity::MemorySpaceId};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "shared-memory").await;
    let store = backend.shared_memory();
    let id = MemorySpaceId::new("runbooks").unwrap();
    let configuration = ConfigureSharedMemory {
        request_id: RequestId::new("configure").unwrap(),
        expected_revision: Counter(0),
        title: "Runbooks".to_owned(),
        workspace_id: WorkspaceId::new("workspace").unwrap(),
        enabled: true,
        character_limit: 3000,
        members: vec![
            SharedMemoryMember {
                principal_id: f.owner.principal_id().clone(),
                role: SharedMemoryRole::Reviewer,
            },
            SharedMemoryMember {
                principal_id: f.reviewer.principal_id().clone(),
                role: SharedMemoryRole::Reviewer,
            },
        ],
    };
    assert!(
        store
            .configure(&f.reviewer, &id, configuration.clone())
            .await
            .is_err()
    );
    let space = store
        .configure(&f.owner, &id, configuration.clone())
        .await
        .unwrap();
    assert_eq!(space.document_revision, Counter(1));
    assert!(store.read(&f.outsider, &id).await.is_err());
    let mut tx = crate::owner_transaction(&backend.pool, &f.outsider.owner())
        .await
        .unwrap();
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.shared_memory_space")
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
        0,
        "RLS must hide other namespaces even before API projection"
    );
    tx.rollback().await.unwrap();
    let proposal = ProposeSharedMemory {
        request_id: RequestId::new("proposal").unwrap(),
        expected_revision: Counter(1),
        edits: vec![SharedMemoryEdit::Add {
            content: "Use the approved deployment runbook.".to_owned(),
        }],
        reason: "Document the reviewed procedure.".to_owned(),
    };
    let change = store
        .propose(&f.owner, &id, proposal.clone())
        .await
        .unwrap();
    assert_eq!(
        store.propose(&f.owner, &id, proposal).await.unwrap().id,
        change.id
    );
    assert!(
        store
            .read(&f.reviewer, &id)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    let review = ReviewSharedMemory {
        request_id: RequestId::new("apply").unwrap(),
        change_id: change.id.clone(),
        expected_state: change.state_digest.clone(),
        decision: SharedMemoryDecision::Apply,
    };
    assert!(
        store.review(&f.owner, &id, review.clone()).await.is_err(),
        "the author cannot approve their own change"
    );
    raw_sql("CREATE FUNCTION public.refuse_shared_revision() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.tenant_id='shared-memory' AND NEW.revision=2 THEN RAISE EXCEPTION 'injected shared revision failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_shared_revision BEFORE INSERT ON zuno_enterprise_preview.shared_memory_revision
        FOR EACH ROW EXECUTE FUNCTION public.refuse_shared_revision();").execute(admin).await.unwrap();
    assert!(
        store
            .review(&f.reviewer, &id, review.clone())
            .await
            .is_err()
    );
    assert!(store.read(&f.owner, &id).await.unwrap().entries.is_empty());
    assert_eq!(
        store.change(&f.owner, &id, &change.id).await.unwrap().state,
        SharedMemoryChangeState::Pending
    );
    raw_sql("DROP TRIGGER refuse_shared_revision ON zuno_enterprise_preview.shared_memory_revision; DROP FUNCTION public.refuse_shared_revision();")
        .execute(admin).await.unwrap();
    let applied = store
        .review(&f.reviewer, &id, review.clone())
        .await
        .unwrap();
    assert_eq!(applied.state, SharedMemoryChangeState::Applied);
    store.review(&f.reviewer, &id, review).await.unwrap();
    assert_eq!(
        store.read(&f.owner, &id).await.unwrap().document_revision,
        Counter(2)
    );
    let undone = store
        .review(
            &f.reviewer,
            &id,
            ReviewSharedMemory {
                request_id: RequestId::new("undo").unwrap(),
                change_id: applied.id,
                expected_state: applied.state_digest,
                decision: SharedMemoryDecision::Undo,
            },
        )
        .await
        .unwrap();
    assert_eq!(undone.state, SharedMemoryChangeState::Undone);
    assert!(store.read(&f.owner, &id).await.unwrap().entries.is_empty());
    let pending = store
        .propose(
            &f.reviewer,
            &id,
            ProposeSharedMemory {
                request_id: RequestId::new("pending-before-policy-change").unwrap(),
                expected_revision: Counter(3),
                edits: vec![SharedMemoryEdit::Add {
                    content: "Await independent review.".to_owned(),
                }],
                reason: "Exercise changed approval policy.".to_owned(),
            },
        )
        .await
        .unwrap();
    let mut update = configuration;
    update.request_id = RequestId::new("revoke-reviewer").unwrap();
    update.expected_revision = Counter(1);
    update
        .members
        .retain(|member| member.principal_id != *f.reviewer.principal_id());
    store.configure(&f.owner, &id, update).await.unwrap();
    assert_eq!(
        store
            .change(&f.owner, &id, &pending.id)
            .await
            .unwrap()
            .state,
        SharedMemoryChangeState::Invalidated
    );
    assert!(matches!(
        store
            .review(
                &f.owner,
                &id,
                ReviewSharedMemory {
                    request_id: RequestId::new("invalidated-review").unwrap(),
                    change_id: pending.id,
                    expected_state: pending.state_digest,
                    decision: SharedMemoryDecision::Apply,
                }
            )
            .await,
        Err(ApplicationError::Conflict)
    ));
    assert!(store.read(&f.reviewer, &id).await.is_err());
    assert!(
        store
            .review(
                &f.reviewer,
                &id,
                ReviewSharedMemory {
                    request_id: RequestId::new("old-review").unwrap(),
                    change_id: change.id,
                    expected_state: change.state_digest,
                    decision: SharedMemoryDecision::Apply,
                }
            )
            .await
            .is_err()
    );
}
