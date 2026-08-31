//! Review, evaluation, apply, undo, and reconciliation state for Skill candidates.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{SkillCandidateOperation, SkillCandidateProjection, SkillCandidateStatus};

const COLUMNS: &str = "id, project_id, pattern_id, name, target_source, target_path, \
    target_writable, target_digest, proposed_content, proposed_digest, diff, evidence_ids, \
    learned_rules, operation_kind, reverts_candidate_id, status, evaluation_run_id, \
    before_content, after_content, companion_name, apply_operation_id, error, time_created, \
    time_updated, time_applied";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSkillCandidate {
    pub id: String,
    pub project_id: String,
    pub pattern_id: Option<String>,
    pub name: String,
    pub target_source: String,
    pub target_path: Option<String>,
    pub target_writable: bool,
    pub target_digest: String,
    pub proposed_content: String,
    pub proposed_digest: String,
    pub diff: String,
    pub evidence_ids: Vec<String>,
    pub learned_rules: Vec<String>,
    pub operation: zuno_types::SkillCandidateOperation,
    pub reverts_candidate_id: Option<String>,
    pub companion_name: Option<String>,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidateRecord {
    pub projection: SkillCandidateProjection,
    pub target_path: Option<String>,
    pub target_writable: bool,
    pub proposed_content: String,
    pub evidence_ids: Vec<String>,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
    pub companion_name: Option<String>,
    pub apply_operation_id: Option<String>,
    pub time_applied: Option<i64>,
}

#[derive(Clone)]
pub struct SkillCandidateStore {
    pool: Arc<Pool>,
}

impl SkillCandidateStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn create(&self, candidate: NewSkillCandidate) -> Result<SkillCandidateRecord, DbError> {
        validate_new(&candidate)?;
        let evidence_ids = serde_json::to_string(&candidate.evidence_ids).map_err(query_error)?;
        let learned_rules = serde_json::to_string(&candidate.learned_rules).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            transaction
                .execute(
                    "INSERT INTO skill_candidate (
                       id, project_id, pattern_id, name, target_source, target_path,
                       target_writable, target_digest, proposed_content, proposed_digest, diff,
                       evidence_ids, learned_rules, operation_kind, reverts_candidate_id,
                       status, companion_name,
                       time_created, time_updated
                    ) VALUES (
                       ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                       ?14, ?15, 'pending_review', ?16, ?17, ?17
                     )
                     ON CONFLICT DO NOTHING",
                    params![
                        candidate.id,
                        candidate.project_id,
                        candidate.pattern_id,
                        candidate.name,
                        candidate.target_source,
                        candidate.target_path,
                        candidate.target_writable,
                        candidate.target_digest,
                        candidate.proposed_content,
                        candidate.proposed_digest,
                        candidate.diff,
                        evidence_ids,
                        learned_rules,
                        candidate.operation.as_str(),
                        candidate.reverts_candidate_id,
                        candidate.companion_name,
                        candidate.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            match (
                candidate.reverts_candidate_id.as_deref(),
                candidate.pattern_id.as_deref(),
            ) {
                (Some(reverts_candidate_id), _) => {
                    read_by_reverts(transaction, reverts_candidate_id)
                }
                (None, Some(pattern_id)) => {
                    read_by_pattern_digest(transaction, pattern_id, &candidate.proposed_digest)
                }
                (None, None) => read_required(transaction, &candidate.id),
            }
        })
    }

    /// Explicit review starts evaluation; there is no automatic path into this state.
    pub fn begin_evaluation(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError> {
        transition(
            &self.pool,
            id,
            "pending_review",
            SkillCandidateStatus::Evaluating,
            None,
            now,
        )
    }

    pub fn settle_evaluation(
        &self,
        id: &str,
        run_id: &str,
        passed: bool,
        error: Option<&str>,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        let status = if passed {
            SkillCandidateStatus::Approved
        } else {
            SkillCandidateStatus::Failed
        };
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE skill_candidate
                     SET status = ?2, evaluation_run_id = ?3, error = ?4, time_updated = ?5
                     WHERE id = ?1 AND status = 'evaluating'",
                    params![id, status.as_str(), run_id, error, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id, "evaluating")?;
            read_required(transaction, id)
        })
    }

    pub fn fail_evaluation(
        &self,
        id: &str,
        error: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE skill_candidate
                     SET status = 'failed', error = ?2, time_updated = ?3
                     WHERE id = ?1 AND status = 'evaluating'",
                    params![id, error, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id, "evaluating")?;
            read_required(transaction, id)
        })
    }

    /// Persist exact snapshots before the filesystem effect starts.
    pub fn begin_apply(
        &self,
        id: &str,
        operation_id: &str,
        before_content: &str,
        after_content: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        if operation_id.trim().is_empty() {
            return Err(query_error(std::io::Error::other(
                "Skill apply operation id must not be empty",
            )));
        }
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE skill_candidate
                     SET status = 'applying', before_content = ?2, after_content = ?3,
                         apply_operation_id = ?4, error = NULL, time_updated = ?5
                     WHERE id = ?1 AND status = 'approved'",
                    params![id, before_content, after_content, operation_id, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id, "approved")?;
            read_required(transaction, id)
        })
    }

    pub fn begin_undo(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError> {
        transition(
            &self.pool,
            id,
            "applied",
            SkillCandidateStatus::Undoing,
            None,
            now,
        )
    }

    pub fn finish_effect(
        &self,
        id: &str,
        expected_status: SkillCandidateStatus,
        status: SkillCandidateStatus,
        error: Option<&str>,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        let valid = matches!(
            (expected_status, status),
            (
                SkillCandidateStatus::Applying,
                SkillCandidateStatus::Applied
                    | SkillCandidateStatus::Failed
                    | SkillCandidateStatus::Uncertain
                    | SkillCandidateStatus::Stale
            ) | (
                SkillCandidateStatus::Undoing,
                SkillCandidateStatus::Undone
                    | SkillCandidateStatus::Failed
                    | SkillCandidateStatus::Uncertain
                    | SkillCandidateStatus::Stale
            )
        );
        if !valid {
            return Err(query_error(std::io::Error::other(
                "invalid Skill effect settlement transition",
            )));
        }
        self.pool.transaction(|transaction| {
            let applied = (status == SkillCandidateStatus::Applied).then_some(now);
            let changed = transaction
                .execute(
                    "UPDATE skill_candidate
                     SET status = ?2, error = ?3, time_updated = ?4,
                         time_applied = coalesce(?5, time_applied)
                     WHERE id = ?1 AND status = ?6",
                    params![
                        id,
                        status.as_str(),
                        error,
                        now,
                        applied,
                        expected_status.as_str()
                    ],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id, expected_status.as_str())?;
            read_required(transaction, id)
        })
    }

    pub fn reject(&self, id: &str, now: i64) -> Result<SkillCandidateRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE skill_candidate
                     SET status = 'rejected', error = NULL, time_updated = ?2
                     WHERE id = ?1 AND status IN ('pending_review','evaluating','approved','failed')",
                    params![id, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id, "reviewable")?;
            read_required(transaction, id)
        })
    }

    pub fn mark_stale(
        &self,
        id: &str,
        error: &str,
        now: i64,
    ) -> Result<SkillCandidateRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE skill_candidate
                     SET status = 'stale', error = ?2, time_updated = ?3
                     WHERE id = ?1 AND status IN ('approved','applying','undoing')",
                    params![id, error, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id, "approved or in-flight")?;
            read_required(transaction, id)
        })
    }

    pub fn get(&self, id: &str) -> Result<SkillCandidateRecord, DbError> {
        let connection = self.pool.get()?;
        read_required(&connection, id)
    }

    pub fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidateRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM skill_candidate
                 WHERE project_id = ?1
                 ORDER BY CASE status
                   WHEN 'pending_review' THEN 0
                   WHEN 'evaluating' THEN 1
                   WHEN 'approved' THEN 2
                   WHEN 'applying' THEN 3
                   WHEN 'undoing' THEN 4
                   WHEN 'uncertain' THEN 5
                   ELSE 6 END,
                   time_created DESC, id DESC LIMIT ?2"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map(params![project_id, limit as i64], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }

    pub fn list_applied_referencing(
        &self,
        experience_ids: &[String],
    ) -> Result<Vec<SkillCandidateRecord>, DbError> {
        if experience_ids.is_empty() {
            return Ok(Vec::new());
        }
        let experience_ids = serde_json::to_string(experience_ids).map_err(query_error)?;
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM skill_candidate
                 WHERE operation_kind = 'apply' AND status = 'applied'
                   AND EXISTS (
                     SELECT 1
                     FROM json_each(skill_candidate.evidence_ids) AS evidence
                     JOIN json_each(?1) AS requested ON requested.value = evidence.value
                   )
                 ORDER BY time_created, id"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map([experience_ids], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }

    pub fn list_referencing(
        &self,
        experience_ids: &[String],
    ) -> Result<Vec<SkillCandidateRecord>, DbError> {
        if experience_ids.is_empty() {
            return Ok(Vec::new());
        }
        let experience_ids = serde_json::to_string(experience_ids).map_err(query_error)?;
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM skill_candidate
                 WHERE EXISTS (
                   SELECT 1
                   FROM json_each(skill_candidate.evidence_ids) AS evidence
                   JOIN json_each(?1) AS requested ON requested.value = evidence.value
                 )
                 ORDER BY time_created, id"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map([experience_ids], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }

    pub fn list_inflight(&self) -> Result<Vec<SkillCandidateRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM skill_candidate
                 WHERE status IN ('applying','undoing') ORDER BY time_updated, id"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map([], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }

    pub fn fail_interrupted_evaluations(&self, now: i64) -> Result<usize, DbError> {
        let connection = self.pool.get()?;
        connection
            .execute(
                "UPDATE skill_candidate
                 SET status = 'failed',
                     error = 'evaluation process stopped before settlement',
                     time_updated = ?1
                 WHERE status = 'evaluating'",
                [now],
            )
            .map_err(open::map_error)
    }
}

fn transition(
    pool: &Pool,
    id: &str,
    from: &str,
    to: SkillCandidateStatus,
    error: Option<&str>,
    now: i64,
) -> Result<SkillCandidateRecord, DbError> {
    pool.transaction(|transaction| {
        let changed = transaction
            .execute(
                "UPDATE skill_candidate SET status = ?2, error = ?3, time_updated = ?4
                 WHERE id = ?1 AND status = ?5",
                params![id, to.as_str(), error, now, from],
            )
            .map_err(open::map_error)?;
        require_changed(changed, id, from)?;
        read_required(transaction, id)
    })
}

fn validate_new(candidate: &NewSkillCandidate) -> Result<(), DbError> {
    if candidate.id.trim().is_empty()
        || candidate.project_id.trim().is_empty()
        || candidate.name.trim().is_empty()
        || candidate.target_source.trim().is_empty()
        || candidate.target_digest.trim().is_empty()
        || (candidate.operation == SkillCandidateOperation::Apply
            && candidate.proposed_content.trim().is_empty())
        || candidate.proposed_digest.trim().is_empty()
        || candidate.diff.trim().is_empty()
        || candidate.evidence_ids.is_empty()
        || candidate.learned_rules.is_empty()
        || candidate.learned_rules.len() > 15
    {
        return Err(query_error(std::io::Error::other(
            "Skill candidate requires complete content, diff, evidence, and 1..=15 learned rules",
        )));
    }
    if candidate.target_writable && candidate.target_path.is_none() {
        return Err(query_error(std::io::Error::other(
            "writable Skill target requires a target path",
        )));
    }
    if !candidate.target_writable
        && candidate
            .companion_name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
    {
        return Err(query_error(std::io::Error::other(
            "read-only Skill target requires a distinct companion Skill name",
        )));
    }
    if (candidate.operation == SkillCandidateOperation::Apply
        && candidate.reverts_candidate_id.is_some())
        || (candidate.operation == SkillCandidateOperation::Revoke
            && candidate.reverts_candidate_id.is_none())
    {
        return Err(query_error(std::io::Error::other(
            "Skill apply candidates cannot name a reversal target and revoke candidates must name one",
        )));
    }
    Ok(())
}

fn read_required(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<SkillCandidateRecord, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM skill_candidate WHERE id = ?1"),
            [id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "skill_candidate".to_owned(),
            id: id.to_owned(),
        })
        .and_then(decode)
}

