//! One data owner persists immutable suites and atomic paired-run settlement.
//! Implementations may bind a user/tenant namespace without changing evaluation
//! policy. These data-owner calls are synchronous; hosts with network-backed
//! implementations must run them in bounded blocking capacity, not on a Worker
//! reactor. The model evaluator remains an independent async capability.
use std::sync::Arc;
use zuno_db::evaluation::{
    EvaluationCaseRecord, EvaluationResultRecord, EvaluationRunRecord, EvaluationRunSettlement,
    EvaluationStore, EvaluationSuiteRecord, NewEvaluationRun, NewEvaluationSuite,
};
use zuno_error::DbError;

pub trait EvaluationPersistence: Send + Sync {
    fn ensure_suite(&self, suite: NewEvaluationSuite) -> Result<EvaluationSuiteRecord, DbError>;
    fn suite(&self, id: &str) -> Result<EvaluationSuiteRecord, DbError>;
    fn cases(&self, suite_id: &str) -> Result<Vec<EvaluationCaseRecord>, DbError>;
    fn start_run(&self, run: NewEvaluationRun) -> Result<EvaluationRunRecord, DbError>;
    /// All case results, aggregate metrics and the run terminal state commit
    /// together. Partial results must never make a candidate appear approved.
    fn settle_run(
        &self,
        id: &str,
        settlement: EvaluationRunSettlement<'_>,
    ) -> Result<EvaluationRunRecord, DbError>;
    fn fail_running(&self, id: &str, error: &str, now: i64)
    -> Result<EvaluationRunRecord, DbError>;
    fn run(&self, id: &str) -> Result<EvaluationRunRecord, DbError>;
    fn results(&self, id: &str) -> Result<Vec<EvaluationResultRecord>, DbError>;
    fn reconcile_running(&self, now: i64) -> Result<usize, DbError>;
}

pub struct SqliteEvaluationPersistence {
    store: EvaluationStore,
}
impl SqliteEvaluationPersistence {
    pub fn new(pool: Arc<zuno_db::Pool>) -> Self {
        Self {
            store: EvaluationStore::new(pool),
        }
    }
}
impl EvaluationPersistence for SqliteEvaluationPersistence {
    fn ensure_suite(&self, suite: NewEvaluationSuite) -> Result<EvaluationSuiteRecord, DbError> {
        self.store.ensure_suite(suite)
    }
    fn suite(&self, id: &str) -> Result<EvaluationSuiteRecord, DbError> {
        self.store.suite(id)
    }
    fn cases(&self, id: &str) -> Result<Vec<EvaluationCaseRecord>, DbError> {
        self.store.cases(id)
    }
    fn start_run(&self, run: NewEvaluationRun) -> Result<EvaluationRunRecord, DbError> {
        self.store.start_run(run)
    }
    fn settle_run(
        &self,
        id: &str,
        settlement: EvaluationRunSettlement<'_>,
    ) -> Result<EvaluationRunRecord, DbError> {
        self.store.settle_run(id, settlement)
    }
    fn fail_running(
        &self,
        id: &str,
        error: &str,
        now: i64,
    ) -> Result<EvaluationRunRecord, DbError> {
        self.store.fail_running(id, error, now)
    }
    fn run(&self, id: &str) -> Result<EvaluationRunRecord, DbError> {
        self.store.run(id)
    }
    fn results(&self, id: &str) -> Result<Vec<EvaluationResultRecord>, DbError> {
        self.store.results(id)
    }
    fn reconcile_running(&self, now: i64) -> Result<usize, DbError> {
        self.store.reconcile_running(now)
    }
}
