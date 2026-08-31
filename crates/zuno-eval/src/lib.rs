//! Offline, paired evaluation for reviewed Skill candidates.
//!
//! The evaluator receives a recorded tool cassette, never a live tool registry.
//! Baseline and candidate attempts are constructed from the same
//! [`AttemptSnapshot`], so the Skill content is the only intended variable.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
use zuno_db::evaluation::{
    EvaluationCaseKind, EvaluationCaseRecord, EvaluationResultRecord, EvaluationRunRecord,
    EvaluationRunSettlement, EvaluationRunStatus, EvaluationStore, EvaluationSuiteRecord,
    NewEvaluationResult, NewEvaluationRun, NewEvaluationSuite,
};
use zuno_error::{BoxSource, DbError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSnapshot {
    pub model: String,
    pub toolset_digest: String,
    pub max_output_tokens: u32,
    pub max_steps: u32,
    pub temperature_millis: u16,
    pub seed: u64,
}

impl AttemptSnapshot {
    fn validate(&self) -> Result<(), EvaluationError> {
        if self.model.trim().is_empty()
            || self.toolset_digest.trim().is_empty()
            || self.max_output_tokens == 0
            || self.max_steps == 0
        {
            return Err(EvaluationError::InvalidSnapshot);
        }
        Ok(())
    }

    fn budget_json(&self) -> Value {
        json!({
            "maxOutputTokens": self.max_output_tokens,
            "maxSteps": self.max_steps,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OfflineCaseRequest {
    pub case_id: String,
    pub skill_content: String,
    pub prompt: String,
    pub expected: String,
    pub tool_cassette: Value,
    pub attempt: AttemptSnapshot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaseObservation {
    pub score: i64,
    pub passed: bool,
    pub critical_failure: bool,
    pub details: Value,
}

#[async_trait]
pub trait OfflineCaseEvaluator: Send + Sync {
    /// Evaluate against recorded responses. Implementations are deliberately not
    /// given a live tool executor or network capability.
    async fn evaluate(&self, request: OfflineCaseRequest) -> Result<CaseObservation, BoxSource>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationDecision {
    pub run_id: String,
    pub passed: bool,
    pub baseline_metric: i64,
    pub candidate_metric: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum EvaluationError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("evaluation attempt snapshot is incomplete")]
    InvalidSnapshot,
    #[error("evaluation suite `{suite_id}` has no cases")]
    EmptySuite { suite_id: String },
    #[error("offline evaluator failed for case `{case_id}`")]
    Evaluator {
        case_id: String,
        #[source]
        source: BoxSource,
    },
}

#[derive(Clone)]
pub struct EvaluationService {
    store: EvaluationStore,
}

impl EvaluationService {
    #[must_use]
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self {
            store: EvaluationStore::new(pool),
        }
    }

    pub async fn evaluate_candidate(
        &self,
        request: CandidateEvaluationRequest,
        evaluator: &dyn OfflineCaseEvaluator,
    ) -> Result<EvaluationDecision, EvaluationError> {
        request.attempt.validate()?;
        let cases = self.store.cases(&request.suite_id)?;
        if cases.is_empty() {
            return Err(EvaluationError::EmptySuite {
                suite_id: request.suite_id,
            });
        }
        let run_id = format!("eval_{}", Uuid::now_v7().simple());
        let snapshot_json =
            serde_json::to_value(&request.attempt).expect("AttemptSnapshot is serializable");
        self.store.start_run(NewEvaluationRun {
            id: run_id.clone(),
            suite_id: request.suite_id,
            candidate_id: request.candidate_id,
            model: request.attempt.model.clone(),
            toolset_digest: request.attempt.toolset_digest.clone(),
            budget: request.attempt.budget_json(),
            attempt_snapshot: snapshot_json,
            time_created: request.time_created,
        })?;

        let mut baseline_metric = 0_i64;
        let mut candidate_metric = 0_i64;
        let mut results = Vec::with_capacity(cases.len());
        let mut cited_failures_fixed = true;
        let mut protection_regressed = false;
        for case in cases {
            let baseline =
                match evaluate_one(evaluator, &case, &request.baseline_skill, &request.attempt)
                    .await
                {
                    Ok(observation) => observation,
                    Err(error) => {
                        let _ = self.store.fail_running(
                            &run_id,
                            &error.to_string(),
                            request.time_completed,
                        );
                        return Err(error);
                    }
                };
            let candidate =
                match evaluate_one(evaluator, &case, &request.candidate_skill, &request.attempt)
                    .await
                {
                    Ok(observation) => observation,
                    Err(error) => {
                        let _ = self.store.fail_running(
                            &run_id,
                            &error.to_string(),
                            request.time_completed,
                        );
                        return Err(error);
                    }
                };
            let weight = i64::from(case.weight);
            baseline_metric = baseline_metric.saturating_add(baseline.score.saturating_mul(weight));
            candidate_metric =
                candidate_metric.saturating_add(candidate.score.saturating_mul(weight));
            let cited_failure_fixed = case.kind != EvaluationCaseKind::Failure || candidate.passed;
            let critical_regression = case.kind == EvaluationCaseKind::Protection
                && baseline.passed
                && (!candidate.passed
                    || candidate.critical_failure
                    || candidate.score < baseline.score);
            cited_failures_fixed &= cited_failure_fixed;
            protection_regressed |= critical_regression;
            results.push(NewEvaluationResult {
                id: format!("evr_{}", Uuid::now_v7().simple()),
                case_id: case.id,
                baseline_score: baseline.score,
                candidate_score: candidate.score,
                cited_failure_fixed,
                critical_regression,
                details: json!({
                    "baseline": baseline.details,
                    "candidate": candidate.details,
                    "baselinePassed": baseline.passed,
                    "candidatePassed": candidate.passed,
                }),
            });
        }
        let passed =
            cited_failures_fixed && !protection_regressed && candidate_metric >= baseline_metric;
        let status = if passed {
            EvaluationRunStatus::Passed
        } else {
            EvaluationRunStatus::Failed
        };
        self.store.settle_run(
            &run_id,
            EvaluationRunSettlement {
                status,
                baseline_metric,
                candidate_metric,
                results: &results,
                error: (!passed).then_some("candidate did not satisfy the evaluation policy"),
                time_completed: request.time_completed,
            },
        )?;
        Ok(EvaluationDecision {
            run_id,
            passed,
            baseline_metric,
            candidate_metric,
        })
    }

    pub fn run(&self, id: &str) -> Result<EvaluationRunRecord, DbError> {
        self.store.run(id)
    }

    pub fn ensure_suite(
        &self,
        suite: NewEvaluationSuite,
    ) -> Result<EvaluationSuiteRecord, DbError> {
        self.store.ensure_suite(suite)
    }

    pub fn suite(&self, id: &str) -> Result<EvaluationSuiteRecord, DbError> {
        self.store.suite(id)
    }

    pub fn cases(&self, suite_id: &str) -> Result<Vec<EvaluationCaseRecord>, DbError> {
        self.store.cases(suite_id)
    }

    pub fn results(&self, run_id: &str) -> Result<Vec<EvaluationResultRecord>, DbError> {
        self.store.results(run_id)
    }

    pub fn reconcile_running(&self, now: i64) -> Result<usize, DbError> {
        self.store.reconcile_running(now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateEvaluationRequest {
    pub suite_id: String,
    pub candidate_id: String,
    pub baseline_skill: String,
    pub candidate_skill: String,
    pub attempt: AttemptSnapshot,
    pub time_created: i64,
    pub time_completed: i64,
}

async fn evaluate_one(
    evaluator: &dyn OfflineCaseEvaluator,
    case: &EvaluationCaseRecord,
    skill_content: &str,
    attempt: &AttemptSnapshot,
) -> Result<CaseObservation, EvaluationError> {
    evaluator
        .evaluate(OfflineCaseRequest {
            case_id: case.id.clone(),
            skill_content: skill_content.to_owned(),
            prompt: case.prompt.clone(),
            expected: case.expected.clone(),
            tool_cassette: case.tool_cassette.clone(),
            attempt: attempt.clone(),
        })
        .await
        .map_err(|source| EvaluationError::Evaluator {
            case_id: case.id.clone(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zuno_db::evaluation::{NewEvaluationCase, NewEvaluationSuite};
    use zuno_db::migration;
    use zuno_paths::DbLocation;

    struct FakeEvaluator {
        requests: Mutex<Vec<OfflineCaseRequest>>,
    }

    #[async_trait]
    impl OfflineCaseEvaluator for FakeEvaluator {
        async fn evaluate(
            &self,
            request: OfflineCaseRequest,
        ) -> Result<CaseObservation, BoxSource> {
            let candidate = request.skill_content.contains("candidate");
            self.requests
                .lock()
                .expect("request lock")
                .push(request.clone());
            Ok(CaseObservation {
                score: if candidate { 10 } else { 5 },
                passed: candidate,
                critical_failure: false,
                details: json!({"cassette": request.tool_cassette}),
            })
        }
    }

    #[tokio::test]
    async fn baseline_and_candidate_share_one_snapshot_and_only_receive_cassettes() {
        let pool = Arc::new(zuno_db::Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]');",
                )
                .expect("project");
        }
        let store = EvaluationStore::new(pool.clone());
        store
            .create_suite(NewEvaluationSuite {
                id: "suite-1".to_owned(),
                project_id: "project-1".to_owned(),
                name: "regression".to_owned(),
                description: String::new(),
                cases: vec![NewEvaluationCase {
                    id: "case-1".to_owned(),
                    name: "failure".to_owned(),
                    prompt: "prompt".to_owned(),
                    expected: "expected".to_owned(),
                    tool_cassette: json!({"recorded": true}),
                    kind: EvaluationCaseKind::Failure,
                    weight: 1,
                }],
                time_created: 1,
            })
            .expect("suite");
        let evaluator = FakeEvaluator {
            requests: Mutex::new(Vec::new()),
        };
        let attempt = AttemptSnapshot {
            model: "provider/model".to_owned(),
            toolset_digest: "tools".to_owned(),
            max_output_tokens: 1_000,
            max_steps: 4,
            temperature_millis: 0,
            seed: 7,
        };
        let decision = EvaluationService::new(pool)
            .evaluate_candidate(
                CandidateEvaluationRequest {
                    suite_id: "suite-1".to_owned(),
                    candidate_id: "candidate-1".to_owned(),
                    baseline_skill: "baseline".to_owned(),
                    candidate_skill: "candidate".to_owned(),
                    attempt: attempt.clone(),
                    time_created: 2,
                    time_completed: 3,
                },
                &evaluator,
            )
            .await
            .expect("evaluation");
        assert!(decision.passed);
        let requests = evaluator.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].attempt, attempt);
        assert_eq!(requests[1].attempt, attempt);
        assert_eq!(requests[0].tool_cassette, json!({"recorded": true}));
        assert_eq!(requests[1].tool_cassette, json!({"recorded": true}));
    }
}
