//! Atomic resident-memory consolidation on the existing durable learning queue.

use crate::memory_candidate::{MemoryCandidateRecord, NewMemoryCandidate};
use crate::memory_evidence::MemoryEvidenceReference;
use crate::resident_memory::{
    ResidentMemoryAuthority, ResidentMemoryCommit, ResidentMemoryDocument, ResidentMemoryOperation,
    ResidentMemoryStore,
};
use crate::{Connection, Pool, open};
use rusqlite::{OptionalExtension as _, params};
use serde_json::json;
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{MemoryAction, MemoryCandidateStatus, MemoryScope, MemorySource};

pub const MEMORY_MAINTENANCE_PURPOSE: &str = "memory";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryMaintenanceState {
    pub input_digest: String,
    pub global_revision: i64,
    pub project_revision: i64,
    pub job_id: Option<String>,
}

pub struct MemoryBatchChange {
    pub candidate: NewMemoryCandidate,
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub apply: bool,
}

pub struct MemoryBatchCommit<'a> {
    pub project_id: &'a str,
    pub global_path: &'a str,
    pub project_path: &'a str,
    pub global_revision: i64,
    pub project_revision: i64,
    pub input_digest: &'a str,
    pub job_id: &'a str,
    pub lease: &'a crate::learning_job::LearningLease,
    pub evidence: &'a [MemoryEvidenceReference],
    pub changes: Vec<MemoryBatchChange>,
    pub now: i64,
}

pub struct MemoryBatchResult {
    pub candidates: Vec<MemoryCandidateRecord>,
    pub documents: Vec<ResidentMemoryDocument>,
}

pub struct MemorySourceRetraction {
    pub forgotten_experience_ids: Vec<String>,
    pub retractions: Vec<MemoryCandidateRecord>,
    pub rejected_candidate_ids: Vec<String>,
    pub documents: Vec<ResidentMemoryDocument>,
}

#[derive(Clone)]
pub struct MemoryMaintenanceStore {
    pool: Arc<Pool>,
}

impl MemoryMaintenanceStore {
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn content_retired(&self, path: &str, content: &str) -> Result<bool, DbError> {
        let connection = self.pool.get()?;
        retired_by_user(&connection, path, content)
    }

    /// Explicit source forgetting and deterministic retraction share a transaction.
    /// Never invert a replacement: that could resurrect an already corrected fact.
    pub fn forget_sources(
        &self,
        ids: &[String],
        paths: &[String],
        source_session: Option<&str>,
        now: i64,
    ) -> Result<MemorySourceRetraction, DbError> {
        self.pool.transaction(|transaction| {
            let forgotten = crate::experience::forget_many_on(transaction, ids, now)?;
            let mut retractions = Vec::new();
            let mut rejected = Vec::new();
            let mut documents = Vec::new();
            for path in paths {
                let mut statement = transaction.prepare(
                    "SELECT id FROM memory_candidate WHERE
                     (target_path=?1 OR id IN (SELECT promoted_memory_candidate_id
                       FROM experience_record WHERE id IN (SELECT value FROM json_each(?2))))
                     AND source_kind='reflection' AND status IN ('pending','failed')
                     AND (EXISTS (SELECT 1 FROM json_each(COALESCE(evidence,'[]')) e
                       JOIN json_each(?2) i ON json_extract(e.value,'$.experience_id')=i.value)
                       OR id IN (SELECT promoted_memory_candidate_id FROM experience_record
                         WHERE id IN (SELECT value FROM json_each(?2))))",
                ).map_err(open::map_error)?;
                let candidate_ids = statement.query_map(
                    params![path,json!(ids).to_string()], |row| row.get::<_,String>(0),
                ).map_err(open::map_error)?.collect::<Result<Vec<_>,_>>().map_err(open::map_error)?;
                drop(statement);
                for id in candidate_ids {
                    transaction.execute(
                        "UPDATE memory_candidate SET status='rejected',error='source evidence was forgotten',
                         time_updated=?2 WHERE id=?1", params![id,now],
                    ).map_err(open::map_error)?;
                    rejected.push(id);
                }
                let view = crate::resident_memory::view_on(transaction, path)?;
                for text in view.suppressed {
                    let current = crate::resident_memory::required(transaction, path)?;
                    let candidate = NewMemoryCandidate {
                        id:format!("mem_retract_{}",uuid::Uuid::now_v7().simple()),
                        target:current.scope,target_path:path.clone(),action:MemoryAction::Remove,
                        content:None,old_text:Some(text.clone()),
                        reason:"All supporting memory sources were invalidated.".to_owned(),
                        confidence:10_000,source:MemorySource::Reflection,
                        source_session_id:source_session.map(str::to_owned),source_message_id:None,
                        fingerprint:None,base_revision:Some(current.revision),evidence:Some(Vec::new()),
                        time_created:now,
                    };
                    let inserted = crate::memory_candidate::create_on(transaction,&candidate)?;
                    let after = current.entries.iter().filter(|entry| *entry != &text).cloned().collect::<Vec<_>>();
                    ResidentMemoryStore::commit_on(transaction,ResidentMemoryCommit {
                        path,scope:current.scope,expected_revision:current.revision,before:&current.entries,
                        after:&after,candidate_id:inserted.record.id(),operation:ResidentMemoryOperation::Apply,
                        now,authority:ResidentMemoryAuthority::Host,
                    })?;
                    retractions.push(crate::memory_candidate::read_required(transaction,inserted.record.id())?);
                }
                documents.push(crate::resident_memory::required(transaction,path)?);
            }
            Ok(MemorySourceRetraction {
                forgotten_experience_ids:forgotten,retractions,rejected_candidate_ids:rejected,documents,
            })
        })
    }

