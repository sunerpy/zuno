//! Durable offline evaluation suites, runs, and per-case results.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use serde_json::Value;
use std::sync::Arc;
use zuno_error::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationCaseKind {
    Failure,
    Protection,
    General,
}

impl EvaluationCaseKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::Protection => "protection",
            Self::General => "general",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "failure" => Ok(Self::Failure),
            "protection" => Ok(Self::Protection),
            "general" => Ok(Self::General),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown evaluation case kind `{value}`"
            )))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvaluationCase {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub expected: String,
    pub tool_cassette: Value,
    pub kind: EvaluationCaseKind,
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvaluationSuite {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub description: String,
    pub cases: Vec<NewEvaluationCase>,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationCaseRecord {
    pub id: String,
    pub suite_id: String,
    pub name: String,
    pub prompt: String,
    pub expected: String,
    pub tool_cassette: Value,
    pub kind: EvaluationCaseKind,
    pub weight: u32,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationSuiteRecord {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub description: String,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationRunStatus {
    Running,
    Passed,
    Failed,
    Uncertain,
}

impl EvaluationRunStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "running" => Ok(Self::Running),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "uncertain" => Ok(Self::Uncertain),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown evaluation run status `{value}`"
            )))),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvaluationRun {
    pub id: String,
    pub suite_id: String,
    pub candidate_id: String,
    pub model: String,
    pub toolset_digest: String,
    pub budget: Value,
    pub attempt_snapshot: Value,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationRunRecord {
    pub id: String,
    pub suite_id: String,
    pub candidate_id: String,
    pub model: String,
    pub toolset_digest: String,
    pub budget: Value,
    pub attempt_snapshot: Value,
    pub status: EvaluationRunStatus,
    pub baseline_metric: Option<i64>,
    pub candidate_metric: Option<i64>,
    pub error: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_completed: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvaluationResult {
    pub id: String,
    pub case_id: String,
    pub baseline_score: i64,
    pub candidate_score: i64,
    pub cited_failure_fixed: bool,
    pub critical_regression: bool,
    pub details: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct EvaluationRunSettlement<'a> {
    pub status: EvaluationRunStatus,
    pub baseline_metric: i64,
    pub candidate_metric: i64,
    pub results: &'a [NewEvaluationResult],
    pub error: Option<&'a str>,
    pub time_completed: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationResultRecord {
    pub id: String,
    pub run_id: String,
    pub case_id: String,
    pub baseline_score: i64,
    pub candidate_score: i64,
    pub cited_failure_fixed: bool,
    pub critical_regression: bool,
    pub details: Value,
    pub time_created: i64,
}

#[derive(Clone)]
pub struct EvaluationStore {
    pool: Arc<Pool>,
}

impl EvaluationStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn create_suite(
        &self,
        suite: NewEvaluationSuite,
    ) -> Result<EvaluationSuiteRecord, DbError> {
        validate_suite(&suite)?;
        self.pool.transaction(|transaction| {
            transaction
                .execute(
                    "INSERT INTO evaluation_suite
                       (id, project_id, name, description, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    params![
                        suite.id,
                        suite.project_id,
                        suite.name,
                        suite.description,
                        suite.time_created
                    ],
                )
                .map_err(open::map_error)?;
            for case in &suite.cases {
                let cassette = serde_json::to_string(&case.tool_cassette).map_err(query_error)?;
                transaction
                    .execute(
                        "INSERT INTO evaluation_case (
                           id, suite_id, name, prompt, expected, tool_cassette, case_kind,
                           weight, time_created, time_updated
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                        params![
                            case.id,
                            suite.id,
                            case.name,
                            case.prompt,
                            case.expected,
                            cassette,
                            case.kind.as_str(),
                            i64::from(case.weight),
                            suite.time_created,
                        ],
                    )
                    .map_err(open::map_error)?;
            }
            read_suite(transaction, &suite.id)
        })
    }

    /// Create a deterministic suite once, or verify that a retry describes the
    /// exact suite already stored under that identity.
    pub fn ensure_suite(
        &self,
        suite: NewEvaluationSuite,
    ) -> Result<EvaluationSuiteRecord, DbError> {
        validate_suite(&suite)?;
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "INSERT INTO evaluation_suite
                       (id, project_id, name, description, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                     ON CONFLICT(id) DO NOTHING",
                    params![
                        suite.id,
                        suite.project_id,
                        suite.name,
                        suite.description,
                        suite.time_created
                    ],
                )
                .map_err(open::map_error)?;
            if changed == 1 {
                for case in &suite.cases {
                    insert_case(transaction, &suite.id, case, suite.time_created)?;
                }
            } else {
                let stored = read_suite(transaction, &suite.id)?;
                let stored_cases = read_cases(transaction, &suite.id)?;
                if stored.project_id != suite.project_id
                    || stored.name != suite.name
                    || stored.description != suite.description
                    || !same_cases(&stored_cases, &suite.cases)
                {
                    return Err(DbError::Conflict {
                        table: "evaluation_suite".to_owned(),
                        id: suite.id.clone(),
                        detail: "retry payload differs from the stored offline suite".to_owned(),
                    });
                }
            }
            read_suite(transaction, &suite.id)
        })
    }

    pub fn suite(&self, id: &str) -> Result<EvaluationSuiteRecord, DbError> {
        let connection = self.pool.get()?;
        read_suite(&connection, id)
    }

    pub fn cases(&self, suite_id: &str) -> Result<Vec<EvaluationCaseRecord>, DbError> {
        let connection = self.pool.get()?;
        read_cases(&connection, suite_id)
    }

    pub fn start_run(&self, run: NewEvaluationRun) -> Result<EvaluationRunRecord, DbError> {
        validate_run(&run)?;
        let budget = serde_json::to_string(&run.budget).map_err(query_error)?;
        let snapshot = serde_json::to_string(&run.attempt_snapshot).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            transaction
                .execute(
                    "INSERT INTO evaluation_run (
                       id, suite_id, candidate_id, model, toolset_digest, budget,
                       attempt_snapshot, status, time_created, time_updated
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running', ?8, ?8)",
                    params![
                        run.id,
                        run.suite_id,
                        run.candidate_id,
                        run.model,
                        run.toolset_digest,
                        budget,
                        snapshot,
                        run.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            read_run(transaction, &run.id)
        })
    }

    pub fn settle_run(
        &self,
        run_id: &str,
        settlement: EvaluationRunSettlement<'_>,
    ) -> Result<EvaluationRunRecord, DbError> {
        let EvaluationRunSettlement {
            status,
            baseline_metric,
            candidate_metric,
            results,
            error,
            time_completed,
        } = settlement;
        if status == EvaluationRunStatus::Running {
            return Err(query_error(std::io::Error::other(
                "evaluation settlement must be terminal",
            )));
        }
        self.pool.transaction(|transaction| {
            let run = read_run(transaction, run_id)?;
            if run.status != EvaluationRunStatus::Running {
                return Err(query_error(std::io::Error::other(format!(
                    "evaluation run `{run_id}` is not running"
                ))));
            }
            let case_ids: std::collections::HashSet<String> =
                read_cases(transaction, &run.suite_id)?
                    .into_iter()
                    .map(|case| case.id)
                    .collect();
            if results.len() != case_ids.len()
                || results
                    .iter()
                    .any(|result| !case_ids.contains(&result.case_id))
            {
                return Err(query_error(std::io::Error::other(
                    "evaluation settlement must contain exactly one result for every suite case",
                )));
            }
            for result in results {
                let details = serde_json::to_string(&result.details).map_err(query_error)?;
                transaction
                    .execute(
                        "INSERT INTO evaluation_result (
                           id, run_id, case_id, baseline_score, candidate_score,
                           cited_failure_fixed, critical_regression, details, time_created
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            result.id,
                            run_id,
                            result.case_id,
                            result.baseline_score,
                            result.candidate_score,
                            result.cited_failure_fixed,
                            result.critical_regression,
                            details,
                            time_completed,
                        ],
                    )
                    .map_err(open::map_error)?;
            }
            transaction
                .execute(
                    "UPDATE evaluation_run
                     SET status = ?2, baseline_metric = ?3, candidate_metric = ?4,
                         error = ?5, time_updated = ?6, time_completed = ?6
                     WHERE id = ?1 AND status = 'running'",
                    params![
                        run_id,
                        status.as_str(),
                        baseline_metric,
                        candidate_metric,
                        error,
                        time_completed
                    ],
                )
                .map_err(open::map_error)?;
            read_run(transaction, run_id)
        })
    }

    pub fn fail_running(
        &self,
        run_id: &str,
        error: &str,
        now: i64,
    ) -> Result<EvaluationRunRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE evaluation_run
                     SET status = 'uncertain', error = ?2, time_updated = ?3, time_completed = ?3
                     WHERE id = ?1 AND status = 'running'",
                    params![run_id, error, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(DbError::Conflict {
                    table: "evaluation_run".to_owned(),
                    id: run_id.to_owned(),
                    detail: "run is no longer running".to_owned(),
                });
            }
            read_run(transaction, run_id)
        })
    }

    pub fn reconcile_running(&self, now: i64) -> Result<usize, DbError> {
        let connection = self.pool.get()?;
        connection
            .execute(
                "UPDATE evaluation_run
                 SET status = 'uncertain',
                     error = 'evaluation process stopped before settlement',
                     time_updated = ?1, time_completed = ?1
                 WHERE status = 'running'",
                [now],
            )
            .map_err(open::map_error)
    }

    pub fn run(&self, id: &str) -> Result<EvaluationRunRecord, DbError> {
        let connection = self.pool.get()?;
        read_run(&connection, id)
    }

    pub fn results(&self, run_id: &str) -> Result<Vec<EvaluationResultRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(
                "SELECT id, run_id, case_id, baseline_score, candidate_score,
                        cited_failure_fixed, critical_regression, details, time_created
                 FROM evaluation_result WHERE run_id = ?1 ORDER BY case_id",
            )
            .map_err(open::map_error)?;
        statement
            .query_map([run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, bool>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(open::map_error)?
            .map(|row| {
                let row = row.map_err(open::map_error)?;
                Ok(EvaluationResultRecord {
                    id: row.0,
                    run_id: row.1,
                    case_id: row.2,
                    baseline_score: row.3,
                    candidate_score: row.4,
                    cited_failure_fixed: row.5,
                    critical_regression: row.6,
                    details: serde_json::from_str(&row.7).map_err(query_error)?,
                    time_created: row.8,
                })
            })
            .collect()
    }
}