fn read_by_pattern_digest(
    connection: &rusqlite::Connection,
    pattern_id: &str,
    proposed_digest: &str,
) -> Result<SkillCandidateRecord, DbError> {
    connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM skill_candidate
                 WHERE pattern_id = ?1 AND proposed_digest = ?2"
            ),
            params![pattern_id, proposed_digest],
            decode_row,
        )
        .map_err(open::map_error)
        .and_then(decode)
}

fn read_by_reverts(
    connection: &rusqlite::Connection,
    reverts_candidate_id: &str,
) -> Result<SkillCandidateRecord, DbError> {
    connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM skill_candidate
                 WHERE reverts_candidate_id = ?1"
            ),
            [reverts_candidate_id],
            decode_row,
        )
        .map_err(open::map_error)
        .and_then(decode)
}

type StoredSkillCandidate = (
    String,         // id
    String,         // project_id
    Option<String>, // pattern_id
    String,         // name
    String,         // target_source
    Option<String>, // target_path
    bool,           // target_writable
    String,         // target_digest
    String,         // proposed_content
    String,         // proposed_digest
    String,         // diff
    String,         // evidence_ids
    String,         // learned_rules
    String,         // operation_kind
    Option<String>, // reverts_candidate_id
    String,         // status
    Option<String>, // evaluation_run_id
    Option<String>, // before_content
    Option<String>, // after_content
    Option<String>, // companion_name
    Option<String>, // apply_operation_id
    Option<String>, // error
    i64,            // time_created
    i64,            // time_updated
    Option<i64>,    // time_applied
);