    pub fn state(
        &self,
        project_id: &str,
        project_path: &str,
    ) -> Result<Option<MemoryMaintenanceState>, DbError> {
        self.pool
            .get()?
            .query_row(
                "SELECT input_digest,global_revision,project_revision,job_id
                 FROM memory_maintenance_state WHERE project_id=?1 AND project_path=?2",
                params![project_id, project_path],
                |row| {
                    Ok(MemoryMaintenanceState {
                        input_digest: row.get(0)?,
                        global_revision: row.get(1)?,
                        project_revision: row.get(2)?,
                        job_id: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(open::map_error)
    }

    /// Candidate rows, all resident changes, source links, job completion and the
    /// no-op watermark either commit together or none of them do.
    pub fn commit(&self, input: MemoryBatchCommit<'_>) -> Result<MemoryBatchResult, DbError> {
        if input.changes.len() > 32 || input.input_digest.len() != 64 {
            return Err(conflict(input.job_id, "invalid bounded memory batch"));
        }
        self.pool.transaction(|transaction| {
            require_lease(transaction, &input)?;
            if !input.evidence.is_empty()
                && !crate::memory_evidence::references_current_on(
                    transaction,
                    input.evidence,
                    true,
                )?
            {
                return Err(conflict(
                    input.job_id,
                    "memory input sources or generation policy changed",
                ));
            }
            for (path, scope, revision) in [
                (input.global_path, MemoryScope::Global, input.global_revision),
                (input.project_path, MemoryScope::Project, input.project_revision),
            ] {
                let current = crate::resident_memory::required(transaction, path)?;
                if current.scope != scope || current.revision != revision {
                    return Err(conflict(input.job_id, "memory changed during consolidation"));
                }
            }

            let mut candidates = Vec::new();
            for change in &input.changes {
                let expected_path = match change.candidate.target {
                    MemoryScope::Global => input.global_path,
                    MemoryScope::Project => input.project_path,
                };
                if change.candidate.target_path != expected_path
                    || change.candidate.source != MemorySource::Reflection
                {
                    return Err(conflict(input.job_id, "memory batch escaped its owned scopes"));
                }
                require_exact_operation(change, input.job_id)?;
                if let Some(content) = change.candidate.content.as_deref()
                    && retired_by_user(transaction, expected_path, content)?
                {
                    return Err(conflict(input.job_id, "memory change resurrects explicitly retired content"));
                }
                let inserted = crate::memory_candidate::create_on(transaction, &change.candidate)?;
                if !inserted.inserted {
                    return Err(conflict(input.job_id, "memory batch candidate was already consumed"));
                }
                let retraction = require_owned_update(transaction, &input, change)?;
                if change.apply {
                    let current = crate::resident_memory::required(transaction, expected_path)?;
                    ResidentMemoryStore::commit_on(
                        transaction,
                        ResidentMemoryCommit {
                            path: expected_path,
                            scope: change.candidate.target,
                            expected_revision: current.revision,
                            before: &change.before,
                            after: &change.after,
                            candidate_id: inserted.record.id(),
                            operation: ResidentMemoryOperation::Apply,
                            now: input.now,
                            authority: if retraction {
                                // This is deterministic removal of unsupported
                                // derived data, not a new model-generated fact.
                                ResidentMemoryAuthority::Host
                            } else {
                                ResidentMemoryAuthority::Learning {
                                    job_id: input.job_id,
                                    lease: input.lease,
                                }
                            },
                        },
                    )?;
                    if let Some(evidence) = &change.candidate.evidence {
                        for source in evidence {
                            transaction
                                .execute(
                                    "UPDATE experience_record SET status='promoted',
                                       promoted_memory_candidate_id=?2,time_updated=?3
                                     WHERE id=?1 AND status IN ('active','promoted')",
                                    params![source.experience_id, inserted.record.id(), input.now],
                                )
                                .map_err(open::map_error)?;
                        }
                    }
                }
                candidates.push(crate::memory_candidate::read_required(
                    transaction,
                    inserted.record.id(),
                )?);
            }
            let global = crate::resident_memory::required(transaction, input.global_path)?;
            let project = crate::resident_memory::required(transaction, input.project_path)?;
            let result = json!({
                "purpose":MEMORY_MAINTENANCE_PURPOSE,"inputDigest":input.input_digest,
                "globalRevision":global.revision,"projectRevision":project.revision,
                "candidates":candidates.iter().map(|candidate| json!({
                    "id":candidate.id(),"status":candidate.projection.status.as_str(),
                    "scope":candidate.projection.scope.as_str(),
                    "action":candidate.projection.action.as_str(),
                })).collect::<Vec<_>>(),
                "applied":candidates.iter().filter(|candidate|
                    candidate.projection.status==MemoryCandidateStatus::Applied).count(),
            });
            let settled = transaction
                .execute(
                    "UPDATE learning_job SET status='completed',result=?4,error=NULL,
                       owner_id=NULL,lease_token=NULL,lease_expires=NULL,
                       time_updated=?5,time_completed=?5
                     WHERE id=?1 AND status='running' AND owner_id=?2 AND lease_token=?3",
                    params![input.job_id,input.lease.owner_id,input.lease.token,result.to_string(),input.now],
                )
                .map_err(open::map_error)?;
            if settled != 1 {
                return Err(conflict(input.job_id, "memory batch lost its lease"));
            }
            transaction
                .execute(
                    "INSERT INTO memory_maintenance_state
                     (project_id,project_path,input_digest,global_revision,project_revision,job_id,time_updated)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)
                     ON CONFLICT(project_id,project_path) DO UPDATE SET
                       input_digest=excluded.input_digest,global_revision=excluded.global_revision,
                       project_revision=excluded.project_revision,job_id=excluded.job_id,
                       time_updated=excluded.time_updated",
                    params![input.project_id,input.project_path,input.input_digest,
                        global.revision,project.revision,input.job_id,input.now],
                )
                .map_err(open::map_error)?;
            Ok(MemoryBatchResult {
                candidates,
                documents: vec![global, project],
            })
        })
    }
}

fn require_exact_operation(change: &MemoryBatchChange, job_id: &str) -> Result<(), DbError> {
    let mut expected = change.before.clone();
    match change.candidate.action {
        MemoryAction::Add => {
            let content = change
                .candidate
                .content
                .as_ref()
                .ok_or_else(|| conflict(job_id, "add needs content"))?;
            if !expected.contains(content) {
                expected.push(content.clone());
            }
        }
        MemoryAction::Replace | MemoryAction::Remove => {
            let index = expected
                .iter()
                .position(|entry| Some(entry) == change.candidate.old_text.as_ref())
                .ok_or_else(|| conflict(job_id, "change needs an exact existing entry"))?;
            if change.candidate.action == MemoryAction::Remove {
                expected.remove(index);
            } else {
                expected[index] = change
                    .candidate
                    .content
                    .clone()
                    .ok_or_else(|| conflict(job_id, "replace needs content"))?;
            }
        }
    }
    if expected != change.after {
        return Err(conflict(
            job_id,
            "memory batch snapshots do not match its operation",
        ));
    }
    Ok(())
}

/// Prompt reminders are not sufficient to enforce an explicit forget/undo.
/// The last direct decision about these exact bytes wins, including reaffirmation.
pub(crate) fn retired_by_user(
    connection: &Connection,
    path: &str,
    content: &str,
) -> Result<bool, DbError> {
    let decision: Option<(String, String, String)> = connection
        .query_row(
            "SELECT status,before_entries,after_entries FROM memory_candidate
         WHERE (target_path=?1 OR id IN (
           SELECT candidate_id FROM resident_memory_revision WHERE path=?1))
           AND ((status='applied' AND source_kind<>'reflection') OR status='undone')
           AND (EXISTS(SELECT 1 FROM json_each(before_entries) WHERE value=?2)
             OR EXISTS(SELECT 1 FROM json_each(after_entries) WHERE value=?2))
         ORDER BY time_updated DESC,rowid DESC LIMIT 1",
            params![path, content],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(open::map_error)?;
    if let Some((status, before, after)) = decision {
        let kept: Vec<String> =
            serde_json::from_str(if status == "undone" { &before } else { &after }).map_err(
                |error| DbError::Query {
                    source: Box::new(error),
                },
            )?;
        return Ok(!kept.iter().any(|entry| entry == content));
    }
    Ok(false)
}

fn require_lease(connection: &Connection, input: &MemoryBatchCommit<'_>) -> Result<(), DbError> {
    let valid: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM learning_job WHERE id=?1 AND project_id=?2
               AND kind='project_aggregation' AND status='running'
               AND owner_id=?3 AND lease_token=?4 AND lease_expires>?5
               AND json_extract(payload,'$.purpose')='memory'
               AND json_extract(payload,'$.projectPath')=?6
               AND json_extract(payload,'$.inputDigest')=?7
               AND json_extract(payload,'$.globalRevision')=?8
               AND json_extract(payload,'$.projectRevision')=?9)",
            params![
                input.job_id,
                input.project_id,
                input.lease.owner_id,
                input.lease.token,
                input.now,
                input.project_path,
                input.input_digest,
                input.global_revision,
                input.project_revision
            ],
            |row| row.get(0),
        )
        .map_err(open::map_error)?;
    if !valid {
        return Err(conflict(
            input.job_id,
            "memory batch lacks its exact input lease",
        ));
    }
    Ok(())
}

/// Return true only for deterministic retraction of an entry with no live source.
fn require_owned_update(
    connection: &Connection,
    input: &MemoryBatchCommit<'_>,
    change: &MemoryBatchChange,
) -> Result<bool, DbError> {
    let candidate = &change.candidate;
    let references = candidate.evidence.as_deref().unwrap_or_default();
    if references
        .iter()
        .any(|reference| !input.evidence.contains(reference))
    {
        return Err(conflict(input.job_id, "memory change invented evidence"));
    }
    if matches!(
        candidate.action,
        MemoryAction::Replace | MemoryAction::Remove
    ) {
        let old = candidate
            .old_text
            .as_deref()
            .ok_or_else(|| conflict(input.job_id, "memory replacement has no exact entry"))?;
        let current = crate::resident_memory::view_on(connection, &candidate.target_path)?;
        if !current.managed.contains_key(old) || !change.before.iter().any(|entry| entry == old) {
            return Err(conflict(
                input.job_id,
                "automatic memory cannot overwrite user-owned entries",
            ));
        }
        if candidate.action == MemoryAction::Remove
            && references.is_empty()
            && current.suppressed.iter().any(|entry| entry == old)
        {
            return Ok(true);
        }
    }
    if references.is_empty()
        || !crate::memory_evidence::references_current_on(connection, references, true)?
    {
        return Err(conflict(
            input.job_id,
            "memory change has no current verified evidence",
        ));
    }
    if candidate.target == MemoryScope::Global {
        for reference in references {
            if !crate::memory_evidence::collect_on(connection, &reference.experience_id)?
                .is_some_and(|source| source.user_authored)
            {
                return Err(conflict(
                    input.job_id,
                    "global memory needs explicit user evidence",
                ));
            }
        }
    }
    Ok(false)
}

fn conflict(id: &str, detail: &str) -> DbError {
    DbError::Conflict {
        table: "memory_maintenance_state".to_owned(),
        id: id.to_owned(),
        detail: detail.to_owned(),
    }
}
