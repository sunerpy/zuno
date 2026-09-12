use super::*;
use zuno_application::QueueText;
use zuno_db::{
    learning_job::LearningLease,
    memory_candidate::NewMemoryCandidate,
    memory_evidence::MemoryEvidenceReference,
    memory_maintenance::{MemoryBatchChange, MemoryBatchCommit},
};
use zuno_memory::remote::MemoryEvidenceOrigin;

pub(super) async fn exercise(
    backend: &PostgresBackend,
    admin: &PgPool,
    memory: &PostgresMemoryBackend,
) {
    let actor = principal("evidence");
    let workspace = WorkspaceId::new("evidence-workspace").unwrap();
    let session = setup(backend, admin, &actor, &workspace).await;
    let service = memory.for_user(actor.clone(), workspace.clone());
    contents(&service).await;
    service
        .request(request(
            "enable",
            MemoryCommand::SetPolicy {
                session_id: None,
                expected_revision: 0,
                use_memories: true,
                generate_private: true,
            },
        ))
        .await
        .unwrap();
    let application = AgentApplication::new(Arc::new(backend.sessions(actor.clone())));
    let mut references = Vec::new();
    for id in ["first", "second"] {
        let receipt = application
            .queue_text(QueueText {
                request_id: RequestId::new(id).unwrap(),
                session_id: session.clone(),
                text: "Use cargo test for validation".to_owned(),
            })
            .await
            .unwrap();
        let origin = MemoryEvidenceOrigin::UserInput {
            session_id: session.clone(),
            input_id: receipt.id,
        };
        assert!(
            service
                .request(request(
                    &format!("invented-{id}"),
                    MemoryCommand::RecordEvidence {
                        origin: origin.clone(),
                        excerpt: "nonexistent".to_owned()
                    }
                ))
                .await
                .is_err()
        );
        let MemoryReply::Evidence { reference } = service
            .request(request(
                id,
                MemoryCommand::RecordEvidence {
                    origin,
                    excerpt: "cargo test".to_owned(),
                },
            ))
            .await
            .unwrap()
        else {
            panic!("evidence");
        };
        references.push(reference);
    }
    let fixture = BatchFixture {
        backend,
        admin,
        actor: &actor,
        workspace: &workspace,
        session: &session,
    };
    assert!(
        fixture
            .consolidate(
                "expired-batch",
                vec![references[0].clone()],
                "run cargo test",
                false
            )
            .await
            .is_err()
    );
    assert!(!contents(&service).await.contains("run cargo test"));
    for (index, reference) in references.iter().enumerate() {
        fixture
            .consolidate(
                &format!("batch-{index}"),
                vec![reference.clone()],
                "run cargo test",
                true,
            )
            .await
            .unwrap();
    }
    let completed: i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.learning_job WHERE tenant_id='memory' AND principal_id='evidence' AND status='completed'")
        .fetch_one(admin).await.unwrap();
    assert_eq!(completed, 2);
    assert!(contents(&service).await.contains("run cargo test"));
    service
        .request(request(
            "forget-one",
            MemoryCommand::Forget {
                evidence_ids: vec![references[0].experience_id.clone()],
            },
        ))
        .await
        .unwrap();
    assert!(
        contents(&service).await.contains("run cargo test"),
        "independent support preserves the note"
    );
    // Revalidating a changed source suppresses recall before maintenance runs.
    query("UPDATE zuno_enterprise_preview.input SET prompt=jsonb_set(prompt,'{prompt,text}','\"source removed\"')
        WHERE tenant_id=$1 AND principal_id=$2 AND request_key LIKE 'application:%'")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).execute(admin).await.unwrap();
    assert!(!contents(&service).await.contains("run cargo test"));
    let MemoryReply::Forgotten { retracted, .. } = service
        .request(request(
            "forget-two",
            MemoryCommand::Forget {
                evidence_ids: vec![references[1].experience_id.clone()],
            },
        ))
        .await
        .unwrap()
    else {
        panic!("forget");
    };
    assert_eq!(retracted, 1);
    let user = candidate(
        service
            .request(request("manual", change("run cargo test")))
            .await
            .unwrap(),
    );
    service
        .request(request(
            "manual-apply",
            MemoryCommand::Apply {
                candidate_id: user.id.clone(),
                expected_state: user.state_digest,
            },
        ))
        .await
        .unwrap();
    assert!(
        contents(&service).await.contains("run cargo test"),
        "explicit user restoration is independent of withdrawn evidence"
    );
    let before = memory_rows(admin, &actor).await;
    assert!(
        fixture
            .consolidate("forgotten-batch", references, "retracted evidence", true)
            .await
            .is_err()
    );
    assert_eq!(memory_rows(admin, &actor).await, before);
}

struct BatchFixture<'a> {
    backend: &'a PostgresBackend,
    admin: &'a PgPool,
    actor: &'a PrincipalScope,
    workspace: &'a WorkspaceId,
    session: &'a SessionId,
}

