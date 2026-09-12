use super::*;
use zuno_db::memory_candidate::{MemoryCandidateInsert, MemoryCandidateRecord, NewMemoryCandidate};
use zuno_types::{MemoryCandidateProjection, MemoryCandidateStatus};

impl TransactionMemory {
    pub(super) async fn load_candidate(
        &self,
        tx: &mut Tx,
        id: &str,
    ) -> Result<MemoryCandidateRecord, Error> {
        let row = query(
            "SELECT key,status,data FROM zuno_enterprise_preview.memory_candidate
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND key IN ('global',$4)",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(id)
        .bind(&self.project_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(sql_error)?
        .ok_or(Error::Denied)?;
        let record: MemoryCandidateRecord =
            serde_json::from_value(row.try_get("data").map_err(sql_error)?)
                .map_err(decode_error)?;
        if record.id() != id
            || record.target_path != row.try_get::<String, _>("key").map_err(sql_error)?
            || record.projection.status.as_str()
                != row.try_get::<String, _>("status").map_err(sql_error)?
            || self.key(&record.target_path)? != record.projection.scope
            || record.projection.confidence > 10_000
            || record.base_revision.is_some_and(|revision| revision < 1)
        {
            return Err(Error::InvalidData);
        }
        Ok(record)
    }

    pub(super) async fn insert_candidate(
        &self,
        tx: &mut Tx,
        mut input: NewMemoryCandidate,
    ) -> Result<MemoryCandidateInsert, Error> {
        if self.key(&input.target_path)? != input.target
            || input.id.is_empty()
            || input.id.len() > 256
            || input.confidence > 10_000
            || input.base_revision.is_some_and(|revision| revision < 1)
        {
            return Err(invalid("invalid Memory candidate"));
        }
        if let Some(session) = &input.source_session_id {
            self.ensure_session(tx, session).await?;
        }
        if let Some(session) = self.lease.as_ref().map(|lease| lease.session_id.as_str()) {
            self.require_generation(tx, Some(session)).await?;
            if input.source_session_id.as_deref() != Some(session)
                || input.source != MemorySource::Tool
            {
                return Err(Error::Denied);
            }
        }
        if let Some(fingerprint) = input.fingerprint.as_ref() {
            if input.source != MemorySource::Reflection
                || input.source_session_id.is_none()
                || input.source_message_id.is_none()
            {
                return Err(invalid(
                    "Memory fingerprint needs exact reflection input identity",
                ));
            }
            let prior: Option<String> = query_scalar(
                "SELECT id FROM zuno_enterprise_preview.memory_candidate WHERE tenant_id=$1 AND principal_id=$2
                  AND key=$3 AND source_session_id=$4 AND source_message_id=$5 AND fingerprint=$6",
            ).bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(&input.target_path)
                .bind(&input.source_session_id).bind(&input.source_message_id).bind(fingerprint)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?;
            if let Some(id) = prior {
                return Ok(MemoryCandidateInsert {
                    record: self.load_candidate(tx, &id).await?,
                    inserted: false,
                });
            }
        }
        input.time_created = database_time(tx).await.map_err(app_error)?;
        let record = MemoryCandidateRecord {
            projection: MemoryCandidateProjection {
                id: input.id,
                scope: input.target,
                action: input.action,
                content: input.content,
                old_text: input.old_text,
                reason: input.reason,
                confidence: input.confidence,
                source: input.source,
                source_session_id: input.source_session_id,
                source_message_id: input.source_message_id,
                status: MemoryCandidateStatus::Pending,
                error: None,
                time_created: input.time_created,
                time_updated: input.time_created,
            },
            target_path: input.target_path,
            fingerprint: input.fingerprint,
            before_entries: None,
            after_entries: None,
            time_applied: None,
            base_revision: input.base_revision,
            evidence: input.evidence,
        };
        query("INSERT INTO zuno_enterprise_preview.memory_candidate(tenant_id,principal_id,id,key,status,source_session_id,source_message_id,fingerprint,data)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
            .bind(record.id()).bind(&record.target_path).bind(record.projection.status.as_str())
            .bind(&record.projection.source_session_id).bind(&record.projection.source_message_id).bind(&record.fingerprint)
            .bind(json!(record)).execute(&mut **tx).await.map_err(sql_error)?;
        Ok(MemoryCandidateInsert {
            record,
            inserted: true,
        })
    }

    pub(super) async fn save_candidate(
        &self,
        tx: &mut Tx,
        record: &MemoryCandidateRecord,
    ) -> Result<(), Error> {
        self.key(&record.target_path)?;
        let changed = query(
            "UPDATE zuno_enterprise_preview.memory_candidate SET status=$4,fingerprint=$5,data=$6
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND key=$7",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(record.id())
        .bind(record.projection.status.as_str())
        .bind(&record.fingerprint)
        .bind(json!(record))
        .bind(&record.target_path)
        .execute(&mut **tx)
        .await
        .map_err(sql_error)?
        .rows_affected();
        if changed != 1 {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    pub(super) async fn list_candidates(
        &self,
        tx: &mut Tx,
    ) -> Result<Vec<MemoryCandidateRecord>, Error> {
        let ids: Vec<String> = query_scalar("SELECT id FROM zuno_enterprise_preview.memory_candidate
            WHERE tenant_id=$1 AND principal_id=$2 AND key IN ('global',$3)
            ORDER BY CASE status WHEN 'pending' THEN 0 WHEN 'applying' THEN 1 WHEN 'undoing' THEN 2
              WHEN 'uncertain' THEN 3 ELSE 4 END, (data->'projection'->>'timeCreated')::bigint DESC,id DESC LIMIT 512")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(&self.project_key)
            .fetch_all(&mut **tx).await.map_err(sql_error)?;
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            records.push(self.load_candidate(tx, &id).await?);
        }
        Ok(records)
    }
}
