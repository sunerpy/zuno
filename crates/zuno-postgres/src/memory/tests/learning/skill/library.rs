use super::*;
use zuno_types::activity::Counter;

pub(super) async fn exercise(
    backend: &PostgresBackend,
    admin: &PgPool,
    runtime: &PostgresLearningRuntime,
    actor: &PrincipalScope,
    other: &PrincipalScope,
    candidate: &SkillCandidateView,
    reviewed: &SkillCandidateView,
) {
    let library = backend.skill_library();
    let install = InstallSkill {
        request_id: RequestId::new("install-reviewed").unwrap(),
        expected_digest: candidate.digest.clone(),
        expected_revision: Counter(0),
        description: "Use the recorded proof.".to_owned(),
    };
    assert!(
        library
            .install(actor, &candidate.id, install.clone())
            .await
            .is_err(),
        "evaluation must finish first"
    );
    // Exercise installation transactions with a frozen completed evaluation.
    // The process fixture separately proves these facts originate from the real
    // paired model evaluator, including rejection of forged Worker settlement.
    let report = SkillEvaluationReport::from_cases(
        &candidate.cases,
        vec![SkillCaseResult {
            case_id: candidate.cases[0].id.clone(),
            baseline: SkillCaseObservation {
                score: 10,
                passed: false,
                critical_failure: false,
                details: json!({}),
            },
            candidate: SkillCaseObservation {
                score: 100,
                passed: true,
                critical_failure: false,
                details: json!({}),
            },
        }],
    )
    .unwrap();
    query("UPDATE zuno_enterprise_preview.learning_job SET status='completed',result=$3
        WHERE principal_id=$1 AND id=$2")
        .bind(actor.principal_id().as_str()).bind(reviewed.job_id.as_ref().unwrap().as_str())
        .bind(json!({"completionDigest":zuno_orchestration::sha256_json(&json!(LearningOutput::SkillEvaluation(report.clone())))}))
        .execute(admin).await.unwrap();
    query("UPDATE zuno_enterprise_preview.skill_candidate SET state='passed',report=$3 WHERE principal_id=$1 AND id=$2")
        .bind(actor.principal_id().as_str()).bind(candidate.id.as_str()).bind(json!(report))
        .execute(admin).await.unwrap();
    assert!(
        library
            .install(other, &candidate.id, install.clone())
            .await
            .is_err()
    );
    raw_sql("CREATE FUNCTION public.refuse_skill_installation() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected installation audit failure'; END $$;
        CREATE TRIGGER refuse_skill_installation BEFORE INSERT ON zuno_enterprise_preview.skill_installation_revision
        FOR EACH ROW EXECUTE FUNCTION public.refuse_skill_installation();").execute(admin).await.unwrap();
    assert!(
        library
            .install(actor, &candidate.id, install.clone())
            .await
            .is_err()
    );
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.skill_installation")
            .fetch_one(admin)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.skill_installation_request"
        )
        .fetch_one(admin)
        .await
        .unwrap(),
        0
    );
    raw_sql("DROP TRIGGER refuse_skill_installation ON zuno_enterprise_preview.skill_installation_revision;
        DROP FUNCTION public.refuse_skill_installation();").execute(admin).await.unwrap();
    let installed = library
        .install(actor, &candidate.id, install.clone())
        .await
        .unwrap();
    assert!(!installed.active);
    assert_eq!(installed.revision, Counter(1));
    assert_eq!(
        library
            .install(actor, &candidate.id, install.clone())
            .await
            .unwrap()
            .revision,
        installed.revision
    );
    let mut changed = install.clone();
    changed.description = "changed under old request".to_owned();
    assert!(
        library
            .install(actor, &candidate.id, changed)
            .await
            .is_err()
    );
    assert!(library.document(other, &installed.id).await.is_err());
    assert!(
        library
            .list(
                other,
                &installed.workspace_id,
                None,
                zuno_application::PageSize::default()
            )
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let activation = ActivateSkill {
        request_id: RequestId::new("activate").unwrap(),
        expected_revision: installed.revision,
        active: true,
    };
    let active = library
        .activate(actor, &installed.id, activation.clone())
        .await
        .unwrap();
    assert!(active.active);
    assert_eq!(active.revision, Counter(2));
    assert_ne!(active.source, installed.source);
    assert_eq!(
        library
            .activate(actor, &installed.id, activation)
            .await
            .unwrap()
            .revision,
        active.revision
    );
    assert!(
        library
            .activate(
                actor,
                &installed.id,
                ActivateSkill {
                    request_id: RequestId::new("stale").unwrap(),
                    expected_revision: Counter(1),
                    active: false,
                }
            )
            .await
            .is_err()
    );
    // The installed revision retains the original proof, independently of
    // mutable candidate status/report fields.
    query(
        "UPDATE zuno_enterprise_preview.skill_candidate SET state='failed',report=NULL WHERE id=$1",
    )
    .bind(candidate.id.as_str())
    .execute(admin)
    .await
    .unwrap();
    runtime
        .evaluate(
            actor,
            &candidate.id,
            ReviewSkillEvaluation {
                request_id: RequestId::new("evaluate-after-installation").unwrap(),
                expected_digest: candidate.digest.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        library
            .document(actor, &installed.id)
            .await
            .unwrap()
            .content,
        candidate.proposed_content
    );
    let rollback = library
        .rollback(
            actor,
            &installed.id,
            RollbackSkill {
                request_id: RequestId::new("rollback").unwrap(),
                expected_revision: active.revision,
                target_revision: installed.revision,
            },
        )
        .await
        .unwrap();
    assert!(!rollback.active);
    assert_eq!(rollback.revision, Counter(3));
    let activate = ActivateSkill {
        request_id: RequestId::new("revoked").unwrap(),
        expected_revision: rollback.revision,
        active: true,
    };
    query("UPDATE zuno_enterprise_preview.organization_member SET active=false WHERE tenant_id=$1 AND principal_id=$2")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).execute(admin).await.unwrap();
    assert!(matches!(
        library.activate(actor, &installed.id, activate).await,
        Err(zuno_application::ApplicationError::Forbidden)
    ));
    query("UPDATE zuno_enterprise_preview.organization_member SET active=true WHERE tenant_id=$1 AND principal_id=$2")
        .bind(actor.tenant_id().as_str()).bind(actor.principal_id().as_str()).execute(admin).await.unwrap();
    // Matching the body checksum is insufficient: the original candidate
    // contents and completed evaluation must authorize the same bytes.
    query("UPDATE zuno_enterprise_preview.skill_installation SET content='forged',content_digest=$2 WHERE id=$1")
        .bind(installed.id.as_str()).bind(zuno_orchestration::sha256_text("forged")).execute(admin).await.unwrap();
    assert!(library.document(actor, &installed.id).await.is_err());
    query("UPDATE zuno_enterprise_preview.skill_installation SET content=$2,content_digest=$3 WHERE id=$1")
        .bind(installed.id.as_str()).bind(&candidate.proposed_content)
        .bind(zuno_orchestration::sha256_text(&candidate.proposed_content)).execute(admin).await.unwrap();
    let replacement = runtime
        .propose(
            actor,
            &candidate.source_job_id,
            ProposeSkill {
                request_id: RequestId::new("replacement").unwrap(),
                name: candidate.name.clone(),
                baseline_content: candidate.proposed_content.clone(),
                proposed_content: "replacement body".to_owned(),
                cases: candidate.cases.clone(),
            },
        )
        .await
        .unwrap();
    let replacement_review = runtime
        .evaluate(
            actor,
            &replacement.id,
            ReviewSkillEvaluation {
                request_id: RequestId::new("replacement-review").unwrap(),
                expected_digest: replacement.digest.clone(),
            },
        )
        .await
        .unwrap();
    let proof: Value = query_scalar(
        "SELECT evaluation_report FROM zuno_enterprise_preview.skill_installation WHERE id=$1",
    )
    .bind(installed.id.as_str())
    .fetch_one(admin)
    .await
    .unwrap();
    query("UPDATE zuno_enterprise_preview.learning_job SET status='completed',result=$2 WHERE id=$1")
        .bind(replacement_review.job_id.as_ref().unwrap().as_str())
        .bind(json!({"completionDigest":zuno_orchestration::sha256_json(&json!({"phase":"skill_evaluation","result":proof}))}))
        .execute(admin).await.unwrap();
    query(
        "UPDATE zuno_enterprise_preview.skill_candidate SET state='passed',report=$2 WHERE id=$1",
    )
    .bind(replacement.id.as_str())
    .bind(proof)
    .execute(admin)
    .await
    .unwrap();
    let replace = InstallSkill {
        request_id: RequestId::new("replace").unwrap(),
        expected_digest: replacement.digest,
        expected_revision: rollback.revision,
        description: "replacement".to_owned(),
    };
    let replaced = library
        .install(actor, &replacement.id, replace)
        .await
        .unwrap();
    assert_eq!(replaced.id, installed.id);
    assert!(!replaced.active);
    assert_eq!(
        library
            .document(actor, &installed.id)
            .await
            .unwrap()
            .content,
        "replacement body"
    );
    let restored = library
        .rollback(
            actor,
            &installed.id,
            RollbackSkill {
                request_id: RequestId::new("restore-first-body").unwrap(),
                expected_revision: replaced.revision,
                target_revision: Counter(1),
            },
        )
        .await
        .unwrap();
    assert!(!restored.active);
    assert_eq!(restored.candidate_id, candidate.id);
    assert_eq!(
        library
            .document(actor, &installed.id)
            .await
            .unwrap()
            .content,
        candidate.proposed_content
    );
}
