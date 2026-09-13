use super::*;
use zuno_memory::remote::{
    MemoryCommand, MemoryDataService, MemoryEvidenceOrigin, MemoryReply, MemoryRequest,
};

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool, migrator: &PgPool) {
    let f = fixture(backend, admin, migrator, "shared-evidence").await;
    let space = MemorySpaceId::new("runbooks").unwrap();
    let store = backend.shared_memory();
    let configuration = ConfigureSharedMemory {
        request_id: RequestId::new("create").unwrap(),
        expected_revision: Counter(0),
        title: "Shared evidence".to_owned(),
        workspace_id: WorkspaceId::new("workspace").unwrap(),
        enabled: true,
        character_limit: 3000,
        members: vec![
            SharedMemoryMember {
                principal_id: f.owner.principal_id().clone(),
                role: SharedMemoryRole::Contributor,
            },
            SharedMemoryMember {
                principal_id: f.reviewer.principal_id().clone(),
                role: SharedMemoryRole::Reviewer,
            },
        ],
    };
    store
        .configure(&f.owner, &space, configuration.clone())
        .await
        .unwrap();
    let memory = crate::PostgresMemoryBackend::new(backend.clone(), Default::default()).unwrap();
    let service = memory.for_user(f.owner.clone(), WorkspaceId::new("workspace").unwrap());
    let MemoryReply::Evidence { reference } = service
        .request(MemoryRequest {
            request_id: RequestId::new("record").unwrap(),
            command: MemoryCommand::RecordEvidence {
                origin: MemoryEvidenceOrigin::UserInput {
                    session_id: f.job.session_id.clone(),
                    input_id: f.job.input_id.clone(),
                },
                excerpt: "Investigate the workspace".to_owned(),
            },
        })
        .await
        .unwrap()
    else {
        panic!("evidence");
    };
    let share = ShareMemoryEvidence {
        request_id: RequestId::new("share").unwrap(),
        evidence_id: reference.experience_id.clone(),
        expected_digest: reference.digest.clone(),
    };
    assert!(
        store
            .share(&f.reviewer, &space, share.clone())
            .await
            .is_err(),
        "another user cannot share private evidence"
    );
    let grant = store.share(&f.owner, &space, share.clone()).await.unwrap();
    assert!(grant.current && grant.active);
    assert_eq!(grant.excerpt, "Investigate the workspace");
    assert_eq!(
        store.share(&f.owner, &space, share).await.unwrap().id,
        grant.id
    );
    let mut tx = crate::owner_transaction(&backend.pool, &f.reviewer.owner())
        .await
        .unwrap();
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.memory_evidence WHERE principal_id='alice'").fetch_one(&mut *tx).await.unwrap(),0);
    tx.rollback().await.unwrap();
    assert!(
        SharedEvidenceStore::list(&store, &f.outsider, &space, None, Default::default())
            .await
            .is_err()
    );
    assert_eq!(
        SharedEvidenceStore::list(&store, &f.reviewer, &space, None, Default::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let content = "Follow the reviewed workspace investigation.".to_owned();
    let proposal = ProposeSharedMemory {
        request_id: RequestId::new("proposal").unwrap(),
        expected_revision: Counter(1),
        edits: vec![SharedMemoryEdit::Add {
            content: content.clone(),
        }],
        reason: "Publish a sourced procedure".to_owned(),
        evidence: vec![SharedEvidenceBinding {
            content: content.clone(),
            grants: vec![grant.id.clone()],
        }],
    };
    let change = store.propose(&f.owner, &space, proposal).await.unwrap();
    assert!(change.evidence.is_some());
    let applied = store
        .review(
            &f.reviewer,
            &space,
            ReviewSharedMemory {
                request_id: RequestId::new("apply").unwrap(),
                change_id: change.id,
                expected_state: change.state_digest,
                decision: SharedMemoryDecision::Apply,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .read(&f.reviewer, &space)
            .await
            .unwrap()
            .suppressed
            .is_empty()
    );
    assert!(
        store
            .revoke(
                &f.reviewer,
                &space,
                &grant.id,
                RevokeSharedEvidence {
                    request_id: RequestId::new("wrong-owner").unwrap(),
                    expected_revision: Counter(1)
                }
            )
            .await
            .is_err()
    );
    raw_sql("CREATE FUNCTION public.refuse_shared_evidence_audit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.operation='revoke-evidence' THEN RAISE EXCEPTION 'injected revoke failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_shared_evidence_audit BEFORE INSERT ON zuno_enterprise_preview.shared_memory_evidence_audit
        FOR EACH ROW EXECUTE FUNCTION public.refuse_shared_evidence_audit();").execute(admin).await.unwrap();
    let revoke = RevokeSharedEvidence {
        request_id: RequestId::new("revoke").unwrap(),
        expected_revision: Counter(1),
    };
    assert!(
        store
            .revoke(&f.owner, &space, &grant.id, revoke.clone())
            .await
            .is_err()
    );
    assert!(
        SharedEvidenceStore::list(&store, &f.reviewer, &space, None, Default::default())
            .await
            .unwrap()
            .items[0]
            .active
    );
    raw_sql("DROP TRIGGER refuse_shared_evidence_audit ON zuno_enterprise_preview.shared_memory_evidence_audit;
        DROP FUNCTION public.refuse_shared_evidence_audit();").execute(admin).await.unwrap();
    store
        .revoke(&f.owner, &space, &grant.id, revoke.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .revoke(&f.owner, &space, &grant.id, revoke)
            .await
            .unwrap()
            .revision,
        Counter(2)
    );
    let current = store.read(&f.reviewer, &space).await.unwrap();
    assert_eq!(current.entries, vec![content.clone()]);
    assert_eq!(current.suppressed, vec![content.clone()]);
    let mut tx = crate::owner_transaction(&backend.pool, &f.reviewer.owner())
        .await
        .unwrap();
    let MemoryReply::SharedSnapshot { documents, .. } = crate::shared_memory::snapshots(
        &mut tx,
        &f.reviewer,
        &WorkspaceId::new("workspace").unwrap(),
    )
    .await
    .unwrap() else {
        panic!("snapshot");
    };
    assert!(
        documents.is_empty(),
        "revocation suppresses recall without waiting for maintenance"
    );
    tx.rollback().await.unwrap();
    // Independent support keeps a note alive until all sources are invalid.
    let other = store
        .share(
            &f.owner,
            &space,
            ShareMemoryEvidence {
                request_id: RequestId::new("second-share").unwrap(),
                evidence_id: reference.experience_id.clone(),
                expected_digest: reference.digest.clone(),
            },
        )
        .await
        .unwrap();
    let application = AgentApplication::new(Arc::new(backend.sessions(f.owner.clone())));
    let second_input = application
        .queue_text(zuno_application::QueueText {
            request_id: RequestId::new("independent-input").unwrap(),
            session_id: f.job.session_id.clone(),
            text: "Independently investigate the workspace".to_owned(),
        })
        .await
        .unwrap();
    let MemoryReply::Evidence {
        reference: independent,
    } = service
        .request(MemoryRequest {
            request_id: RequestId::new("record-independent").unwrap(),
            command: MemoryCommand::RecordEvidence {
                origin: MemoryEvidenceOrigin::UserInput {
                    session_id: f.job.session_id.clone(),
                    input_id: second_input.id,
                },
                excerpt: "Independently investigate".to_owned(),
            },
        })
        .await
        .unwrap()
    else {
        panic!("independent evidence");
    };
    let independent_grant = store
        .share(
            &f.owner,
            &space,
            ShareMemoryEvidence {
                request_id: RequestId::new("independent-share").unwrap(),
                evidence_id: independent.experience_id.clone(),
                expected_digest: independent.digest.clone(),
            },
        )
        .await
        .unwrap();
    let replacement = store
        .propose(
            &f.owner,
            &space,
            ProposeSharedMemory {
                request_id: RequestId::new("replace-support").unwrap(),
                expected_revision: current.document_revision,
                edits: vec![SharedMemoryEdit::Add {
                    content: content.clone(),
                }],
                reason: "Use the new explicit sharing grant".to_owned(),
                evidence: vec![SharedEvidenceBinding {
                    content: content.clone(),
                    grants: vec![other.id.clone(), independent_grant.id.clone()],
                }],
            },
        )
        .await
        .unwrap();
    store
        .review(
            &f.reviewer,
            &space,
            ReviewSharedMemory {
                request_id: RequestId::new("review-support").unwrap(),
                change_id: replacement.id,
                expected_state: replacement.state_digest,
                decision: SharedMemoryDecision::Apply,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .read(&f.reviewer, &space)
            .await
            .unwrap()
            .suppressed
            .is_empty()
    );
    service
        .request(MemoryRequest {
            request_id: RequestId::new("forget-independent").unwrap(),
            command: MemoryCommand::Forget {
                evidence_ids: vec![independent.experience_id],
            },
        })
        .await
        .unwrap();
    assert!(
        store
            .read(&f.reviewer, &space)
            .await
            .unwrap()
            .suppressed
            .is_empty(),
        "another current evidence source preserves recall"
    );
    query("UPDATE zuno_enterprise_preview.input SET prompt=jsonb_set(prompt,'{prompt,text}','\"changed\"') WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(f.owner.tenant_id().as_str()).bind(f.owner.principal_id().as_str()).bind(f.job.input_id.as_str()).execute(admin).await.unwrap();
    assert_eq!(
        store.read(&f.reviewer, &space).await.unwrap().suppressed,
        vec![content.clone()]
    );
    let stale = store
        .propose(
            &f.owner,
            &space,
            ProposeSharedMemory {
                request_id: RequestId::new("stale-source").unwrap(),
                expected_revision: Counter(3),
                edits: vec![SharedMemoryEdit::Add {
                    content: content.clone(),
                }],
                reason: "stale".to_owned(),
                evidence: vec![SharedEvidenceBinding {
                    content: content.clone(),
                    grants: vec![other.id.clone()],
                }],
            },
        )
        .await;
    assert!(stale.is_err());
    let manual = store
        .propose(
            &f.owner,
            &space,
            ProposeSharedMemory {
                request_id: RequestId::new("manual-restore").unwrap(),
                expected_revision: Counter(3),
                edits: vec![SharedMemoryEdit::Add {
                    content: content.clone(),
                }],
                reason: "Independently confirm as an organization note".to_owned(),
                evidence: Vec::new(),
            },
        )
        .await
        .unwrap();
    store
        .review(
            &f.reviewer,
            &space,
            ReviewSharedMemory {
                request_id: RequestId::new("manual-review").unwrap(),
                change_id: manual.id,
                expected_state: manual.state_digest,
                decision: SharedMemoryDecision::Apply,
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .read(&f.reviewer, &space)
            .await
            .unwrap()
            .suppressed
            .is_empty(),
        "manual restoration is independent of invalid evidence"
    );
    let mut update = configuration;
    update.request_id = RequestId::new("remove-author").unwrap();
    update.expected_revision = Counter(1);
    update
        .members
        .retain(|m| m.principal_id != *f.owner.principal_id());
    store.configure(&f.owner, &space, update).await.unwrap();
    store
        .revoke(
            &f.owner,
            &space,
            &other.id,
            RevokeSharedEvidence {
                request_id: RequestId::new("revoke-after-leaving").unwrap(),
                expected_revision: Counter(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(applied.state, SharedMemoryChangeState::Applied);
}