fn insert_case(
    transaction: &rusqlite::Transaction<'_>,
    suite_id: &str,
    case: &NewEvaluationCase,
    now: i64,
) -> Result<(), DbError> {
    let cassette = serde_json::to_string(&case.tool_cassette).map_err(query_error)?;
    transaction
        .execute(
            "INSERT INTO evaluation_case (
               id, suite_id, name, prompt, expected, tool_cassette, case_kind,
               weight, time_created, time_updated
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                case.id,
                suite_id,
                case.name,
                case.prompt,
                case.expected,
                cassette,
                case.kind.as_str(),
                i64::from(case.weight),
                now,
            ],
        )
        .map_err(open::map_error)?;
    Ok(())
}

fn same_cases(stored: &[EvaluationCaseRecord], requested: &[NewEvaluationCase]) -> bool {
    if stored.len() != requested.len() {
        return false;
    }
    let requested = requested
        .iter()
        .map(|case| {
            (
                case.id.clone(),
                case.name.clone(),
                case.prompt.clone(),
                case.expected.clone(),
                serde_json::to_string(&case.tool_cassette)
                    .expect("serde_json::Value has a total Serialize implementation"),
                case.kind.as_str().to_owned(),
                case.weight,
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    stored.iter().all(|case| {
        requested.contains(&(
            case.id.clone(),
            case.name.clone(),
            case.prompt.clone(),
            case.expected.clone(),
            serde_json::to_string(&case.tool_cassette)
                .expect("serde_json::Value has a total Serialize implementation"),
            case.kind.as_str().to_owned(),
            case.weight,
        ))
    })
}

fn validate_suite(suite: &NewEvaluationSuite) -> Result<(), DbError> {
    if suite.id.trim().is_empty()
        || suite.project_id.trim().is_empty()
        || suite.name.trim().is_empty()
        || suite.cases.is_empty()
    {
        return Err(query_error(std::io::Error::other(
            "evaluation suite identity, project, name, and cases are required",
        )));
    }
    let mut names = std::collections::HashSet::new();
    for case in &suite.cases {
        if case.id.trim().is_empty()
            || case.name.trim().is_empty()
            || case.prompt.trim().is_empty()
            || case.expected.trim().is_empty()
            || case.weight == 0
            || !names.insert(case.name.as_str())
        {
            return Err(query_error(std::io::Error::other(
                "evaluation cases require unique names, prompts, expectations, and positive weights",
            )));
        }
    }
    Ok(())
}

fn validate_run(run: &NewEvaluationRun) -> Result<(), DbError> {
    if run.id.trim().is_empty()
        || run.suite_id.trim().is_empty()
        || run.candidate_id.trim().is_empty()
        || run.model.trim().is_empty()
        || run.toolset_digest.trim().is_empty()
    {
        return Err(query_error(std::io::Error::other(
            "evaluation run identity, candidate, model, and toolset digest are required",
        )));
    }
    Ok(())
}

fn read_suite(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<EvaluationSuiteRecord, DbError> {
    connection
        .query_row(
            "SELECT id, project_id, name, description, time_created, time_updated
             FROM evaluation_suite WHERE id = ?1",
            [id],
            |row| {
                Ok(EvaluationSuiteRecord {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    time_created: row.get(4)?,
                    time_updated: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "evaluation_suite".to_owned(),
            id: id.to_owned(),
        })
}

fn read_cases(
    connection: &rusqlite::Connection,
    suite_id: &str,
) -> Result<Vec<EvaluationCaseRecord>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id, suite_id, name, prompt, expected, tool_cassette, case_kind,
                    weight, time_created, time_updated
             FROM evaluation_case WHERE suite_id = ?1 ORDER BY name, id",
        )
        .map_err(open::map_error)?;
    statement
        .query_map([suite_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })
        .map_err(open::map_error)?
        .map(|row| {
            let row = row.map_err(open::map_error)?;
            Ok(EvaluationCaseRecord {
                id: row.0,
                suite_id: row.1,
                name: row.2,
                prompt: row.3,
                expected: row.4,
                tool_cassette: serde_json::from_str(&row.5).map_err(query_error)?,
                kind: EvaluationCaseKind::parse(&row.6)?,
                weight: u32::try_from(row.7).map_err(query_error)?,
                time_created: row.8,
                time_updated: row.9,
            })
        })
        .collect()
}

type StoredRun = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
    i64,
    i64,
    Option<i64>,
);

fn read_run(connection: &rusqlite::Connection, id: &str) -> Result<EvaluationRunRecord, DbError> {
    connection
        .query_row(
            "SELECT id, suite_id, candidate_id, model, toolset_digest, budget,
                    attempt_snapshot, status, baseline_metric, candidate_metric, error,
                    time_created, time_updated, time_completed
             FROM evaluation_run WHERE id = ?1",
            [id],
            decode_run_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "evaluation_run".to_owned(),
            id: id.to_owned(),
        })
        .and_then(decode_run)
}

fn decode_run_row(row: &Row<'_>) -> rusqlite::Result<StoredRun> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
    ))
}