fn decode_row(row: &Row<'_>) -> rusqlite::Result<StoredSkillCandidate> {
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
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
        row.get(19)?,
        row.get(20)?,
        row.get(21)?,
        row.get(22)?,
        row.get(23)?,
        row.get(24)?,
    ))
}

fn decode(row: StoredSkillCandidate) -> Result<SkillCandidateRecord, DbError> {
    let operation = SkillCandidateOperation::parse(&row.13).ok_or_else(|| {
        query_error(std::io::Error::other(format!(
            "unknown Skill candidate operation `{}`",
            row.13
        )))
    })?;
    let status = SkillCandidateStatus::parse(&row.15).ok_or_else(|| {
        query_error(std::io::Error::other(format!(
            "unknown Skill candidate status `{}`",
            row.15
        )))
    })?;
    let learned_rules = serde_json::from_str(&row.12).map_err(query_error)?;
    Ok(SkillCandidateRecord {
        projection: SkillCandidateProjection {
            id: row.0,
            project_id: row.1,
            pattern_id: row.2,
            name: row.3,
            target_source: row.4,
            target_digest: row.7,
            proposed_digest: row.9,
            diff: row.10,
            learned_rules,
            operation,
            reverts_candidate_id: row.14,
            status,
            evaluation_run_id: row.16,
            error: row.21,
            time_created: row.22,
            time_updated: row.23,
        },
        target_path: row.5,
        target_writable: row.6,
        proposed_content: row.8,
        evidence_ids: serde_json::from_str(&row.11).map_err(query_error)?,
        before_content: row.17,
        after_content: row.18,
        companion_name: row.19,
        apply_operation_id: row.20,
        time_applied: row.24,
    })
}

