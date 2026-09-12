use std::collections::BTreeMap;

use super::*;
use zuno_db::{
    memory_candidate::MemoryCandidateRecord,
    memory_evidence::MemoryEvidenceReference,
    resident_memory::{
        ResidentMemoryAuthority, ResidentMemoryCommit, ResidentMemoryDocument,
        ResidentMemoryOperation, ResidentMemoryView,
    },
};
use zuno_types::{MemoryAction, MemoryCandidateStatus};

impl TransactionMemory {
    pub(super) async fn load_document(
        &self,
        tx: &mut Tx,
        key: &str,
    ) -> Result<Option<ResidentMemoryDocument>, Error> {
        let scope = self.key(key)?;
        let row = query("SELECT revision,scope,data FROM zuno_enterprise_preview.memory_document WHERE tenant_id=$1 AND principal_id=$2 AND key=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(key)
            .fetch_optional(&mut **tx).await.map_err(sql_error)?;
        row.map(|row| {
            let document: ResidentMemoryDocument =
                serde_json::from_value(row.try_get("data").map_err(sql_error)?)
                    .map_err(decode_error)?;
            if document.path != key
                || document.scope != scope
                || document.revision < 1
                || document.revision != row.try_get::<i64, _>("revision").map_err(sql_error)?
                || document.scope.as_str()
                    != row.try_get::<String, _>("scope").map_err(sql_error)?
                || document.content_digest
                    != zuno_orchestration::sha256_json(&json!(document.entries))
            {
                return Err(Error::InvalidData);
            }
            Ok(document)
        })
        .transpose()
    }

    pub(super) async fn required_document(
        &self,
        tx: &mut Tx,
        key: &str,
    ) -> Result<ResidentMemoryDocument, Error> {
        self.load_document(tx, key).await?.ok_or(Error::Conflict)
    }

    pub(super) async fn adopt_document(
        &self,
        tx: &mut Tx,
        key: &str,
        scope: MemoryScope,
        entries: &[String],
    ) -> Result<ResidentMemoryDocument, Error> {
        if self.key(key)? != scope {
            return Err(Error::Denied);
        }
        if let Some(document) = self.load_document(tx, key).await? {
            return Ok(document);
        }
        // Logical storage has no legacy file to import. Initial contents enter
        // through the validated, audited candidate workflow.
        if !entries.is_empty() {
            return Err(Error::Denied);
        }
        let now = database_time(tx).await.map_err(app_error)?;
        let document = ResidentMemoryDocument {
            path: key.to_owned(),
            scope,
            revision: 1,
            entries: Vec::new(),
            content_digest: zuno_orchestration::sha256_json(&json!([])),
            projected_revision: 0,
            projection_error: None,
            time_created: now,
            time_updated: now,
        };
        query("INSERT INTO zuno_enterprise_preview.memory_document(tenant_id,principal_id,key,scope,revision,data) VALUES($1,$2,$3,$4,1,$5)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
            .bind(key).bind(scope.as_str()).bind(json!(document)).execute(&mut **tx).await.map_err(sql_error)?;
        self.insert_revision(tx, &document, "adopt", None).await?;
        Ok(document)
    }

    async fn insert_revision(
        &self,
        tx: &mut Tx,
        document: &ResidentMemoryDocument,
        operation: &str,
        candidate: Option<&str>,
    ) -> Result<(), Error> {
        query("INSERT INTO zuno_enterprise_preview.memory_revision(tenant_id,principal_id,key,revision,entries,content_digest,operation,candidate_id,time_created)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
            .bind(&document.path).bind(document.revision).bind(json!(document.entries)).bind(&document.content_digest)
            .bind(operation).bind(candidate).bind(document.time_updated).execute(&mut **tx).await.map_err(sql_error)?;
        Ok(())
    }

    pub(super) async fn commit(
        &self,
        tx: &mut Tx,
        input: ResidentMemoryCommit<'_>,
    ) -> Result<ResidentMemoryDocument, Error> {
        if self.key(input.path)? != input.scope {
            return Err(Error::Denied);
        }
        let mut candidate = self.load_candidate(tx, input.candidate_id).await?;
        if candidate.target_path != input.path || candidate.projection.scope != input.scope {
            return Err(Error::Denied);
        }
        let mut current = self.required_document(tx, input.path).await?;
        if current.revision != input.expected_revision || current.entries != input.before {
            return Err(Error::Conflict);
        }
        if input.operation == ResidentMemoryOperation::Apply {
            if candidate
                .base_revision
                .is_some_and(|revision| revision != current.revision)
                || !matches!(
                    candidate.projection.status,
                    MemoryCandidateStatus::Pending | MemoryCandidateStatus::Failed
                )
            {
                return Err(Error::Conflict);
            }
            let operation = zuno_memory::Operation::parse(
                1,
                candidate.projection.action.as_str(),
                candidate.projection.content.as_deref(),
                candidate.projection.old_text.as_deref(),
            )?;
            let expected = zuno_memory::store::preview_entries(
                input.scope.into(),
                self.limits.for_scope(input.scope.into()),
                input.before,
                &[operation],
            )?;
            if expected != input.after {
                return Err(invalid("Memory change does not match its snapshots"));
            }
            if let Some(evidence) = &candidate.evidence
                && !evidence.is_empty()
                && !self.references_current(tx, evidence, true).await?
            {
                return Err(Error::Conflict);
            }
        } else if candidate.projection.status != MemoryCandidateStatus::Applied
            || candidate.after_entries.as_deref() != Some(input.before)
            || candidate.before_entries.as_deref() != Some(input.after)
        {
            return Err(Error::Conflict);
        }
        if let Some(lease) = &self.lease
            && !matches!(input.authority, ResidentMemoryAuthority::Model {session_id} if session_id == lease.session_id.as_str())
        {
            return Err(Error::Denied);
        }
        match input.authority {
            ResidentMemoryAuthority::Host => self.host_only()?,
            ResidentMemoryAuthority::Model { session_id } => {
                self.require_generation(tx, Some(session_id)).await?;
                if candidate.projection.source_session_id.as_deref() != Some(session_id) {
                    return Err(Error::Denied);
                }
            }
            ResidentMemoryAuthority::Learning { job_id, lease } => {
                self.require_learning(
                    tx,
                    job_id,
                    lease,
                    candidate.projection.source_session_id.as_deref(),
                )
                .await?;
                if candidate.projection.source != MemorySource::Reflection {
                    return Err(Error::Denied);
                }
            }
        }
        let now = database_time(tx).await.map_err(app_error)?;
        candidate.projection.time_updated = now;
        candidate.projection.error = None;
        let operation = match input.operation {
            ResidentMemoryOperation::Apply => {
                candidate.projection.status = MemoryCandidateStatus::Applied;
                candidate.before_entries = Some(input.before.to_vec());
                candidate.after_entries = Some(input.after.to_vec());
                candidate.time_applied = Some(now);
                "apply"
            }
            ResidentMemoryOperation::Undo => {
                candidate.projection.status = MemoryCandidateStatus::Undone;
                "undo"
            }
        };
        self.save_candidate(tx, &candidate).await?;
        self.update_provenance(tx, &input, &candidate).await?;
        if input.before == input.after {
            return Ok(current);
        }
        current.revision = current.revision.checked_add(1).ok_or(Error::Conflict)?;
        current.entries = input.after.to_vec();
        current.content_digest = zuno_orchestration::sha256_json(&json!(current.entries));
        current.time_updated = now;
        let changed = query(
            "UPDATE zuno_enterprise_preview.memory_document SET revision=$4,data=$5
            WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND revision=$6",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(input.path)
        .bind(current.revision)
        .bind(json!(current))
        .bind(input.expected_revision)
        .execute(&mut **tx)
        .await
        .map_err(sql_error)?
        .rows_affected();
        if changed != 1 {
            return Err(Error::Conflict);
        }
        self.insert_revision(tx, &current, operation, Some(candidate.id()))
            .await?;
        Ok(current)
    }

    pub(super) async fn view(&self, tx: &mut Tx, key: &str) -> Result<ResidentMemoryView, Error> {
        let document = self.required_document(tx, key).await?;
        let rows = query("SELECT content,evidence FROM zuno_enterprise_preview.memory_provenance WHERE tenant_id=$1 AND principal_id=$2 AND key=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(key)
            .fetch_all(&mut **tx).await.map_err(sql_error)?;
        let mut managed = BTreeMap::new();
        for row in rows {
            let references: Vec<MemoryEvidenceReference> =
                serde_json::from_value(row.try_get("evidence").map_err(sql_error)?)
                    .map_err(decode_error)?;
            managed.insert(
                row.try_get::<String, _>("content").map_err(sql_error)?,
                references,
            );
        }
        let mut entries = Vec::new();
        let mut suppressed = Vec::new();
        for entry in &document.entries {
            let mut usable = true;
            if let Some(references) = managed.get(entry) {
                usable = false;
                for reference in references {
                    if self
                        .references_current(tx, std::slice::from_ref(reference), false)
                        .await?
                    {
                        usable = true;
                        break;
                    }
                }
            }
            if usable {
                entries.push(entry.clone());
            } else {
                suppressed.push(entry.clone());
            }
        }
        Ok(ResidentMemoryView {
            document,
            entries,
            managed,
            suppressed,
        })
    }

    async fn update_provenance(
        &self,
        tx: &mut Tx,
        input: &ResidentMemoryCommit<'_>,
        candidate: &MemoryCandidateRecord,
    ) -> Result<(), Error> {
        query("DELETE FROM zuno_enterprise_preview.memory_provenance WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND NOT(content=ANY($4))")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.path)
            .bind(input.after).execute(&mut **tx).await.map_err(sql_error)?;
        let explicit = input.operation == ResidentMemoryOperation::Undo
            || candidate.projection.source != MemorySource::Reflection;
        if explicit {
            for content in input
                .before
                .iter()
                .filter(|content| !input.after.contains(content))
            {
                query("INSERT INTO zuno_enterprise_preview.memory_retired(tenant_id,principal_id,key,content_digest,content)
                    VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.path)
                    .bind(zuno_orchestration::sha256_json(&json!(content))).bind(content).execute(&mut **tx).await.map_err(sql_error)?;
            }
            // A deliberate reaffirmation, including an identical Add, wins over
            // prior retirement and no longer depends on extraction evidence.
            let affirmed = if input.operation == ResidentMemoryOperation::Undo {
                input
                    .after
                    .iter()
                    .filter(|text| !input.before.contains(text))
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                candidate.projection.content.iter().cloned().collect()
            };
            for content in affirmed {
                self.remove_provenance(tx, input.path, &content).await?;
                query("DELETE FROM zuno_enterprise_preview.memory_retired WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND content_digest=$4")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.path)
                    .bind(zuno_orchestration::sha256_json(&json!(content))).execute(&mut **tx).await.map_err(sql_error)?;
            }
            return Ok(());
        }
        if candidate.projection.action == MemoryAction::Remove {
            return Ok(());
        }
        let Some(content) = candidate.projection.content.as_deref() else {
            return Ok(());
        };
        let Some(evidence) = candidate.evidence.as_ref() else {
            return Ok(());
        };
        let prior: Option<Value> = query_scalar("SELECT evidence FROM zuno_enterprise_preview.memory_provenance WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND content=$4")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.path).bind(content)
            .fetch_optional(&mut **tx).await.map_err(sql_error)?;
        if prior.is_none() && input.before.iter().any(|entry| entry == content) {
            return Ok(());
        }
        let mut references: Vec<MemoryEvidenceReference> = prior
            .map(serde_json::from_value)
            .transpose()
            .map_err(decode_error)?
            .unwrap_or_default();
        for reference in evidence {
            references.retain(|old| old.experience_id != reference.experience_id);
            references.push(reference.clone());
        }
        references.sort_by(|left, right| left.experience_id.cmp(&right.experience_id));
        if references.len() > 128 {
            return Err(invalid("Memory entry exceeds its evidence limit"));
        }
        query("INSERT INTO zuno_enterprise_preview.memory_provenance(tenant_id,principal_id,key,content,evidence) VALUES($1,$2,$3,$4,$5)
            ON CONFLICT(tenant_id,principal_id,key,content) DO UPDATE SET evidence=excluded.evidence")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.path)
            .bind(content).bind(json!(references)).execute(&mut **tx).await.map_err(sql_error)?;
        Ok(())
    }

    async fn remove_provenance(&self, tx: &mut Tx, key: &str, content: &str) -> Result<(), Error> {
        query("DELETE FROM zuno_enterprise_preview.memory_provenance WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND content=$4")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(key).bind(content)
            .execute(&mut **tx).await.map_err(sql_error)?;
        Ok(())
    }
}
