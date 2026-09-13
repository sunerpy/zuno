//! Bounded completed-descendant evidence. Delegated prompts and Agent report
//! prose are never substituted for authenticated successful operation receipts.
use super::*;
use std::collections::{BTreeSet, VecDeque};
use zuno_application::runtime::RuntimeJob;

const MAX_SOURCE_JOBS: usize = 256;

impl TransactionMemory {
    pub(in crate::memory) async fn completed_source_jobs(
        &self,
        tx: &mut Tx,
        root: &RuntimeJob,
    ) -> Result<(Vec<String>, bool), Error> {
        let current = crate::runtime::read_job(tx, &self.principal.owner(), root.id.as_str())
            .await
            .map_err(app_error)?;
        if current.phase != zuno_application::runtime::JobPhase::Completed
            || current.session_id != root.session_id
        {
            return Err(Error::Denied);
        }
        let mut jobs = vec![root.id.to_string()];
        let mut queue = VecDeque::from([(root.id.to_string(), root.session_id.to_string(), 0)]);
        let mut seen = BTreeSet::from([root.id.to_string()]);
        let mut truncated = false;
        while let Some((parent, session, depth)) = queue.pop_front() {
            let available = MAX_SOURCE_JOBS.saturating_sub(jobs.len());
            let rows = query(
                "SELECT c.job_id,c.child_session_id,c.completion,c.completion_digest
                 FROM zuno_enterprise_preview.runtime_child c
                 JOIN zuno_enterprise_preview.runtime_job r
                   ON r.tenant_id=c.tenant_id AND r.principal_id=c.principal_id
                     AND r.job_id=c.job_id AND r.session_id=c.child_session_id
                 JOIN zuno_enterprise_preview.session s
                   ON s.tenant_id=r.tenant_id AND s.principal_id=r.principal_id AND s.id=r.session_id
                 WHERE c.tenant_id=$1 AND c.principal_id=$2 AND c.parent_job_id=$3 AND c.parent_session_id=$4
                   AND c.state IN('completed','consumed') AND r.phase='completed'
                   AND s.workspace_id=$5 AND s.parent_id=$4
                 ORDER BY c.job_id LIMIT $6",
            )
            .bind(self.principal.tenant_id().as_str())
            .bind(self.principal.principal_id().as_str())
            .bind(parent).bind(&session).bind(self.workspace.as_str())
            .bind((available + 1) as i64)
            .fetch_all(&mut **tx).await.map_err(sql_error)?;
            truncated |= rows.len() > available;
            if depth >= 16 && !rows.is_empty() {
                return Err(Error::InvalidData);
            }
            for row in rows.into_iter().take(available) {
                let id: String = row.try_get("job_id").map_err(sql_error)?;
                let child: String = row.try_get("child_session_id").map_err(sql_error)?;
                if !seen.insert(id.clone()) {
                    return Err(Error::InvalidData);
                }
                let completion: Value = row.try_get("completion").map_err(sql_error)?;
                let digest: String = row.try_get("completion_digest").map_err(sql_error)?;
                let envelope: zuno_types::execution::CompletionEnvelope =
                    serde_json::from_value(completion.clone()).map_err(decode_error)?;
                if digest != zuno_orchestration::sha256_json(&completion)
                    || envelope.source != zuno_types::execution::CompletionSource::AgentJob
                    || envelope.parent_session_id != session
                    || envelope.payload["jobId"] != id
                    || envelope.payload["status"] != "completed"
                {
                    return Err(Error::InvalidData);
                }
                match self.require_automation(tx, Some(&child)).await {
                    Ok(()) => {}
                    Err(Error::Denied) => continue,
                    Err(error) => return Err(error),
                }
                jobs.push(id.clone());
                queue.push_back((id, child, depth + 1));
            }
            if jobs.len() == MAX_SOURCE_JOBS {
                truncated |= !queue.is_empty();
                break;
            }
        }
        Ok((jobs, truncated))
    }

    pub(super) async fn extraction_sources_current(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
    ) -> Result<bool, Error> {
        if !matches!(job.input, LearningInput::Extraction(_)) {
            return Ok(true);
        }
        let row = self.execution_row(tx, &job.id).await?;
        let context: ExtractionContext =
            serde_json::from_value(row.try_get("context").map_err(sql_error)?)
                .map_err(decode_error)?;
        for frozen in context.sources {
            let Some(source) = self
                .source(tx, &frozen.origin, self.workspace.as_str())
                .await?
            else {
                return Ok(false);
            };
            if source.digest != frozen.source_digest {
                return Ok(false);
            }
            self.require_automation(tx, Some(&source.session)).await?;
        }
        Ok(true)
    }
}
