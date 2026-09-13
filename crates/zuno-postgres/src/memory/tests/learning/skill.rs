use super::*;
use zuno_application::skill::*;
mod library;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let actor = principal("skill-owner");
    let other = principal("skill-other");
    let workspace = WorkspaceId::new("skill-workspace").unwrap();
    setup(backend, admin, &actor, &workspace).await;
    setup(backend, admin, &other, &workspace).await;
    let source = source(backend, admin, &actor, &workspace, "skill-source").await;
    let memory = PostgresMemoryBackend::new(backend.clone(), MemoryStoreLimits::default()).unwrap();
    let model = ConfigurationRef {
        id: ConfigurationId::new("skill-model").unwrap(),
        version: 1,
        sha256: "c".repeat(64),
    };
    let runtime = PostgresLearningRuntime::with_skills(
        memory,
        actor.tenant_id().clone(),
        Vec::new(),
        vec![SkillLearningGrant {
            source: configured(),
            evaluation: model.clone(),
            workspace: workspace.clone(),
            maximum_steps: 2,
            limits: LearningExecutionLimits {
                maximum_input_bytes: 32768,
                maximum_output_tokens: 512,
                request_tokens: 40000,
                total_tokens: 240000,
                maximum_attempts: 3,
                duration_ms: 60000,
            },
            model: LearningModelIdentity {
                provider_id: "fixture".to_owned(),
                model_id: "model".to_owned(),
                wire_id: "model".to_owned(),
            },
        }],
    )
    .unwrap();
    let candidate = runtime
        .propose(
            &actor,
            &source,
            ProposeSkill {
                request_id: RequestId::new("skill-create").unwrap(),
                name: "reviewed".to_owned(),
                baseline_content: "baseline".to_owned(),
                proposed_content: "candidate".to_owned(),
                cases: vec![SkillEvaluationCase {
                    id: RequestId::new("case").unwrap(),
                    prompt: "Explain the recorded result".to_owned(),
                    expected: "Use evidence".to_owned(),
                    kind: SkillCaseKind::Failure,
                    weight: 1,
                    calls: Vec::new(),
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(candidate.state, SkillEvaluationState::PendingReview);
    assert!(runtime.candidate(&other, &candidate.id).await.is_err());
    assert!(
        runtime
            .claim(
                WorkerInstanceId::new("no-review").unwrap(),
                vec![model.clone()],
                30000
            )
            .await
            .unwrap()
            .is_none()
    );
    let review = ReviewSkillEvaluation {
        request_id: RequestId::new("review").unwrap(),
        expected_digest: candidate.digest.clone(),
    };
    raw_sql("CREATE FUNCTION public.refuse_skill_execution() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.phase='skill_evaluation' THEN RAISE EXCEPTION 'injected skill dispatch failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER refuse_skill_execution BEFORE INSERT ON zuno_enterprise_preview.learning_execution
        FOR EACH ROW EXECUTE FUNCTION public.refuse_skill_execution();").execute(admin).await.unwrap();
    assert!(
        runtime
            .evaluate(&actor, &candidate.id, review.clone())
            .await
            .is_err()
    );
    assert!(
        runtime
            .candidate(&actor, &candidate.id)
            .await
            .unwrap()
            .job_id
            .is_none()
    );
    assert_eq!(query_scalar::<_,i64>("SELECT count(*) FROM zuno_enterprise_preview.learning_job WHERE principal_id='skill-owner' AND kind='evaluation'")
        .fetch_one(admin).await.unwrap(),0);
    raw_sql("DROP TRIGGER refuse_skill_execution ON zuno_enterprise_preview.learning_execution; DROP FUNCTION public.refuse_skill_execution();")
        .execute(admin).await.unwrap();
    let queued = runtime
        .evaluate(&actor, &candidate.id, review.clone())
        .await
        .unwrap();
    assert_eq!(
        queued.job_id,
        runtime
            .evaluate(&actor, &candidate.id, review)
            .await
            .unwrap()
            .job_id
    );
    let claimed = runtime
        .claim(
            WorkerInstanceId::new("skill-worker").unwrap(),
            vec![model],
            30000,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        claimed.execution.input,
        LearningInput::SkillEvaluation(_)
    ));
    assert!(
        runtime
            .journal(prepared(&claimed, "wrong-purpose"))
            .await
            .is_err()
    );
    let fake = SkillEvaluationReport::from_cases(
        &candidate.cases,
        vec![SkillCaseResult {
            case_id: candidate.cases[0].id.clone(),
            baseline: SkillCaseObservation {
                score: 0,
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
    assert!(
        runtime
            .complete(LearningCompletion {
                lease: claimed.lease.clone(),
                result: LearningOutput::SkillEvaluation(fake)
            })
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .candidate(&actor, &candidate.id)
            .await
            .unwrap()
            .state,
        SkillEvaluationState::Evaluating
    );
    let cancelled = backend
        .cancel_learning_job(
            &actor,
            queued.job_id.as_ref().unwrap(),
            zuno_application::learning_api::CancelLearning {
                request_id: RequestId::new("skill-cancel").unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        cancelled.job.state,
        zuno_application::learning_api::LearningState::Cancelled
    );
    assert_eq!(
        runtime
            .candidate(&actor, &candidate.id)
            .await
            .unwrap()
            .state,
        SkillEvaluationState::Cancelled
    );
    assert!(runtime.renew(claimed.lease, 30000).await.is_err());
    let repeated = runtime
        .evaluate(
            &actor,
            &candidate.id,
            ReviewSkillEvaluation {
                request_id: RequestId::new("explicit-review-again").unwrap(),
                expected_digest: candidate.digest.clone(),
            },
        )
        .await
        .unwrap();
    assert_ne!(
        repeated.job_id, queued.job_id,
        "only a new explicit review creates a new evaluation budget"
    );
    assert_eq!(
        backend
            .client_learning_job(&actor, queued.job_id.as_ref().unwrap())
            .await
            .unwrap()
            .state,
        zuno_application::learning_api::LearningState::Cancelled,
        "old accounting and cancellation remain durable"
    );
    Box::pin(library::exercise(
        backend, admin, &runtime, &actor, &other, &candidate, &repeated,
    ))
    .await;
}
