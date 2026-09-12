use super::*;
use zuno_db::{
    learning_job::LearningLease,
    memory_candidate::NewMemoryCandidate,
    memory_maintenance::{MemoryBatchCommit, MemoryBatchResult, MemorySourceRetraction},
    resident_memory::{ResidentMemoryAuthority, ResidentMemoryCommit, ResidentMemoryOperation},
};
use zuno_types::{MemoryAction, MemoryCandidateStatus};

impl TransactionMemory {
    pub(super) async fn require_learning(
        &self,
        tx: &mut Tx,
        id: &str,
        lease: &LearningLease,
        session: Option<&str>,
    ) -> Result<Value, Error> {
        let now = database_time(tx).await.map_err(app_error)?;
        let row = query("SELECT session_id,payload FROM zuno_enterprise_preview.learning_job
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND workspace_id=$4 AND status='running'
              AND owner_id=$5 AND lease_token=$6 AND lease_expires>$7
              AND (kind='extraction' OR (kind='project_aggregation' AND payload->>'purpose'='memory')) FOR UPDATE")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(id).bind(self.workspace.as_str())
            .bind(&lease.owner_id).bind(&lease.token).bind(now).fetch_optional(&mut **tx).await.map_err(sql_error)?.ok_or(Error::Conflict)?;
        let source_session: Option<String> = row.try_get("session_id").map_err(sql_error)?;
        if session.is_some() && session != source_session.as_deref() {
            return Err(Error::Denied);
        }
        self.require_generation(tx, source_session.as_deref())
            .await?;
        row.try_get("payload").map_err(sql_error)
    }

    pub(super) async fn retired(
        &self,
        tx: &mut Tx,
        key: &str,
        content: &str,
    ) -> Result<bool, Error> {
        self.key(key)?;
        query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.memory_retired WHERE tenant_id=$1 AND principal_id=$2 AND key=$3 AND content_digest=$4)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(key)
            .bind(zuno_orchestration::sha256_json(&json!(content))).fetch_one(&mut **tx).await.map_err(sql_error)
    }

