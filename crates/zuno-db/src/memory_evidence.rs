//! Revalidated, content-addressed evidence for automatic memory.
//!
//! Cached verification flags are not authority: source bytes may have changed or
//! been deleted since extraction. Both the writer transaction and the read path
//! use this module. Disabling future generation does not revoke existing memory.

use crate::experience::{ExperienceEvidenceKind, ExperienceRecord};
use crate::learning_source::{LearningSource, LearningSourceKind, LearningSourceStore};
use crate::{Connection, Pool, open};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{ExperienceKind, ExperienceStatus};

pub const MAX_MEMORY_EVIDENCE: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEvidenceReference {
    pub experience_id: String,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct MemoryEvidence {
    pub reference: MemoryEvidenceReference,
    pub record: ExperienceRecord,
    pub generation_allowed: bool,
    /// User statements can support preferences/corrections, not a fabricated
    /// successful tool execution.
    pub user_authored: bool,
    /// Stage-one suggestions, never executable instructions or independent proof.
    pub raw_hints: Vec<Value>,
}

#[derive(Clone)]
pub struct MemoryEvidenceStore {
    pool: Arc<Pool>,
}

impl MemoryEvidenceStore {
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn get(&self, id: &str) -> Result<Option<MemoryEvidence>, DbError> {
        let connection = self.pool.get()?;
        collect_on(&connection, id)
    }

    pub fn select(&self, project_id: &str, limit: usize) -> Result<Vec<MemoryEvidence>, DbError> {
        let connection = self.pool.get()?;
        let limit = limit.clamp(1, MAX_MEMORY_EVIDENCE);
        let mut statement = connection
            .prepare(
                "SELECT id FROM experience_record
                 WHERE project_id=?1 AND status IN ('active','promoted')
                   AND kind<>'unresolved_issue'
                 ORDER BY time_created DESC,id DESC LIMIT 512",
            )
            .map_err(open::map_error)?;
        let ids = statement
            .query_map([project_id], |row| row.get::<_, String>(0))
            .map_err(open::map_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(open::map_error)?;
        let mut selected = Vec::new();
        for id in ids {
            if let Some(evidence) = collect_on(&connection, &id)?
                && evidence.generation_allowed
            {
                selected.push(evidence);
                if selected.len() == limit {
                    break;
                }
            }
        }
        Ok(selected)
    }

    pub fn references_current(
        &self,
        references: &[MemoryEvidenceReference],
        for_write: bool,
    ) -> Result<bool, DbError> {
        let connection = self.pool.get()?;
        references_current_on(&connection, references, for_write)
    }
}

pub(crate) fn references_current_on(
    connection: &Connection,
    references: &[MemoryEvidenceReference],
    for_write: bool,
) -> Result<bool, DbError> {
    if references.is_empty() || references.len() > MAX_MEMORY_EVIDENCE {
        return Ok(false);
    }
    for reference in references {
        let Some(current) = collect_on(connection, &reference.experience_id)? else {
            return Ok(false);
        };
        if current.reference != *reference || (for_write && !current.generation_allowed) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// An entry with multiple independent sources survives the loss of one source.
pub(crate) fn any_reference_current_on(
    connection: &Connection,
    references: &[MemoryEvidenceReference],
) -> Result<bool, DbError> {
    if references.len() > 4_096 {
        return Ok(false);
    }
    for reference in references {
        if let Some(current) = collect_on(connection, &reference.experience_id)?
            && current.reference == *reference
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn collect_on(
    connection: &Connection,
    id: &str,
) -> Result<Option<MemoryEvidence>, DbError> {
    if id.is_empty() || id.len() > 256 {
        return Ok(None);
    }
    let record = match crate::experience::read_required(connection, id) {
        Ok(record) => record,
        Err(DbError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    if record.projection.status == ExperienceStatus::Forgotten
        || !record.projection.kind.promotable()
        || !record.verified_sources()
    {
        return Ok(None);
    }
    let mut eligible = false;
    let mut user_authored = false;
    let mut raw_hints = Vec::new();
    if let Some(job_id) = &record.extraction_job_id {
        let payload: Option<String> = connection
            .query_row(
                "SELECT payload FROM learning_job WHERE id=?1 AND kind='extraction' AND status='completed'
                   AND length(CAST(payload AS BLOB))<=2097152",
                [job_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(open::map_error)?
            .flatten();
        let Some(payload) = payload else {
            return Ok(None);
        };
        let payload: Value = serde_json::from_str(&payload).map_err(query_error)?;
        let Some(sources) = payload.get("request").unwrap_or(&payload).get("sources") else {
            return Ok(None);
        };
        let sources: Vec<LearningSource> =
            serde_json::from_value(sources.clone()).map_err(query_error)?;
        if sources.len() > 256 {
            return Ok(None);
        }
        let Some(session_id) = &record.projection.session_id else {
            return Ok(None);
        };
        let result: Option<String> = connection
            .query_row(
                "SELECT result FROM learning_job WHERE id=?1
                   AND length(CAST(result AS BLOB))<=2097152",
                [job_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(open::map_error)?
            .flatten();
        if let Some(result) = result {
            let result: Value = serde_json::from_str(&result).map_err(query_error)?;
            if let Some(hints) = result.get("memoryHints").and_then(Value::as_array) {
                raw_hints = hints
                    .iter()
                    .take(16)
                    .filter(|hint| hint.get("experienceId").and_then(Value::as_str) == Some(id))
                    .filter_map(|hint| hint.get("hint").filter(|value| !value.is_null()).cloned())
                    .collect();
            }
        }
        for evidence in &record.evidence {
            let Some(source) = sources.iter().find(|source| {
                evidence.source_id.as_deref() == Some(source.source_id.as_str())
                    && evidence.source_digest.as_deref() == Some(source.source_digest.as_str())
                    && same_kind(evidence.kind, source.kind)
                    && source.content.contains(evidence.excerpt.as_str())
            }) else {
                return Ok(None);
            };
            if crate::learning_source::digest(&evidence.excerpt) != evidence.digest
                || !LearningSourceStore::source_is_current_on(connection, session_id, source)?
                || matches!(
                    source.tool.as_deref(),
                    Some("memory_update" | "memory_propose" | "memory_read" | "experience_search")
                )
            {
                return Ok(None);
            }
            let human = source.kind == LearningSourceKind::User
                || (source.kind == LearningSourceKind::Feedback
                    && serde_json::from_str::<Value>(&source.content)
                        .ok()
                        .and_then(|value| {
                            value.get("note").and_then(Value::as_str).map(str::to_owned)
                        })
                        .is_some_and(|note| !note.trim().is_empty()));
            user_authored |= human;
            eligible |= source.proves_success
                || (human
                    && matches!(
                        record.projection.kind,
                        ExperienceKind::UserCorrection | ExperienceKind::ExplicitFeedback
                    ));
        }
    } else {
        // Host-admitted /learn remember records are explicit user input. Losing an
        // extraction job must not turn arbitrary tool evidence into a manual note.
        for evidence in &record.evidence {
            if evidence.kind != ExperienceEvidenceKind::User
                || !evidence.promotion_eligible
                || crate::learning_source::digest(&evidence.excerpt) != evidence.digest
            {
                return Ok(None);
            }
        }
        eligible = true;
        user_authored = true;
    }
    if !eligible {
        return Ok(None);
    }
    let generation_allowed = if let Some(session_id) = &record.projection.session_id {
        connection
            .query_row(
                "SELECT COALESCE(
                   (SELECT generation='enabled' FROM session_memory_policy WHERE session_id=?1),1)",
                [session_id],
                |row| row.get(0),
            )
            .map_err(open::map_error)?
    } else {
        true
    };
    Ok(Some(MemoryEvidence {
        reference: reference_for(&record),
        record,
        generation_allowed,
        user_authored,
        raw_hints,
    }))
}

fn same_kind(stored: ExperienceEvidenceKind, source: LearningSourceKind) -> bool {
    matches!(
        (stored, source),
        (ExperienceEvidenceKind::Message, LearningSourceKind::Message)
            | (ExperienceEvidenceKind::Tool, LearningSourceKind::Tool)
            | (
                ExperienceEvidenceKind::Artifact,
                LearningSourceKind::Artifact
            )
            | (ExperienceEvidenceKind::User, LearningSourceKind::User)
            | (
                ExperienceEvidenceKind::Feedback,
                LearningSourceKind::Feedback
            )
    )
}

/// Usage counters and promotion bookkeeping are deliberately absent: reading or
/// applying memory must not make its own input look like new learning.
pub(crate) fn reference_for(record: &ExperienceRecord) -> MemoryEvidenceReference {
    let evidence = record
        .evidence
        .iter()
        .map(|item| {
            json!({
                "kind":item.kind.as_str(),"source":item.source_id,"digest":item.digest,
                "sourceDigest":item.source_digest,"verified":item.verified
            })
        })
        .collect::<Vec<_>>();
    let input = json!({
        "id":record.projection.id,"project":record.projection.project_id,
        "session":record.projection.session_id,"kind":record.projection.kind.as_str(),
        "fingerprint":record.fingerprint,"confidence":record.projection.confidence,
        "title":record.projection.title,"summary":record.projection.summary,
        "resolution":record.projection.resolution,
        "evidence":evidence,
    });
    MemoryEvidenceReference {
        experience_id: record.projection.id.clone(),
        digest: crate::learning_source::digest(&input.to_string()),
    }
}

/// Preserve the provenance of already accepted format-11 automatic memories.
/// Only exact linked contents are adopted; newer explicit edits remain user-owned.
pub(crate) fn backfill_provenance(connection: &Connection) -> Result<(), DbError> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT d.path,j.value,e.id,c.id
             FROM resident_memory_document d JOIN json_each(d.entries) j
             JOIN memory_candidate c ON c.content=j.value
               AND c.status='applied' AND c.source_kind='reflection'
               AND EXISTS(SELECT 1 FROM resident_memory_revision r
                 WHERE r.path=d.path AND r.candidate_id=c.id AND r.operation='apply')
             JOIN experience_record e ON e.promoted_memory_candidate_id=c.id
             WHERE NOT EXISTS (
               SELECT 1 FROM memory_candidate u WHERE
                 (u.target_path=d.path OR EXISTS(SELECT 1 FROM resident_memory_revision r
                   WHERE r.path=d.path AND r.candidate_id=u.id AND r.operation='apply'))
                 AND u.status='applied' AND u.source_kind IN ('tool','user')
                 AND u.content=j.value AND u.action IN ('add','replace')
                 AND COALESCE(u.time_applied,u.time_updated)>=COALESCE(c.time_applied,c.time_updated)
             ) ORDER BY d.path,j.value,e.id",
        )
        .map_err(open::map_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(open::map_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(open::map_error)?;
    let mut grouped = std::collections::BTreeMap::<
        (String, String),
        (Vec<MemoryEvidenceReference>, String),
    >::new();
    for (path, content, id, candidate_id) in rows {
        let record = crate::experience::read_required(connection, &id)?;
        let entry = grouped.entry((path, content)).or_default();
        entry.0.push(reference_for(&record));
        entry.1 = candidate_id;
    }
    for ((path, content), (references, candidate_id)) in grouped {
        connection
            .execute(
                "INSERT OR IGNORE INTO resident_memory_provenance
                 (path,content,evidence,candidate_id,time_updated) VALUES (?1,?2,?3,?4,0)",
                params![
                    path,
                    content,
                    serde_json::to_string(&references).map_err(query_error)?,
                    candidate_id
                ],
            )
            .map_err(open::map_error)?;
    }
    Ok(())
}

fn query_error(error: impl std::error::Error + Send + Sync + 'static) -> DbError {
    DbError::Query {
        source: Box::new(error),
    }
}