impl BatchFixture<'_> {
    /// Exercise the persistence contract with a real queue lease and one owner
    /// transaction. No test-only implementation substitutes for the provider.
    async fn consolidate(
        &self,
        job: &str,
        references: Vec<MemoryEvidenceReference>,
        text: &str,
        live_lease: bool,
    ) -> Result<(), Error> {
        let Self {
            backend,
            admin,
            actor,
            workspace,
            session,
        } = self;
        let mut tx = scoped_transaction(&backend.pool, actor)
            .await
            .map_err(app_error)?;
        let documents: Vec<Value> = query_scalar("SELECT data FROM zuno_enterprise_preview.memory_document WHERE tenant_id=$1 AND principal_id=$2 ORDER BY key")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).fetch_all(&mut *tx).await.map_err(sql_error)?;
        let documents = documents
            .into_iter()
            .map(serde_json::from_value::<zuno_db::resident_memory::ResidentMemoryDocument>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(decode_error)?;
        let global = documents
            .iter()
            .find(|document| document.scope == MemoryScope::Global)
            .unwrap();
        let project = documents
            .iter()
            .find(|document| document.scope == MemoryScope::Project)
            .unwrap();
        let payload = json!({"purpose":"memory","projectPath":project.path,"inputDigest":"b".repeat(64),"globalRevision":global.revision,"projectRevision":project.revision});
        tx.commit().await.map_err(sql_error)?;
        query("INSERT INTO zuno_enterprise_preview.learning_job(tenant_id,principal_id,id,workspace_id,session_id,kind,status,owner_id,lease_token,lease_expires,payload,time_updated)
            VALUES($1,$2,$3,$4,$5,'project_aggregation','running','learning-worker','lease',CASE WHEN $6 THEN 4102444800000 ELSE 0 END,$7,1)")
            .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).bind(job).bind(workspace.as_str()).bind(session.as_str())
            .bind(live_lease).bind(payload).execute(*admin).await.map_err(sql_error)?;
        let tx = scoped_transaction(&backend.pool, actor)
            .await
            .map_err(app_error)?;
        let provider = Arc::new(TransactionMemory {
            transaction: Mutex::new(Some(tx)),
            runtime: Handle::current(),
            principal: (*actor).clone(),
            workspace: (*workspace).clone(),
            lease: None,
            limits: ScopeLimits::default(),
            project_key: project.path.clone(),
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        });
        let mut after = project.entries.clone();
        if !after.iter().any(|entry| entry == text) {
            after.push(text.to_owned());
        }
        let candidate = NewMemoryCandidate {
            id: format!("mem_{job}"),
            target: MemoryScope::Project,
            target_path: project.path.clone(),
            action: MemoryAction::Add,
            content: Some(text.to_owned()),
            old_text: None,
            reason: "verified source".to_owned(),
            confidence: 10_000,
            source: MemorySource::Reflection,
            source_session_id: Some(session.to_string()),
            source_message_id: None,
            fingerprint: None,
            base_revision: Some(project.revision),
            evidence: Some(references.clone()),
            time_created: 1,
        };
        let (global_revision, project_revision, before, job) = (
            global.revision,
            project.revision,
            project.entries.clone(),
            job.to_owned(),
        );
        tokio::task::spawn_blocking(move || {
            provider.commit_maintenance(MemoryBatchCommit {
                project_id: provider.workspace.as_str(),
                global_path: "global",
                project_path: &provider.project_key,
                global_revision,
                project_revision,
                input_digest: &"b".repeat(64),
                job_id: &job,
                lease: &LearningLease {
                    owner_id: "learning-worker".to_owned(),
                    token: "lease".to_owned(),
                },
                evidence: &references,
                changes: vec![MemoryBatchChange {
                    candidate,
                    before,
                    after,
                    apply: true,
                }],
                now: 1,
            })?;
            let tx = provider.transaction.lock().unwrap().take().unwrap();
            provider.runtime.block_on(tx.commit()).map_err(sql_error)
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}
