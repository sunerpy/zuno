//! Durable experience records, evidence, extraction settlement, and SQLite FTS.

use crate::event_log::{NewSessionEvent, append_in, query_error};
use crate::{Pool, open};
use rusqlite::{OptionalExtension as _, Row, params};
use serde_json::{Map, Value};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{ExperienceKind, ExperienceProjection, ExperienceStatus};

const COLUMNS: &str = "id, project_id, session_id, source_message_id, extraction_job_id, \
    extraction_ordinal, kind, title, summary, resolution, confidence, fingerprint, status, \
    promoted_memory_candidate_id, time_created, time_updated";
const QUALIFIED_COLUMNS: &str = "experience_record.id, experience_record.project_id, \
    experience_record.session_id, experience_record.source_message_id, \
    experience_record.extraction_job_id, experience_record.extraction_ordinal, \
    experience_record.kind, experience_record.title, experience_record.summary, \
    experience_record.resolution, experience_record.confidence, experience_record.fingerprint, \
    experience_record.status, experience_record.promoted_memory_candidate_id, \
    experience_record.time_created, experience_record.time_updated";

const EXPERIENCE_FTS_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS experience_search_fts USING fts5(
  title,
  summary,
  resolution,
  content='experience_record',
  content_rowid='rowid',
  tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS experience_search_fts_insert AFTER INSERT ON experience_record BEGIN
  INSERT INTO experience_search_fts(rowid, title, summary, resolution)
  VALUES (new.rowid, new.title, new.summary, new.resolution);
END;
CREATE TRIGGER IF NOT EXISTS experience_search_fts_delete AFTER DELETE ON experience_record BEGIN
  INSERT INTO experience_search_fts(experience_search_fts, rowid, title, summary, resolution)
  VALUES ('delete', old.rowid, old.title, old.summary, old.resolution);
END;
CREATE TRIGGER IF NOT EXISTS experience_search_fts_update
AFTER UPDATE OF title, summary, resolution ON experience_record BEGIN
  INSERT INTO experience_search_fts(experience_search_fts, rowid, title, summary, resolution)
  VALUES ('delete', old.rowid, old.title, old.summary, old.resolution);
  INSERT INTO experience_search_fts(rowid, title, summary, resolution)
  VALUES (new.rowid, new.title, new.summary, new.resolution);
END;
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewExperienceEvidence {
    pub id: String,
    pub kind: ExperienceEvidenceKind,
    pub source_id: Option<String>,
    pub excerpt: String,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperienceEvidenceKind {
    Message,
    Tool,
    Feedback,
    Artifact,
    User,
}

impl ExperienceEvidenceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Tool => "tool",
            Self::Feedback => "feedback",
            Self::Artifact => "artifact",
            Self::User => "user",
        }
    }

    fn parse(value: &str) -> Result<Self, DbError> {
        match value {
            "message" => Ok(Self::Message),
            "tool" => Ok(Self::Tool),
            "feedback" => Ok(Self::Feedback),
            "artifact" => Ok(Self::Artifact),
            "user" => Ok(Self::User),
            _ => Err(query_error(std::io::Error::other(format!(
                "unknown experience evidence kind `{value}`"
            )))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperienceEvidenceRecord {
    pub id: String,
    pub experience_id: String,
    pub kind: ExperienceEvidenceKind,
    pub source_id: Option<String>,
    pub excerpt: String,
    pub digest: String,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewExperience {
    pub id: String,
    pub project_id: String,
    pub session_id: Option<String>,
    pub source_message_id: Option<String>,
    pub extraction_job_id: Option<String>,
    pub extraction_ordinal: Option<u32>,
    pub kind: ExperienceKind,
    pub title: String,
    pub summary: String,
    pub resolution: Option<String>,
    pub confidence: u16,
    pub fingerprint: String,
    pub evidence: Vec<NewExperienceEvidence>,
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperienceRecord {
    pub projection: ExperienceProjection,
    pub extraction_job_id: Option<String>,
    pub extraction_ordinal: Option<u32>,
    pub fingerprint: String,
    pub evidence: Vec<ExperienceEvidenceRecord>,
}

#[derive(Clone)]
pub struct ExperienceStore {
    pool: Arc<Pool>,
}

impl ExperienceStore {
    #[must_use]
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    /// Install the optional FTS provider and rebuild it from durable records.
    pub fn ensure_fts(&self) -> Result<(), DbError> {
        self.pool.transaction(|transaction| {
            transaction
                .execute_batch(EXPERIENCE_FTS_SQL)
                .map_err(open::map_error)?;
            transaction
                .execute(
                    "INSERT INTO experience_search_fts(experience_search_fts) VALUES ('rebuild')",
                    [],
                )
                .map_err(open::map_error)?;
            Ok(())
        })
    }

    /// Persist an extractor response and settle its job in one atomic commit.
    pub fn complete_extraction(
        &self,
        job_id: &str,
        owner_id: &str,
        experiences: &[NewExperience],
        result: &Value,
        now: i64,
    ) -> Result<Vec<ExperienceRecord>, DbError> {
        validate_batch(experiences)?;
        let result = serde_json::to_string(result).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let (session_id, source_message_id) = transaction
                .query_row(
                    "SELECT session_id, source_message_id FROM learning_job
                     WHERE id = ?1 AND owner_id = ?2 AND status = 'running'
                       AND kind = 'extraction'",
                    params![job_id, owner_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(open::map_error)?
                .ok_or_else(|| {
                    query_error(std::io::Error::other(format!(
                        "extraction job `{job_id}` is not running for owner `{owner_id}`"
                    )))
                })?;

            let mut stored = Vec::with_capacity(experiences.len());
            for experience in experiences {
                if experience.extraction_job_id.as_deref() != Some(job_id)
                    || experience.session_id.as_deref() != Some(session_id.as_str())
                    || experience.source_message_id.as_deref() != Some(source_message_id.as_str())
                {
                    return Err(query_error(std::io::Error::other(
                        "extracted experience provenance does not match its job",
                    )));
                }
                insert_experience(transaction, experience)?;
                let record = read_required(transaction, &experience.id)?;
                append_experience_event(transaction, &session_id, &record.projection)?;
                stored.push(record);
            }
            let changed = transaction
                .execute(
                    "UPDATE learning_job
                     SET status = 'completed', result = ?3, error = NULL, owner_id = NULL,
                         lease_expires = NULL, time_updated = ?4, time_completed = ?4
                     WHERE id = ?1 AND owner_id = ?2 AND status = 'running'",
                    params![job_id, owner_id, result, now],
                )
                .map_err(open::map_error)?;
            if changed != 1 {
                return Err(query_error(std::io::Error::other(format!(
                    "extraction job `{job_id}` lost its lease before commit"
                ))));
            }
            let mut properties = Map::new();
            properties.insert("jobID".to_owned(), Value::String(job_id.to_owned()));
            properties.insert(
                "sourceMessageID".to_owned(),
                Value::String(source_message_id),
            );
            properties.insert("experienceCount".to_owned(), Value::from(stored.len()));
            append_in(
                transaction,
                &session_id,
                NewSessionEvent::new("learning.extraction.completed", properties)?,
            )?;
            Ok(stored)
        })
    }

    /// Store a user-authored experience without an extractor job.
    pub fn create_manual(&self, experience: NewExperience) -> Result<ExperienceRecord, DbError> {
        validate_new(&experience)?;
        if experience.extraction_job_id.is_some() || experience.extraction_ordinal.is_some() {
            return Err(query_error(std::io::Error::other(
                "manual experience must not claim an extraction job",
            )));
        }
        self.pool.transaction(|transaction| {
            insert_experience(transaction, &experience)?;
            let record = read_required(transaction, &experience.id)?;
            if let Some(session_id) = experience.session_id.as_deref() {
                append_experience_event(transaction, session_id, &record.projection)?;
            }
            Ok(record)
        })
    }

    pub fn get(&self, id: &str) -> Result<ExperienceRecord, DbError> {
        let connection = self.pool.get()?;
        read_required(&connection, id)
    }

    pub fn list_for_project(
        &self,
        project_id: &str,
        limit: usize,
    ) -> Result<Vec<ExperienceRecord>, DbError> {
        let connection = self.pool.get()?;
        query_records(
            &connection,
            &format!(
                "SELECT {COLUMNS} FROM experience_record
                 WHERE project_id = ?1 AND status <> 'forgotten'
                 ORDER BY time_created DESC, id DESC LIMIT ?2"
            ),
            params![project_id, limit as i64],
        )
    }

    pub fn list_for_session(&self, session_id: &str) -> Result<Vec<ExperienceRecord>, DbError> {
        let connection = self.pool.get()?;
        query_records(
            &connection,
            &format!(
                "SELECT {COLUMNS} FROM experience_record
                 WHERE session_id = ?1 AND status <> 'forgotten'
                 ORDER BY time_created, id"
            ),
            [session_id],
        )
    }

    pub fn list_active_since(
        &self,
        project_id: &str,
        since: i64,
    ) -> Result<Vec<ExperienceRecord>, DbError> {
        let connection = self.pool.get()?;
        query_records(
            &connection,
            &format!(
                "SELECT {COLUMNS} FROM experience_record
                 WHERE project_id = ?1 AND status <> 'forgotten' AND time_created > ?2
                 ORDER BY time_created, id"
            ),
            params![project_id, since],
        )
    }

    /// Full-text search used by the explicit `experience_search` tool.
    pub fn search(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ExperienceRecord>, DbError> {
        if query.trim().is_empty() {
            return self.list_for_project(project_id, limit);
        }
        self.ensure_fts()?;
        let connection = self.pool.get()?;
        query_records(
            &connection,
            &format!(
                "SELECT {QUALIFIED_COLUMNS} FROM experience_search_fts
                 JOIN experience_record ON experience_record.rowid = experience_search_fts.rowid
                 WHERE experience_search_fts MATCH ?1
                   AND experience_record.project_id = ?2
                   AND experience_record.status <> 'forgotten'
                 ORDER BY bm25(experience_search_fts), experience_record.time_created DESC
                 LIMIT ?3"
            ),
            params![query, project_id, limit as i64],
        )
    }

    pub fn forget(&self, id: &str, now: i64) -> Result<ExperienceRecord, DbError> {
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE experience_record SET status = 'forgotten', time_updated = ?2
                     WHERE id = ?1 AND status <> 'forgotten'",
                    params![id, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    /// Hide an exact set of experiences in one transaction. The rows remain
    /// durable because pending Memory and Skill revocations still cite them.
    pub fn forget_many(&self, ids: &[String], now: i64) -> Result<Vec<String>, DbError> {
        let ids = ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        self.pool.transaction(|transaction| {
            for id in &ids {
                let changed = transaction
                    .execute(
                        "UPDATE experience_record SET status = 'forgotten', time_updated = ?2
                         WHERE id = ?1 AND status <> 'forgotten'",
                        params![id, now],
                    )
                    .map_err(open::map_error)?;
                require_changed(changed, id)?;
            }
            Ok(ids.into_iter().collect())
        })
    }

    /// Hide every experience derived from one source session while preserving
    /// the records needed by pending Memory and Skill revocation review.
    pub fn forget_for_session(&self, session_id: &str, now: i64) -> Result<Vec<String>, DbError> {
        self.pool.transaction(|transaction| {
            let mut statement = transaction
                .prepare(
                    "SELECT id FROM experience_record
                     WHERE session_id = ?1 AND status <> 'forgotten'
                     ORDER BY time_created, id",
                )
                .map_err(open::map_error)?;
            let ids = statement
                .query_map([session_id], |row| row.get::<_, String>(0))
                .map_err(open::map_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(open::map_error)?;
            drop(statement);
            transaction
                .execute(
                    "UPDATE experience_record
                     SET status = 'forgotten', time_updated = ?2
                     WHERE session_id = ?1 AND status <> 'forgotten'",
                    params![session_id, now],
                )
                .map_err(open::map_error)?;
            Ok(ids)
        })
    }

    pub fn solve(&self, id: &str, resolution: &str, now: i64) -> Result<ExperienceRecord, DbError> {
        if resolution.trim().is_empty() {
            return Err(query_error(std::io::Error::other(
                "experience resolution must not be empty",
            )));
        }
        self.pool.transaction(|transaction| {
            let changed = transaction
                .execute(
                    "UPDATE experience_record
                     SET kind = 'problem', resolution = ?2, time_updated = ?3
                     WHERE id = ?1 AND kind = 'unresolved_issue' AND status = 'active'",
                    params![id, resolution.trim(), now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    /// Link an experience to a resident-memory candidate. Unresolved issues are rejected in SQL
    /// and here so the caller receives a precise failure before a constraint error.
    pub fn mark_promoted(
        &self,
        id: &str,
        memory_candidate_id: &str,
        now: i64,
    ) -> Result<ExperienceRecord, DbError> {
        self.pool.transaction(|transaction| {
            let kind = transaction
                .query_row(
                    "SELECT kind FROM experience_record WHERE id = ?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(open::map_error)?
                .ok_or_else(|| DbError::NotFound {
                    table: "experience_record".to_owned(),
                    id: id.to_owned(),
                })?;
            if ExperienceKind::parse(&kind).is_some_and(|kind| !kind.promotable()) {
                return Err(query_error(std::io::Error::other(
                    "unresolved issues cannot be promoted to memory or Skill",
                )));
            }
            let changed = transaction
                .execute(
                    "UPDATE experience_record
                     SET status = 'promoted', promoted_memory_candidate_id = ?2,
                         time_updated = ?3
                     WHERE id = ?1 AND status = 'active'",
                    params![id, memory_candidate_id, now],
                )
                .map_err(open::map_error)?;
            require_changed(changed, id)?;
            read_required(transaction, id)
        })
    }

    pub fn count_new_since(&self, project_id: &str, since: i64) -> Result<u32, DbError> {
        let connection = self.pool.get()?;
        let count = connection
            .query_row(
                "SELECT count(*) FROM experience_record
                 WHERE project_id = ?1 AND status <> 'forgotten' AND time_created > ?2",
                params![project_id, since],
                |row| row.get::<_, i64>(0),
            )
            .map_err(open::map_error)?;
        u32::try_from(count).map_err(query_error)
    }
}

fn validate_batch(experiences: &[NewExperience]) -> Result<(), DbError> {
    for experience in experiences {
        validate_new(experience)?;
        if experience.extraction_job_id.is_none() || experience.extraction_ordinal.is_none() {
            return Err(query_error(std::io::Error::other(
                "extracted experiences require a job id and ordinal",
            )));
        }
    }
    Ok(())
}

fn validate_new(experience: &NewExperience) -> Result<(), DbError> {
    if experience.id.trim().is_empty()
        || experience.project_id.trim().is_empty()
        || experience.title.trim().is_empty()
        || experience.summary.trim().is_empty()
        || experience.fingerprint.trim().is_empty()
    {
        return Err(query_error(std::io::Error::other(
            "experience identity, title, summary, and fingerprint must not be empty",
        )));
    }
    if !experience.kind.promotable() && experience.resolution.is_some() {
        return Err(query_error(std::io::Error::other(
            "an unresolved issue cannot carry a resolution",
        )));
    }
    if experience.evidence.is_empty() {
        return Err(query_error(std::io::Error::other(
            "an experience requires at least one evidence item",
        )));
    }
    Ok(())
}

fn insert_experience(
    transaction: &rusqlite::Transaction<'_>,
    experience: &NewExperience,
) -> Result<(), DbError> {
    let changed = transaction
        .execute(
            "INSERT INTO experience_record (
               id, project_id, session_id, source_message_id, extraction_job_id,
               extraction_ordinal, kind, title, summary, resolution, confidence,
               fingerprint, status, time_created, time_updated
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'active', ?13, ?13
             )
             ON CONFLICT(extraction_job_id, extraction_ordinal)
             WHERE extraction_job_id IS NOT NULL DO NOTHING",
            params![
                experience.id,
                experience.project_id,
                experience.session_id,
                experience.source_message_id,
                experience.extraction_job_id,
                experience.extraction_ordinal.map(i64::from),
                experience.kind.as_str(),
                experience.title.trim(),
                experience.summary.trim(),
                experience.resolution.as_deref().map(str::trim),
                i64::from(experience.confidence),
                experience.fingerprint,
                experience.time_created,
            ],
        )
        .map_err(open::map_error)?;
    if changed != 1 {
        let existing = transaction
            .query_row(
                "SELECT id, fingerprint FROM experience_record
                 WHERE extraction_job_id = ?1 AND extraction_ordinal = ?2",
                params![
                    experience.extraction_job_id,
                    experience.extraction_ordinal.map(i64::from)
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(open::map_error)?;
        if existing != (experience.id.clone(), experience.fingerprint.clone()) {
            return Err(query_error(std::io::Error::other(
                "an extraction retry produced different content for a durable ordinal",
            )));
        }
        return Ok(());
    }
    for evidence in &experience.evidence {
        if evidence.id.trim().is_empty()
            || evidence.excerpt.trim().is_empty()
            || evidence.digest.trim().is_empty()
        {
            return Err(query_error(std::io::Error::other(
                "experience evidence identity, excerpt, and digest must not be empty",
            )));
        }
        transaction
            .execute(
                "INSERT INTO experience_evidence (
                   id, experience_id, kind, source_id, excerpt, digest, time_created
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    evidence.id,
                    experience.id,
                    evidence.kind.as_str(),
                    evidence.source_id,
                    evidence.excerpt,
                    evidence.digest,
                    experience.time_created,
                ],
            )
            .map_err(open::map_error)?;
    }
    Ok(())
}

fn append_experience_event(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    experience: &ExperienceProjection,
) -> Result<(), DbError> {
    let mut properties = Map::new();
    properties.insert(
        "experienceID".to_owned(),
        Value::String(experience.id.clone()),
    );
    properties.insert(
        "projectID".to_owned(),
        Value::String(experience.project_id.clone()),
    );
    properties.insert(
        "kind".to_owned(),
        Value::String(experience.kind.as_str().to_owned()),
    );
    properties.insert(
        "sourceMessageID".to_owned(),
        experience
            .source_message_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    append_in(
        transaction,
        session_id,
        NewSessionEvent::new("learning.experience.recorded", properties)?,
    )?;
    Ok(())
}

fn query_records<P>(
    connection: &rusqlite::Connection,
    sql: &str,
    parameters: P,
) -> Result<Vec<ExperienceRecord>, DbError>
where
    P: rusqlite::Params,
{
    let mut statement = connection.prepare(sql).map_err(open::map_error)?;
    statement
        .query_map(parameters, decode_row)
        .map_err(open::map_error)?
        .map(|row| {
            row.map_err(open::map_error)
                .and_then(decode_record)
                .and_then(|mut record| {
                    record.evidence = read_evidence(connection, &record.projection.id)?;
                    Ok(record)
                })
        })
        .collect()
}

fn read_required(connection: &rusqlite::Connection, id: &str) -> Result<ExperienceRecord, DbError> {
    let mut record = connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM experience_record WHERE id = ?1"),
            [id],
            decode_row,
        )
        .optional()
        .map_err(open::map_error)?
        .ok_or_else(|| DbError::NotFound {
            table: "experience_record".to_owned(),
            id: id.to_owned(),
        })
        .and_then(decode_record)?;
    record.evidence = read_evidence(connection, id)?;
    Ok(record)
}

fn read_evidence(
    connection: &rusqlite::Connection,
    experience_id: &str,
) -> Result<Vec<ExperienceEvidenceRecord>, DbError> {
    let mut statement = connection
        .prepare(
            "SELECT id, experience_id, kind, source_id, excerpt, digest, time_created
             FROM experience_evidence WHERE experience_id = ?1 ORDER BY time_created, id",
        )
        .map_err(open::map_error)?;
    statement
        .query_map([experience_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(open::map_error)?
        .map(|row| {
            let row = row.map_err(open::map_error)?;
            Ok(ExperienceEvidenceRecord {
                id: row.0,
                experience_id: row.1,
                kind: ExperienceEvidenceKind::parse(&row.2)?,
                source_id: row.3,
                excerpt: row.4,
                digest: row.5,
                time_created: row.6,
            })
        })
        .collect()
}

type StoredExperience = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    String,
    Option<String>,
    i64,
    i64,
);

fn decode_row(row: &Row<'_>) -> rusqlite::Result<StoredExperience> {
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

fn decode_record(row: StoredExperience) -> Result<ExperienceRecord, DbError> {
    let confidence = u16::try_from(row.10).map_err(query_error)?;
    let extraction_ordinal = row.5.map(u32::try_from).transpose().map_err(query_error)?;
    Ok(ExperienceRecord {
        projection: ExperienceProjection {
            id: row.0,
            project_id: row.1,
            session_id: row.2,
            source_message_id: row.3,
            kind: ExperienceKind::parse(&row.6).ok_or_else(|| {
                query_error(std::io::Error::other(format!(
                    "unknown experience kind `{}`",
                    row.6
                )))
            })?,
            title: row.7,
            summary: row.8,
            resolution: row.9,
            confidence,
            status: ExperienceStatus::parse(&row.12).ok_or_else(|| {
                query_error(std::io::Error::other(format!(
                    "unknown experience status `{}`",
                    row.12
                )))
            })?,
            promoted_memory_candidate_id: row.13,
            time_created: row.14,
            time_updated: row.15,
        },
        extraction_job_id: row.4,
        extraction_ordinal,
        fingerprint: row.11,
        evidence: Vec::new(),
    })
}

fn require_changed(changed: usize, id: &str) -> Result<(), DbError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(query_error(std::io::Error::other(format!(
            "experience `{id}` is not in the required state"
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning_job::{LearningJobStore, NewLearningJob};
    use crate::migration;
    use serde_json::json;
    use zuno_paths::DbLocation;

    fn fixture() -> (Arc<Pool>, ExperienceStore) {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute_batch(
                    "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                     VALUES ('project-1', '/workspace', 1, 1, '[]');
                     INSERT INTO session
                       (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES ('session-1', 'project-1', 'slug', '/workspace', 'title', '1', 1, 1);
                     INSERT INTO message (id, session_id, time_created, time_updated, data)
                     VALUES ('assistant-1', 'session-1', 1, 1, '{\"role\":\"assistant\"}');",
                )
                .expect("fixture");
        }
        (pool.clone(), ExperienceStore::new(pool))
    }

    fn extracted(kind: ExperienceKind) -> NewExperience {
        NewExperience {
            id: "experience-1".to_owned(),
            project_id: "project-1".to_owned(),
            session_id: Some("session-1".to_owned()),
            source_message_id: Some("assistant-1".to_owned()),
            extraction_job_id: Some("job-1".to_owned()),
            extraction_ordinal: Some(0),
            kind,
            title: "Keep exact evidence".to_owned(),
            summary: "A persisted trace made the diagnosis reproducible.".to_owned(),
            resolution: kind
                .promotable()
                .then(|| "Inspect the durable trace.".to_owned()),
            confidence: 9200,
            fingerprint: "fingerprint-1".to_owned(),
            evidence: vec![NewExperienceEvidence {
                id: "evidence-1".to_owned(),
                kind: ExperienceEvidenceKind::Message,
                source_id: Some("assistant-1".to_owned()),
                excerpt: "durable trace".to_owned(),
                digest: "digest-1".to_owned(),
            }],
            time_created: 20,
        }
    }

    #[test]
    fn extraction_settlement_is_atomic_and_fts_searches_it() {
        let (pool, store) = fixture();
        LearningJobStore::new(pool)
            .enqueue(NewLearningJob::extraction(
                "job-1",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({}),
                10,
            ))
            .expect("enqueue");
        let jobs = LearningJobStore::new(store.pool.clone());
        jobs.claim_due("worker-1", 11, 30)
            .expect("claim")
            .expect("job");

        let records = store
            .complete_extraction(
                "job-1",
                "worker-1",
                &[extracted(ExperienceKind::Procedure)],
                &json!({"count": 1}),
                21,
            )
            .expect("complete");
        assert_eq!(records.len(), 1);
        assert_eq!(jobs.get("job-1").expect("job").status.as_str(), "completed");
        let found = store
            .search("project-1", "reproducible", 5)
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].projection.id, "experience-1");
    }

    #[test]
    fn unresolved_issue_cannot_be_promoted() {
        let (pool, store) = fixture();
        LearningJobStore::new(pool)
            .enqueue(NewLearningJob::extraction(
                "job-1",
                "project-1",
                "session-1",
                "assistant-1",
                "extractor-v1",
                json!({}),
                10,
            ))
            .expect("enqueue");
        let jobs = LearningJobStore::new(store.pool.clone());
        jobs.claim_due("worker-1", 11, 30)
            .expect("claim")
            .expect("job");
        let mut unresolved = extracted(ExperienceKind::UnresolvedIssue);
        unresolved.resolution = None;
        store
            .complete_extraction("job-1", "worker-1", &[unresolved], &json!({"count": 1}), 21)
            .expect("complete");
        let error = store
            .mark_promoted("experience-1", "memory-1", 22)
            .expect_err("must reject unresolved promotion");
        assert!(
            std::error::Error::source(&error)
                .expect("source")
                .to_string()
                .contains("unresolved")
        );
    }
}
