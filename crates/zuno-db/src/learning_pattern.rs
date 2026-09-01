//! Durable project and global patterns mined from independent experiences.

use crate::event_log::query_error;
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{LearningPatternProjection, LearningPatternStatus};

const COLUMNS: &str = "id, scope, project_id, fingerprint, title, summary, learned_rules, \
    evidence_ids, evidence_digest, evidence_version, independent_sessions, project_count, status, \
    rejected_evidence_version, time_created, time_updated";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternScope {
    Project,
    Global,
}

impl PatternScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "project" => Ok(Self::Project),
            "global" => Ok(Self::Global),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown learning pattern scope `{value}`"
            )))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLearningPattern {
    pub id: String,
    pub scope: PatternScope,
    pub project_id: Option<String>,
    pub fingerprint: String,
    pub title: String,
    pub summary: String,
    pub learned_rules: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub evidence_digest: String,
    pub evidence_version: i64,
    pub independent_sessions: u32,
    pub project_count: u32,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningPatternRecord {
    pub projection: LearningPatternProjection,
    pub scope: PatternScope,
    pub evidence_ids: Vec<String>,
    pub evidence_digest: String,
    pub rejected_evidence_version: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternProposal {
    Proposed {
        record: LearningPatternRecord,
        inserted: bool,
    },
    Suppressed {
        record: LearningPatternRecord,
    },
}

#[derive(Clone)]
pub struct LearningPatternStore {
    pool: Arc<Pool>,
}

impl LearningPatternStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Propose a pattern, reopening a rejection only when its evidence version advances.
    pub fn propose(&self, pattern: NewLearningPattern) -> Result<PatternProposal, DbError> {
        validate_new(&pattern)?;
        let learned_rules = serde_json::to_string(&pattern.learned_rules).map_err(query_error)?;
        let evidence_ids = serde_json::to_string(&pattern.evidence_ids).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let existing = read_by_identity(
                transaction,
                pattern.scope,
                pattern.project_id.as_deref(),
                &pattern.fingerprint,
            )?;
            if let Some(existing) = existing {
                if existing.projection.status == LearningPatternStatus::Rejected
                    && pattern.evidence_digest == existing.evidence_digest
                {
                    return Ok(PatternProposal::Suppressed { record: existing });
                }
                transaction
                    .execute(
                        "UPDATE learning_pattern SET
                           title = ?2, summary = ?3, learned_rules = ?4, evidence_ids = ?5,
                           evidence_digest = ?6, evidence_version = ?7,
                           independent_sessions = ?8, project_count = ?9, status = 'pending',
                           rejected_evidence_version = NULL, time_updated = ?10
                         WHERE id = ?1",
                        params![
                            existing.projection.id,
                            pattern.title,
                            pattern.summary,
                            learned_rules,
                            evidence_ids,
                            pattern.evidence_digest,
                            pattern.evidence_version,
                            i64::from(pattern.independent_sessions),
                            i64::from(pattern.project_count),
                            pattern.time_created,
                        ],
                    )
                    .map_err(open::map_error)?;
                return Ok(PatternProposal::Proposed {
                    record: read_required(transaction, &existing.projection.id)?,
                    inserted: false,
                });
            }
            transaction
                .execute(
                    "INSERT INTO learning_pattern (
                       id, scope, project_id, fingerprint, title, summary, learned_rules,
                       evidence_ids, evidence_digest, evidence_version, independent_sessions,
                       project_count, status, rejected_evidence_version, time_created, time_updated
                     ) VALUES (
                       ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       'pending', NULL, ?13, ?13
                     )",
                    params![
                        pattern.id,
                        pattern.scope.as_str(),
                        pattern.project_id,
                        pattern.fingerprint,
                        pattern.title,
                        pattern.summary,
                        learned_rules,
                        evidence_ids,
                        pattern.evidence_digest,
                        pattern.evidence_version,
                        i64::from(pattern.independent_sessions),
                        i64::from(pattern.project_count),
                        pattern.time_created,
                    ],
                )
                .map_err(open::map_error)?;
            Ok(PatternProposal::Proposed {
                record: read_required(transaction, &pattern.id)?,
                inserted: true,
            })
        })
    }

    pub fn reject(&self, id: &str, now: i64) -> Result<LearningPatternRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE learning_pattern
                     SET status = 'rejected', rejected_evidence_version = evidence_version,
                         time_updated = ?2
                     WHERE id = ?1 AND status = 'pending'",
                    params![id, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    pub fn promote(&self, id: &str, now: i64) -> Result<LearningPatternRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE learning_pattern
                     SET status = 'promoted', time_updated = ?2
                     WHERE id = ?1 AND status = 'pending'",
                    params![id, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    pub fn get(&self, id: &str) -> Result<LearningPatternRecord, DbError> {
        let connection = self.pool.get()?;
        read_required(&connection, id)
    }

    pub fn list_visible(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<LearningPatternRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM learning_pattern
                 WHERE (scope = 'project' AND project_id = ?1) OR scope = 'global'
                 ORDER BY CASE status WHEN 'pending' THEN 0 ELSE 1 END,
                          time_updated DESC, id DESC LIMIT ?2"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map(params![project_id, limit as i64], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }

    pub fn list_promoted_projects(&self) -> Result<Vec<LearningPatternRecord>, DbError> {
        let connection = self.pool.get()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {COLUMNS} FROM learning_pattern
                 WHERE scope = 'project' AND status = 'promoted'
                 ORDER BY fingerprint, project_id, id"
            ))
            .map_err(open::map_error)?;
        statement
            .query_map([], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect()
    }
}

fn validate_new(pattern: &NewLearningPattern) -> Result<(), DbError> {
    if pattern.id.trim().is_empty()
        || pattern.fingerprint.trim().is_empty()
        || pattern.title.trim().is_empty()
        || pattern.summary.trim().is_empty()
        || pattern.evidence_digest.trim().is_empty()
        || pattern.evidence_version < 1
        || pattern.project_count < 1
        || pattern.learned_rules.is_empty()
        || pattern.evidence_ids.is_empty()
    {
        return Err(query_error(std::io::Error::other(
            "learning pattern fields and evidence must not be empty",
        )));
    }
    match pattern.scope {
        PatternScope::Project if pattern.project_id.is_none() => Err(query_error(
            std::io::Error::other("project pattern requires project_id"),
        )),
        PatternScope::Global if pattern.project_id.is_some() => Err(query_error(
            std::io::Error::other("global pattern must not carry project_id"),
        )),
        PatternScope::Project | PatternScope::Global => Ok(()),
    }
}

fn read_required(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<LearningPatternRecord, DbError> {
    connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM learning_pattern WHERE id = ?1"),
            [id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "learning_pattern".to_owned(),
            id: id.to_owned(),
        })
        .and_then(decode)
}

fn read_by_identity(
    connection: &rusqlite::Connection,
    scope: PatternScope,
    project_id: Option<&str>,
    fingerprint: &str,
) -> Result<Option<LearningPatternRecord>, DbError> {
    connection
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM learning_pattern
                 WHERE scope = ?1 AND coalesce(project_id, '') = coalesce(?2, '')
                   AND fingerprint = ?3"
            ),
            params![scope.as_str(), project_id, fingerprint],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .map(decode)
        .transpose()
}

