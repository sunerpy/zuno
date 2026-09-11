use super::{COLUMNS, LearningJobRecord, LearningJobStore, decode, decode_row, read_required};
use crate::{event_log::query_error, open};
use rusqlite::{OptionalExtension as _, params};
use serde_json::json;
use zuno_error::DbError;

pub const MAX_LEARNING_HISTORY_BATCH: usize = 32;

#[derive(Debug, Clone)]
pub struct LearningHistoryBatch {
    pub jobs: Vec<LearningJobRecord>,
    pub has_more: bool,
}

impl LearningJobStore {
    /// One bounded repair batch. A recorded outcome prevents repeated automatic
    /// repair of an unavailable source or repeatedly retrying a paid failure.
    pub fn legacy_for_project(
        &self,
        project_id: &str,
        current_version: &str,
        limit: usize,
    ) -> Result<LearningHistoryBatch, DbError> {
        let limit = limit.clamp(1, MAX_LEARNING_HISTORY_BATCH);
        let connection = self.pool.get()?;
        let mut query = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM learning_job
             WHERE project_id=?1 AND kind='extraction'
               AND status IN ('queued','completed','skipped','failed')
               AND (extractor_version<>?2 OR json_extract(payload,'$.sourceSnapshot.version') IS NULL)
               AND COALESCE(json_extract(payload,'$.historyRepair.version'),'')<>?2
               AND COALESCE(length(CAST(payload AS BLOB)),0)<=2097152
               AND COALESCE(length(CAST(result AS BLOB)),0)<=2097152
               AND NOT EXISTS(SELECT 1 FROM session_memory_policy p
                 WHERE p.session_id=learning_job.session_id AND p.generation<>'enabled')
               AND NOT EXISTS(SELECT 1 FROM experience_record e
                 WHERE e.session_id=learning_job.session_id
                   AND e.source_message_id=learning_job.source_message_id AND e.status='forgotten')
             ORDER BY time_created DESC,id DESC LIMIT ?3"
        )).map_err(open::map_error)?;
        let bound = i64::try_from(limit + 1).expect("history batch is bounded");
        let mut jobs = query
            .query_map(params![project_id, current_version, bound], decode_row)
            .map_err(open::map_error)?
            .map(|row| row.map_err(open::map_error).and_then(decode))
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = jobs.len() > limit;
        jobs.truncate(limit);
        Ok(LearningHistoryBatch { jobs, has_more })
    }

    pub fn extraction_for_source(
        &self,
        project_id: &str,
        message_id: &str,
        extractor_version: &str,
    ) -> Result<Option<LearningJobRecord>, DbError> {
        self.pool.get()?.query_row(
            &format!("SELECT {COLUMNS} FROM learning_job
             WHERE project_id=?1 AND source_message_id=?2 AND kind='extraction'
               AND (extractor_version=?3 OR status='running')
               AND COALESCE(json_extract(payload,'$.trigger'),'automatic_post_turn')<>'manual'
             ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END,time_created DESC,id DESC LIMIT 1"),
            params![project_id,message_id,extractor_version],decode_row,
        ).optional().map_err(open::map_error)?.map(decode).transpose()
    }

    /// Compare-and-set repair bookkeeping without rewriting the original request
    /// or resetting any attempt count. Running and uncertain jobs are immutable.
    pub fn record_history_repair(
        &self,
        job_id: &str,
        expected_updated_at: i64,
        version: &str,
        outcome: &str,
        now: i64,
    ) -> Result<(), DbError> {
        if version.is_empty() || version.len() > 256 || outcome.len() > 128 {
            return Err(query_error(std::io::Error::other(
                "invalid history repair marker",
            )));
        }
        self.pool.transaction(|transaction| {
            let job=read_required(transaction,job_id)?;
            let mut payload=job.payload.unwrap_or_else(||json!({}));
            if !payload.is_object() {payload=json!({"legacyPayload":payload});}
            payload["historyRepair"]=json!({"version":version,"outcome":outcome,"time":now});
            let changed=transaction.execute(
                "UPDATE learning_job SET payload=?3,time_updated=?4
                 WHERE id=?1 AND time_updated=?2 AND status IN ('queued','completed','skipped','failed')",
                params![job_id,expected_updated_at,payload.to_string(),now],
            ).map_err(open::map_error)?;
            if changed!=1 {
                return Err(DbError::Conflict {table:"learning_job".to_owned(),id:job_id.to_owned(),
                    detail:"history repair lost its source revision".to_owned()});
            }
            Ok(())
        })
    }

    pub fn generation_for_session(
        &self,
        session_id: &str,
    ) -> Result<zuno_types::SessionMemoryGeneration, DbError> {
        let generation = self
            .pool
            .get()?
            .query_row(
                "SELECT generation FROM session_memory_policy WHERE session_id=?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(open::map_error)?;
        generation
            .as_deref()
            .map(|generation| {
                zuno_types::SessionMemoryGeneration::parse(generation).ok_or_else(|| {
                    query_error(std::io::Error::other("invalid session memory policy"))
                })
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub fn source_was_forgotten(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<bool, DbError> {
        self.pool
            .get()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM experience_record
             WHERE session_id=?1 AND source_message_id=?2 AND status='forgotten')",
                params![session_id, message_id],
                |row| row.get(0),
            )
            .map_err(open::map_error)
    }
}
