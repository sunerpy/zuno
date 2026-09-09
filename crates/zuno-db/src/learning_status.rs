//! Read projections and explicit accounting of selected experience context.

use crate::{
    Pool,
    event_log::{NewSessionEvent, append_in, query_error},
    open,
};
use rusqlite::{OptionalExtension as _, params};
use std::sync::Arc;
use zuno_error::DbError;
use zuno_types::{LearningJobProjection, LearningQueueProjection, LearningRetrievalProjection};

#[derive(Clone)]
pub struct LearningStatusStore {
    pool: Arc<Pool>,
}

impl LearningStatusStore {
    pub fn new(pool: Arc<Pool>) -> Self {
        Self { pool }
    }

    pub fn usage(
        &self,
        project_id: &str,
        experience_id: &str,
    ) -> Result<(u64, Option<i64>), DbError> {
        let (count,last)=self.pool.get()?.query_row(
            "SELECT use_count,last_used_at FROM experience_record WHERE project_id=?1 AND id=?2",
            params![project_id,experience_id],|row|Ok((row.get::<_,i64>(0)?,row.get(1)?)),
        ).map_err(open::map_error)?;
        Ok((u64::try_from(count).map_err(query_error)?, last))
    }

    pub fn queue(&self, project_id: &str) -> Result<LearningQueueProjection, DbError> {
        let connection = self.pool.get()?;
        let mut result = connection
            .query_row(
                "SELECT count(*) FILTER (WHERE status='queued'),
                count(*) FILTER (WHERE status='running'),
                count(*) FILTER (WHERE status='failed'),
                count(*) FILTER (WHERE status='uncertain'),
                min(scheduled_at) FILTER (WHERE status='queued')
             FROM learning_job WHERE project_id=?1 OR kind='global_aggregation'",
                [project_id],
                |row| {
                    Ok(LearningQueueProjection {
                        queued: row.get::<_, i64>(0)?.unsigned_abs(),
                        running: row.get::<_, i64>(1)?.unsigned_abs(),
                        failed: row.get::<_, i64>(2)?.unsigned_abs(),
                        uncertain: row.get::<_, i64>(3)?.unsigned_abs(),
                        next_due_at: row.get(4)?,
                        jobs: Vec::new(),
                    })
                },
            )
            .map_err(open::map_error)?;
        let mut query=connection.prepare(
            "SELECT id,kind,status,session_id,attempt,scheduled_at,substr(error,1,4096)
             FROM learning_job WHERE project_id=?1 OR kind='global_aggregation'
             ORDER BY CASE status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 WHEN 'failed' THEN 2 ELSE 3 END,
                time_updated DESC,id DESC LIMIT 20"
        ).map_err(open::map_error)?;
        result.jobs = query
            .query_map([project_id], |row| {
                Ok(LearningJobProjection {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    status: row.get(2)?,
                    session_id: row.get(3)?,
                    attempt: row.get(4)?,
                    scheduled_at: row.get(5)?,
                    error: row.get(6)?,
                })
            })
            .map_err(open::map_error)?
            .collect::<Result<_, _>>()
            .map_err(open::map_error)?;
        Ok(result)
    }

    pub fn retrieval(
        &self,
        session_id: &str,
    ) -> Result<Option<LearningRetrievalProjection>, DbError> {
        let row = self.pool.get()?.query_row(
            "SELECT query_digest,selected_ids,candidate_count,estimated_tokens,reason,time_updated
             FROM learning_retrieval_snapshot WHERE session_id=?1", [session_id], |row|Ok((
                row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,u32>(2)?,
                row.get::<_,u32>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,i64>(5)?
             ))
        ).optional().map_err(open::map_error)?;
        row.map(|row| {
            Ok(LearningRetrievalProjection {
                query_digest: row.0,
                selected_ids: serde_json::from_str(&row.1).map_err(query_error)?,
                candidate_count: row.2,
                estimated_tokens: row.3,
                reason: row.4,
                time_updated: row.5,
            })
        })
        .transpose()
    }

    pub fn record_retrieval(
        &self,
        session_id: &str,
        project_id: &str,
        projection: &LearningRetrievalProjection,
    ) -> Result<(), DbError> {
        let ids = serde_json::to_string(&projection.selected_ids).map_err(query_error)?;
        self.pool.transaction(|transaction| {
            let same_project: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM session WHERE id=?1 AND project_id=?2)",
                params![session_id,project_id],|row|row.get(0),
            ).map_err(open::map_error)?;
            if !same_project {return Err(query_error(std::io::Error::other("retrieval project mismatch"))); }
            transaction.execute(
                "INSERT INTO learning_retrieval_snapshot
                 (session_id,query_digest,selected_ids,candidate_count,estimated_tokens,reason,time_updated)
                 VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id) DO UPDATE SET
                   query_digest=excluded.query_digest,selected_ids=excluded.selected_ids,
                   candidate_count=excluded.candidate_count,estimated_tokens=excluded.estimated_tokens,
                   reason=excluded.reason,time_updated=excluded.time_updated",
                params![session_id,projection.query_digest,ids,projection.candidate_count,
                    projection.estimated_tokens,projection.reason,projection.time_updated],
            ).map_err(open::map_error)?;
            transaction.execute(
                "UPDATE experience_record SET last_used_at=?3,use_count=use_count+1
                 WHERE project_id=?1 AND status<>'forgotten'
                   AND id IN (SELECT value FROM json_each(?2))",
                params![project_id,ids,projection.time_updated],
            ).map_err(open::map_error)?;
            let properties=serde_json::to_value(projection).map_err(query_error)?
                .as_object().cloned().expect("retrieval projection object");
            append_in(transaction,session_id,NewSessionEvent::new("learning.retrieval.selected",properties)?)?;
            Ok(())
        })
    }
}