fn require_changed(changed: usize, id: &str, expected: &str) -> Result<(), DbError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(query_error(std::io::Error::other(format!(
            "Skill candidate `{id}` is not {expected}"
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::{
        EvaluationCaseKind, EvaluationStore, NewEvaluationCase, NewEvaluationRun,
        NewEvaluationSuite,
    };
    use crate::migration;
    use serde_json::json;
    use zuno_paths::DbLocation;

    fn store() -> SkillCandidateStore {
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
        SkillCandidateStore::new(pool)
    }

    fn candidate() -> NewSkillCandidate {
        NewSkillCandidate {
            id: "candidate-1".to_owned(),
            project_id: "project-1".to_owned(),
            pattern_id: None,
            name: "durable-evidence".to_owned(),
            target_source: "project://.agents/skills/durable-evidence/SKILL.md".to_owned(),
            target_path: Some("/workspace/.agents/skills/durable-evidence/SKILL.md".to_owned()),
            target_writable: true,
            target_digest: "before-digest".to_owned(),
            proposed_content: "# Durable evidence\n".to_owned(),
            proposed_digest: "after-digest".to_owned(),
            diff: "@@ skill @@".to_owned(),
            evidence_ids: vec!["experience-1".to_owned()],
            learned_rules: vec!["Inspect durable evidence.".to_owned()],
            operation: SkillCandidateOperation::Apply,
            reverts_candidate_id: None,
            companion_name: None,
            time_created: 10,
        }
    }

    #[test]
    fn apply_cannot_start_before_review_and_evaluation() {
        let store = store();
        store.create(candidate()).expect("candidate");
        assert!(
            store
                .begin_apply("candidate-1", "operation-1", "before", "after", 11)
                .is_err()
        );
        store
            .begin_evaluation("candidate-1", 12)
            .expect("human review");
        let evaluations = EvaluationStore::new(store.pool.clone());
        evaluations
            .create_suite(NewEvaluationSuite {
                id: "suite-1".to_owned(),
                project_id: "project-1".to_owned(),
                name: "candidate suite".to_owned(),
                description: String::new(),
                cases: vec![NewEvaluationCase {
                    id: "case-1".to_owned(),
                    name: "case".to_owned(),
                    prompt: "prompt".to_owned(),
                    expected: "expected".to_owned(),
                    tool_cassette: json!({}),
                    kind: EvaluationCaseKind::General,
                    weight: 1,
                }],
                time_created: 12,
            })
            .expect("suite");
        evaluations
            .start_run(NewEvaluationRun {
                id: "run-1".to_owned(),
                suite_id: "suite-1".to_owned(),
                candidate_id: "candidate-1".to_owned(),
                model: "provider/model".to_owned(),
                toolset_digest: "tools".to_owned(),
                budget: json!({}),
                attempt_snapshot: json!({}),
                time_created: 12,
            })
            .expect("run");
        store
            .settle_evaluation("candidate-1", "run-1", true, None, 13)
            .expect("evaluation");
        let applying = store
            .begin_apply("candidate-1", "operation-1", "before", "after", 14)
            .expect("apply");
        assert_eq!(applying.projection.status, SkillCandidateStatus::Applying);
    }
}
