use super::*;
use zuno_db::{
    memory_candidate::{
        MemoryCandidateEdit, MemoryCandidateInsert, MemoryCandidateRecord, NewMemoryCandidate,
    },
    memory_maintenance::{
        MemoryBatchCommit, MemoryBatchResult, MemoryMaintenanceState, MemorySourceRetraction,
    },
    resident_memory::{ResidentMemoryCommit, ResidentMemoryDocument, ResidentMemoryView},
};
use zuno_types::MemoryCandidateStatus;

impl MemoryPersistence for TransactionMemory {
    fn create_candidate(
        &self,
        candidate: NewMemoryCandidate,
    ) -> Result<MemoryCandidateInsert, Error> {
        self.execute(async |tx| self.insert_candidate(tx, candidate).await)
    }
    fn create_model_candidate(
        &self,
        candidate: NewMemoryCandidate,
        session: &str,
    ) -> Result<MemoryCandidateInsert, Error> {
        self.execute(async |tx| {
            self.require_generation(tx, Some(session)).await?;
            if candidate.source_session_id.as_deref() != Some(session) {
                return Err(Error::Denied);
            }
            self.insert_candidate(tx, candidate).await
        })
    }
    fn candidate(&self, id: &str) -> Result<MemoryCandidateRecord, Error> {
        self.execute(async |tx| self.load_candidate(tx, id).await)
    }
    fn candidates_for_paths(
        &self,
        global: &str,
        project: &str,
    ) -> Result<Vec<MemoryCandidateRecord>, Error> {
        if global != "global" || project != self.project_key {
            return Err(Error::Denied);
        }
        self.execute(async |tx| self.list_candidates(tx).await)
    }
    fn edit_candidate(
        &self,
        input: MemoryCandidateEdit<'_>,
    ) -> Result<MemoryCandidateRecord, Error> {
        self.host_only()?;
        self.execute(async |tx| {
            let mut candidate = self.load_candidate(tx, input.id).await?;
            if !matches!(
                candidate.projection.status,
                MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
            ) {
                return Err(Error::Conflict);
            }
            candidate.projection.content = input.content.map(str::to_owned);
            candidate.projection.old_text = input.old_text.map(str::to_owned);
            candidate.projection.reason = input.reason.to_owned();
            candidate.projection.confidence = input.confidence;
            candidate.projection.source = MemorySource::User;
            candidate.projection.error = None;
            candidate.projection.time_updated = database_time(tx).await.map_err(app_error)?;
            candidate.fingerprint = None;
            candidate.evidence = None;
            candidate.base_revision = Some(input.base_revision);
            self.save_candidate(tx, &candidate).await?;
            Ok(candidate)
        })
    }
    fn set_candidate_status(
        &self,
        id: &str,
        status: MemoryCandidateStatus,
        error: Option<&str>,
        _at: i64,
    ) -> Result<MemoryCandidateRecord, Error> {
        self.execute(async |tx| {
            let mut candidate = self.load_candidate(tx, id).await?;
            use MemoryCandidateStatus as S;
            let allowed = match status {
                S::Applied | S::Uncertain => {
                    matches!(candidate.projection.status, S::Applying | S::Undoing)
                }
                S::Undone => candidate.projection.status == S::Undoing,
                S::Failed => matches!(
                    candidate.projection.status,
                    S::Pending | S::Failed | S::Applying
                ),
                S::Rejected => matches!(candidate.projection.status, S::Pending | S::Failed),
                _ => false,
            };
            if !allowed {
                return Err(Error::Conflict);
            }
            candidate.projection.status = status;
            candidate.projection.error = error.map(str::to_owned);
            candidate.projection.time_updated = database_time(tx).await.map_err(app_error)?;
            self.save_candidate(tx, &candidate).await?;
            Ok(candidate)
        })
    }
    fn document(&self, key: &str) -> Result<Option<ResidentMemoryDocument>, Error> {
        self.execute(async |tx| self.load_document(tx, key).await)
    }
    fn views(&self, keys: &[String]) -> Result<Vec<ResidentMemoryView>, Error> {
        if keys.len() > 2 {
            return Err(Error::Denied);
        }
        self.execute(async |tx| {
            let mut views = Vec::new();
            for key in keys {
                views.push(self.view(tx, key).await?);
            }
            Ok(views)
        })
    }
    fn require_model_use(&self, session: &str) -> Result<(), Error> {
        self.execute(async |tx| {
            if self.use_enabled(tx, Some(session)).await? {
                Ok(())
            } else {
                Err(Error::Denied)
            }
        })
    }
    fn adopt(
        &self,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
        _at: i64,
    ) -> Result<ResidentMemoryDocument, Error> {
        self.execute(async |tx| self.adopt_document(tx, key, scope, entries).await)
    }
    fn commit_document(
        &self,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, Error> {
        self.execute(async |tx| self.commit(tx, input).await)
    }
    fn revision_entries(&self, key: &str, revision: i64) -> Result<Vec<String>, Error> {
        self.key(key)?;
        self.execute(async |tx| {
            let entries: Value = query_scalar("SELECT entries FROM zuno_enterprise_preview.memory_revision WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND revision=$4")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(key).bind(revision)
                .fetch_one(&mut **tx).await.map_err(sql_error)?;
            serde_json::from_value(entries).map_err(decode_error)
        })
    }
    fn record_projection(
        &self,
        _key: &str,
        _revision: i64,
        _error: Option<&str>,
    ) -> Result<bool, Error> {
        Err(Error::Denied)
    }
    fn import_projection(
        &self,
        _key: &str,
        _scope: MemoryScope,
        _entries: &[String],
        _at: i64,
    ) -> Result<ResidentMemoryDocument, Error> {
        Err(Error::Denied)
    }
    fn content_retired(&self, key: &str, content: &str) -> Result<bool, Error> {
        self.execute(async |tx| self.retired(tx, key, content).await)
    }
    fn maintenance_state(
        &self,
        project: &str,
        key: &str,
    ) -> Result<Option<MemoryMaintenanceState>, Error> {
        if project != self.workspace.as_str() || key != self.project_key {
            return Err(Error::Denied);
        }
        self.execute(async |tx| {
            let row = query("SELECT input_digest,global_revision,project_revision,job_id FROM zuno_enterprise_preview.memory_maintenance_state
                WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3 AND key=$4")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(project).bind(key)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?;
            row.map(|row| Ok(MemoryMaintenanceState {
                input_digest: row.try_get("input_digest").map_err(sql_error)?,
                global_revision: row.try_get("global_revision").map_err(sql_error)?,
                project_revision: row.try_get("project_revision").map_err(sql_error)?,
                job_id: Some(row.try_get("job_id").map_err(sql_error)?),
            })).transpose()
        })
    }
    fn commit_maintenance(&self, input: MemoryBatchCommit<'_>) -> Result<MemoryBatchResult, Error> {
        self.execute(async |tx| self.maintenance(tx, input).await)
    }
    fn forget_sources(
        &self,
        ids: &[String],
        keys: &[String],
        session: Option<&str>,
        _at: i64,
    ) -> Result<MemorySourceRetraction, Error> {
        self.host_only()?;
        self.execute(async |tx| self.forget(tx, ids, keys, session).await)
    }
}