fn decode_run(row: StoredRun) -> Result<EvaluationRunRecord, DbError> {
    Ok(EvaluationRunRecord {
        id: row.0,
        suite_id: row.1,
        candidate_id: row.2,
        model: row.3,
        toolset_digest: row.4,
        budget: serde_json::from_str(&row.5).map_err(query_error)?,
        attempt_snapshot: serde_json::from_str(&row.6).map_err(query_error)?,
        status: EvaluationRunStatus::parse(&row.7)?,
        baseline_metric: row.8,
        candidate_metric: row.9,
        error: row.10,
        time_created: row.11,
        time_updated: row.12,
        time_completed: row.13,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration;
    use serde_json::json;
    use zuno_paths::DbLocation;

    #[test]
    fn suite_and_run_preserve_offline_cassettes_and_attempt_snapshot() {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
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
        let store = EvaluationStore::new(pool);
        store
            .create_suite(NewEvaluationSuite {
                id: "suite-1".to_owned(),
                project_id: "project-1".to_owned(),
                name: "skill regression".to_owned(),
                description: "offline only".to_owned(),
                cases: vec![NewEvaluationCase {
                    id: "case-1".to_owned(),
                    name: "failed trace".to_owned(),
                    prompt: "diagnose".to_owned(),
                    expected: "use durable evidence".to_owned(),
                    tool_cassette: json!({"tool": "shell", "response": "recorded"}),
                    kind: EvaluationCaseKind::Failure,
                    weight: 2,
                }],
                time_created: 10,
            })
            .expect("suite");
        let run = store
            .start_run(NewEvaluationRun {
                id: "run-1".to_owned(),
                suite_id: "suite-1".to_owned(),
                candidate_id: "candidate-1".to_owned(),
                model: "provider/model".to_owned(),
                toolset_digest: "tools".to_owned(),
                budget: json!({"tokens": 1000}),
                attempt_snapshot: json!({"seed": 7, "temperature": 0}),
                time_created: 11,
            })
            .expect("run");
        assert_eq!(run.attempt_snapshot["seed"], 7);
        assert_eq!(
            store.cases("suite-1").expect("cases")[0].tool_cassette["response"],
            "recorded"
        );
    }

    #[test]
    fn deterministic_suite_retry_rejects_changed_cassettes() {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
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
        let store = EvaluationStore::new(pool);
        let suite = NewEvaluationSuite {
            id: "suite-stable".to_owned(),
            project_id: "project-1".to_owned(),
            name: "immutable cassette".to_owned(),
            description: "candidate evidence".to_owned(),
            cases: vec![NewEvaluationCase {
                id: "case-stable".to_owned(),
                name: "failure".to_owned(),
                prompt: "diagnose".to_owned(),
                expected: "durable evidence".to_owned(),
                tool_cassette: json!({"recorded": "first"}),
                kind: EvaluationCaseKind::Failure,
                weight: 2,
            }],
            time_created: 10,
        };
        store.ensure_suite(suite.clone()).expect("first suite");
        store.ensure_suite(suite.clone()).expect("identical retry");

        let mut changed = suite;
        changed.cases[0].tool_cassette = json!({"recorded": "different"});
        let error = store
            .ensure_suite(changed)
            .expect_err("changed retry must conflict");
        assert!(matches!(error, DbError::Conflict { .. }));
        assert_eq!(
            store.cases("suite-stable").expect("stored cases")[0].tool_cassette,
            json!({"recorded": "first"})
        );
    }
}