type StoredPattern = (
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    String,
    Option<i64>,
    i64,
    i64,
);

fn decode_row(row: &Row<'_>) -> rusqlite::Result<StoredPattern> {
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
    ))
}

fn decode(row: StoredPattern) -> Result<LearningPatternRecord, DbError> {
    let learned_rules = serde_json::from_str(&row.6).map_err(query_error)?;
    let evidence_ids = serde_json::from_str(&row.7).map_err(query_error)?;
    Ok(LearningPatternRecord {
        projection: LearningPatternProjection {
            id: row.0,
            project_id: row.2,
            fingerprint: row.3,
            title: row.4,
            summary: row.5,
            learned_rules,
            independent_sessions: u32::try_from(row.10).map_err(query_error)?,
            project_count: u32::try_from(row.11).map_err(query_error)?,
            status: LearningPatternStatus::parse(&row.12).ok_or_else(|| {
                query_error(std::io::Error::other(format!(
                    "unknown learning pattern status `{}`",
                    row.12
                )))
            })?,
            evidence_version: row.9,
            time_created: row.14,
            time_updated: row.15,
        },
        scope: PatternScope::parse(&row.1)?,
        evidence_ids,
        evidence_digest: row.8,
        rejected_evidence_version: row.13,
    })
}

fn require_changed(changed: usize, id: &str) -> Result<(), DbError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(query_error(std::io::Error::other(format!(
            "learning pattern `{id}` is not pending"
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration;
    use zuno_paths::DbLocation;

    fn store() -> LearningPatternStore {
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
        LearningPatternStore::new(pool)
    }

    fn proposal(version: i64) -> NewLearningPattern {
        NewLearningPattern {
            id: format!("pattern-{version}"),
            scope: PatternScope::Project,
            project_id: Some("project-1".to_owned()),
            fingerprint: "same-pattern".to_owned(),
            title: "Verify durable evidence".to_owned(),
            summary: "Repeated sessions benefited from durable evidence.".to_owned(),
            learned_rules: vec!["Inspect the durable source before inferring.".to_owned()],
            evidence_ids: vec![format!("experience-{version}")],
            evidence_digest: format!("digest-{version}"),
            evidence_version: version,
            independent_sessions: 3,
            project_count: 1,
            time_created: version,
        }
    }

    #[test]
    fn rejected_pattern_needs_new_evidence_before_reproposal() {
        let store = store();
        let first = store.propose(proposal(1)).expect("propose");
        let PatternProposal::Proposed { record, .. } = first else {
            panic!("first proposal suppressed")
        };
        store.reject(&record.projection.id, 2).expect("reject");
        assert!(matches!(
            store.propose(proposal(1)).expect("same evidence"),
            PatternProposal::Suppressed { .. }
        ));
        assert!(matches!(
            store.propose(proposal(2)).expect("new evidence"),
            PatternProposal::Proposed {
                inserted: false,
                ..
            }
        ));
    }
}