    pub(super) async fn maintenance(
        &self,
        tx: &mut Tx,
        input: MemoryBatchCommit<'_>,
    ) -> Result<MemoryBatchResult, Error> {
        if input.project_id != self.workspace.as_str()
            || input.project_path != self.project_key
            || input.global_path != "global"
        {
            return Err(Error::Denied);
        }
        if input.changes.len() > 32
            || input.input_digest.len() != 64
            || !input
                .input_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("invalid bounded Memory batch"));
        }
        let payload = self
            .require_learning(tx, input.job_id, input.lease, None)
            .await?;
        if payload["purpose"] != "memory"
            || payload["projectPath"] != input.project_path
            || payload["inputDigest"] != input.input_digest
            || payload["globalRevision"].as_i64() != Some(input.global_revision)
            || payload["projectRevision"].as_i64() != Some(input.project_revision)
        {
            return Err(Error::Conflict);
        }
        if !input.evidence.is_empty() && !self.references_current(tx, input.evidence, true).await? {
            return Err(Error::Conflict);
        }
        for (key, scope, revision) in [
            ("global", MemoryScope::Global, input.global_revision),
            (
                input.project_path,
                MemoryScope::Project,
                input.project_revision,
            ),
        ] {
            let document = self.required_document(tx, key).await?;
            if document.scope != scope || document.revision != revision {
                return Err(Error::Conflict);
            }
        }
        let mut candidates = Vec::new();
        for change in input.changes {
            let candidate = &change.candidate;
            let scope = self.key(&candidate.target_path)?;
            if scope != candidate.target || candidate.source != MemorySource::Reflection {
                return Err(Error::Denied);
            }
            let references = candidate.evidence.as_deref().unwrap_or_default();
            if references
                .iter()
                .any(|reference| !input.evidence.contains(reference))
            {
                return Err(Error::Denied);
            }
            let operation = zuno_memory::Operation::parse(
                1,
                candidate.action.as_str(),
                candidate.content.as_deref(),
                candidate.old_text.as_deref(),
            )?;
            let after = zuno_memory::store::preview_entries(
                scope.into(),
                self.limits.for_scope(scope.into()),
                &change.before,
                &[operation],
            )?;
            if after != change.after {
                return Err(invalid("Memory batch snapshots do not match its operation"));
            }
            if let Some(content) = &candidate.content
                && self.retired(tx, &candidate.target_path, content).await?
            {
                return Err(Error::Conflict);
            }
            let view = self.view(tx, &candidate.target_path).await?;
            if view.document.entries != change.before {
                return Err(Error::Conflict);
            }
            let mut retraction = false;
            if matches!(
                candidate.action,
                MemoryAction::Replace | MemoryAction::Remove
            ) {
                let old = candidate
                    .old_text
                    .as_deref()
                    .ok_or_else(|| invalid("Memory change needs an exact old entry"))?;
                if !view.managed.contains_key(old) {
                    return Err(Error::Denied);
                }
                retraction = candidate.action == MemoryAction::Remove
                    && references.is_empty()
                    && view.suppressed.iter().any(|entry| entry == old);
            }
            if !retraction && !self.references_current(tx, references, true).await? {
                return Err(Error::Conflict);
            }
            if scope == MemoryScope::Global {
                for reference in references {
                    if !self
                        .reference_source(tx, reference, true)
                        .await?
                        .is_some_and(|source| source.user_authored)
                    {
                        return Err(Error::Denied);
                    }
                }
            }
            let inserted = self.insert_candidate(tx, change.candidate.clone()).await?;
            if !inserted.inserted {
                return Err(Error::Conflict);
            }
            if change.apply {
                self.commit(
                    tx,
                    ResidentMemoryCommit {
                        path: &candidate.target_path,
                        scope,
                        expected_revision: view.document.revision,
                        before: &change.before,
                        after: &change.after,
                        candidate_id: inserted.record.id(),
                        operation: ResidentMemoryOperation::Apply,
                        now: input.now,
                        authority: if retraction {
                            ResidentMemoryAuthority::Host
                        } else {
                            ResidentMemoryAuthority::Learning {
                                job_id: input.job_id,
                                lease: input.lease,
                            }
                        },
                    },
                )
                .await?;
            }
            candidates.push(self.load_candidate(tx, inserted.record.id()).await?);
        }
        let global = self.required_document(tx, "global").await?;
        let project = self.required_document(tx, input.project_path).await?;
        // Recheck at the end against DB time. A model request made before this
        // transaction cannot extend its learning lease by supplying a timestamp.
        self.require_learning(tx, input.job_id, input.lease, None)
            .await?;
        let now = database_time(tx).await.map_err(app_error)?;
        let changed = query("UPDATE zuno_enterprise_preview.learning_job SET status='completed',owner_id=NULL,lease_token=NULL,lease_expires=NULL,result=$7,time_updated=$8
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND status='running' AND owner_id=$4 AND lease_token=$5 AND lease_expires>$6")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.job_id)
            .bind(&input.lease.owner_id).bind(&input.lease.token).bind(now)
            .bind(json!({"purpose":"memory","inputDigest":input.input_digest,"globalRevision":global.revision,"projectRevision":project.revision,
                "candidates":candidates.iter().map(|candidate| &candidate.projection).collect::<Vec<_>>()}))
            .bind(now).execute(&mut **tx).await.map_err(sql_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::Conflict);
        }
        query("INSERT INTO zuno_enterprise_preview.memory_maintenance_state(tenant_id,principal_id,workspace_id,key,input_digest,global_revision,project_revision,job_id)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(tenant_id,principal_id,workspace_id,key) DO UPDATE SET
              input_digest=excluded.input_digest,global_revision=excluded.global_revision,project_revision=excluded.project_revision,job_id=excluded.job_id")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(input.project_id).bind(input.project_path)
            .bind(input.input_digest).bind(global.revision).bind(project.revision).bind(input.job_id).execute(&mut **tx).await.map_err(sql_error)?;
        Ok(MemoryBatchResult {
            candidates,
            documents: vec![global, project],
        })
    }

    pub(super) async fn forget(
        &self,
        tx: &mut Tx,
        ids: &[String],
        keys: &[String],
        session: Option<&str>,
    ) -> Result<MemorySourceRetraction, Error> {
        if ids.is_empty() || ids.len() > 128 || keys.len() > 2 {
            return Err(invalid("invalid bounded source retraction"));
        }
        if let Some(session) = session {
            self.ensure_session(tx, session).await?;
        }
        for key in keys {
            self.key(key)?;
        }
        let mut forgotten = Vec::new();
        for id in ids {
            let row = query("SELECT workspace_id FROM zuno_enterprise_preview.memory_evidence WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(id)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?.ok_or(Error::Denied)?;
            if row
                .try_get::<String, _>("workspace_id")
                .map_err(sql_error)?
                != self.workspace.as_str()
            {
                return Err(Error::Denied);
            }
            query("UPDATE zuno_enterprise_preview.memory_evidence SET forgotten=true WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(id)
                .execute(&mut **tx).await.map_err(sql_error)?;
            if !forgotten.contains(id) {
                forgotten.push(id.clone());
            }
        }
        let now = database_time(tx).await.map_err(app_error)?;
        // Do not use a bounded UI page to find all dependent proposals.
        let candidate_ids: Vec<String> = query_scalar("SELECT id FROM zuno_enterprise_preview.memory_candidate
            WHERE tenant_id=$1 AND principal_id=$2 AND key=ANY($3) AND status IN ('pending','failed')
              AND data->'projection'->>'source'='reflection'
              AND EXISTS(SELECT 1 FROM jsonb_array_elements(COALESCE(NULLIF(data->'evidence','null'),'[]')) e WHERE e->>'experience_id'=ANY($4))")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(keys).bind(ids)
            .fetch_all(&mut **tx).await.map_err(sql_error)?;
        let mut rejected = Vec::new();
        for id in candidate_ids {
            let mut candidate = self.load_candidate(tx, &id).await?;
            candidate.projection.status = MemoryCandidateStatus::Rejected;
            candidate.projection.error = Some("source evidence was forgotten".to_owned());
            candidate.projection.time_updated = now;
            self.save_candidate(tx, &candidate).await?;
            rejected.push(id);
        }
        let mut retractions = Vec::new();
        let mut documents = Vec::new();
        for key in keys {
            let view = self.view(tx, key).await?;
            for text in view.suppressed {
                let current = self.required_document(tx, key).await?;
                let inserted = self
                    .insert_candidate(
                        tx,
                        NewMemoryCandidate {
                            id: format!("mem_retract_{}", uuid::Uuid::now_v7().simple()),
                            target: current.scope,
                            target_path: key.clone(),
                            action: MemoryAction::Remove,
                            content: None,
                            old_text: Some(text.clone()),
                            reason: "All supporting memory sources were invalidated.".to_owned(),
                            confidence: 10_000,
                            source: MemorySource::Reflection,
                            source_session_id: session.map(str::to_owned),
                            source_message_id: None,
                            fingerprint: None,
                            base_revision: Some(current.revision),
                            evidence: Some(Vec::new()),
                            time_created: now,
                        },
                    )
                    .await?;
                let after = current
                    .entries
                    .iter()
                    .filter(|entry| **entry != text)
                    .cloned()
                    .collect::<Vec<_>>();
                self.commit(
                    tx,
                    ResidentMemoryCommit {
                        path: key,
                        scope: current.scope,
                        expected_revision: current.revision,
                        before: &current.entries,
                        after: &after,
                        candidate_id: inserted.record.id(),
                        operation: ResidentMemoryOperation::Apply,
                        now,
                        authority: ResidentMemoryAuthority::Host,
                    },
                )
                .await?;
                retractions.push(self.load_candidate(tx, inserted.record.id()).await?);
            }
            documents.push(self.required_document(tx, key).await?);
        }
        Ok(MemorySourceRetraction {
            forgotten_experience_ids: forgotten,
            retractions,
            rejected_candidate_ids: rejected,
            documents,
        })
    }
}
