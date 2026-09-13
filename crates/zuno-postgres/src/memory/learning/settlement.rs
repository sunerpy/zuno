use super::*;
use zuno_db::{
    learning_job::{LearningJobKind, LearningJobRecord, LearningJobStatus, LearningLease},
    memory_evidence::MemoryEvidenceReference,
    resident_memory::ResidentMemoryView,
};
use zuno_learning::{
    ExtractedEvidenceKind, ExtractedExperienceKind, LearningExtraction, MemoryConsolidation,
    MemoryConsolidationRequest,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MaintenanceContext {
    batch: zuno_learning::MemoryJobInput,
    views: Vec<ResidentMemoryView>,
    references: Vec<MemoryEvidenceReference>,
}

impl PostgresLearningRuntime {
    pub async fn complete(&self, completion: LearningCompletion) -> Result<(), Error> {
        let (actor, workspace, session) = self
            .job_binding(&completion.lease.owner, &completion.lease.job_id)
            .await?;
        self.memory.automate(actor,workspace,session,move |provider| {
            let digest=zuno_orchestration::sha256_json(&json!(completion.result));
            let prior=provider.execute(async |tx| {
                let row=provider.execution_row(tx,&completion.lease.job_id).await?;
                if row.try_get::<String,_>("status").map_err(sql_error)?=="completed" {
                    let result:Value=query_scalar("SELECT result FROM zuno_enterprise_preview.learning_job
                        WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                        .bind(completion.lease.owner.tenant_id.as_str()).bind(completion.lease.owner.principal_id.as_str()).bind(completion.lease.job_id.as_str())
                        .fetch_one(&mut **tx).await.map_err(sql_error)?;
                    return if result["completionDigest"]==digest {Ok(true)} else {Err(Error::Conflict)};
                }
                Ok(false)
            })?;
            if prior {return Ok(());}
            let (job,context)=provider.execute(async |tx| {
                let job=provider.check_execution(tx,&completion.lease).await?;
                if job.tokens_charged>job.limits.total_tokens {return Err(Error::Denied);}
                let row=provider.execution_row(tx,&job.id).await?;
                let context:Value=row.try_get("context").map_err(sql_error)?;
                let row=query("SELECT outcome,outcome_digest FROM zuno_enterprise_preview.learning_model_request
                    WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='completed'
                    ORDER BY created_at DESC,request_id DESC LIMIT 1")
                    .bind(completion.lease.owner.tenant_id.as_str()).bind(completion.lease.owner.principal_id.as_str()).bind(job.id.as_str())
                    .fetch_optional(&mut **tx).await.map_err(sql_error)?.ok_or(Error::Conflict)?;
                let raw:Value=row.try_get("outcome").map_err(sql_error)?;
                if row.try_get::<String,_>("outcome_digest").map_err(sql_error)?!=zuno_orchestration::sha256_json(&raw) {
                    return Err(Error::InvalidData);
                }
                let record:zuno_learning::LearningModelRecord=serde_json::from_value(raw).map_err(decode_error)?;
                let zuno_learning::LearningModelEvent::Outcome{outcome:zuno_learning::LearningModelOutcome::Completed{output,..},..}=record.event else{return Err(Error::Conflict);};
                let expected=LearningOutput::decode(job.input.phase(),&output).map_err(decode_error)?;
                if zuno_orchestration::sha256_json(&json!(expected))!=digest {return Err(Error::Denied);}
                Ok((job,context))
            })?;
            match (&job.input,&completion.result) {
                (LearningInput::Extraction(input),LearningOutput::Extraction(output))=>{
                    let context:ExtractionContext=serde_json::from_value(context).map_err(decode_error)?;
                    provider.execute(async |tx| provider.settle_extraction(tx,&job,input,output,&context).await)?;
                    provider.schedule_maintenance(&job,&context)?;
                }
                (LearningInput::Maintenance(_),LearningOutput::Maintenance(output))=>{
                    let context:MaintenanceContext=serde_json::from_value(context).map_err(decode_error)?;
                    provider.settle_maintenance(&job,&completion.lease,output,&context)?;
                }
                _=>return Err(Error::Denied),
            }
            provider.execute(async |tx| {
                query("UPDATE zuno_enterprise_preview.learning_job SET result=COALESCE(result,'{}'::jsonb)||$4
                    WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                    .bind(completion.lease.owner.tenant_id.as_str()).bind(completion.lease.owner.principal_id.as_str()).bind(job.id.as_str())
                    .bind(json!({"completionDigest":digest})).execute(&mut **tx).await.map_err(sql_error)?;
                Ok(())
            })
        }).await
    }
}

impl TransactionMemory {
    async fn settle_extraction(
        &self,
        tx: &mut Tx,
        job: &LearningExecution,
        input: &zuno_learning::ExtractionRequest,
        output: &LearningExtraction,
        context: &ExtractionContext,
    ) -> Result<(), Error> {
        output
            .validate_bounds()
            .map_err(|_| invalid("invalid extraction output bounds"))?;
        if output.memories.iter().any(|hint| {
            hint.experience_ordinal >= output.experiences.len()
                || !hint.confidence.is_finite()
                || !(0.0..=1.0).contains(&hint.confidence)
        }) {
            return Err(invalid("invalid memory hint provenance"));
        }
        let now = database_time(tx).await.map_err(app_error)?;
        for (ordinal, experience) in output.experiences.iter().enumerate() {
            if experience.title.trim().is_empty()
                || experience.summary.trim().is_empty()
                || !experience.confidence.is_finite()
                || !(0.0..=1.0).contains(&experience.confidence)
                || experience.kind == ExtractedExperienceKind::UnresolvedIssue
                    && experience.resolution.is_some()
            {
                return Err(invalid("invalid extracted experience"));
            }
            let mut references = Vec::new();
            let mut verified = false;
            let mut user_authored = false;
            for evidence in &experience.evidence {
                let reference = evidence
                    .source_id
                    .as_ref()
                    .ok_or_else(|| invalid("extraction evidence needs a source ID"))?;
                let selected = input
                    .sources
                    .iter()
                    .find(|source| source.reference_id == *reference)
                    .ok_or_else(|| invalid("extraction invented a source"))?;
                let expected_kind = if selected.proves_success {
                    ExtractedEvidenceKind::Tool
                } else {
                    ExtractedEvidenceKind::User
                };
                if evidence.kind != expected_kind
                    || evidence.excerpt.trim().is_empty()
                    || evidence.excerpt.len() > 2048
                    || !selected.content.contains(&evidence.excerpt)
                {
                    return Err(invalid(
                        "extraction evidence must cite an exact supplied excerpt",
                    ));
                }
                let frozen = context
                    .sources
                    .iter()
                    .find(|source| source.reference == *reference)
                    .ok_or(Error::InvalidData)?;
                let source = self
                    .source(tx, &frozen.origin, self.workspace.as_str())
                    .await?
                    .ok_or(Error::Conflict)?;
                if source.digest != frozen.source_digest {
                    return Err(Error::Conflict);
                }
                self.require_automation(tx, Some(&source.session)).await?;
                references.push(
                    self.record_evidence(tx, frozen.origin.clone(), &evidence.excerpt)
                        .await?,
                );
                verified |= selected.proves_success;
                user_authored |= source.user_authored;
            }
            let eligible = !references.is_empty()
                && experience.kind != ExtractedExperienceKind::UnresolvedIssue
                && (verified
                    || user_authored
                        && matches!(
                            experience.kind,
                            ExtractedExperienceKind::UserCorrection
                                | ExtractedExperienceKind::ExplicitFeedback
                        ));
            let hints = if eligible {
                output
                    .memories
                    .iter()
                    .filter(|hint| hint.experience_ordinal == ordinal)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let id = format!(
                "experience_{}",
                zuno_orchestration::sha256_json(&json!([self.principal.owner(), job.id, ordinal]))
            );
            query("INSERT INTO zuno_enterprise_preview.learning_experience
                (tenant_id,principal_id,id,workspace_id,job_id,ordinal,data,evidence,hints,created_at)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(id)
                .bind(self.workspace.as_str()).bind(job.id.as_str()).bind(ordinal as i32)
                .bind(json!({"experience":experience,"eligible":eligible,"userAuthored":user_authored}))
                .bind(json!(references)).bind(json!(hints)).bind(now).execute(&mut **tx).await.map_err(sql_error)?;
        }
        let settled_at = database_time(tx).await.map_err(app_error)?;
        let changed=query("UPDATE zuno_enterprise_preview.learning_job SET status='completed',owner_id=NULL,lease_token=NULL,lease_expires=NULL,
            result=$4,time_updated=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND status='running' AND lease_expires>$5
            AND EXISTS(SELECT 1 FROM zuno_enterprise_preview.learning_execution e
                WHERE e.tenant_id=$1 AND e.principal_id=$2 AND e.job_id=$3 AND e.deadline_at>$5)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(job.id.as_str())
            .bind(json!({"experiences":output.experiences.len()})).bind(settled_at).execute(&mut **tx).await.map_err(sql_error)?.rows_affected();
        if changed != 1 {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    fn schedule_maintenance(
        self: &Arc<Self>,
        job: &LearningExecution,
        extraction: &ExtractionContext,
    ) -> Result<(), Error> {
        let service = self.service()?;
        let views = service.read_views()?;
        let signals = service.maintenance_signals()?;
        let (mut experiences,mut references)=self.execute(async |tx| {
            let rows=query("SELECT data,evidence,hints,created_at FROM zuno_enterprise_preview.learning_experience
                WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3 AND data->>'eligible'='true'
                ORDER BY created_at DESC,id DESC LIMIT 64")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(self.workspace.as_str())
                .fetch_all(&mut **tx).await.map_err(sql_error)?;
            let mut experiences=Vec::new();let mut references=Vec::new();
            for row in rows {
                let data:Value=row.try_get("data").map_err(sql_error)?;
                let refs:Vec<MemoryEvidenceReference>=serde_json::from_value(row.try_get("evidence").map_err(sql_error)?).map_err(decode_error)?;
                for reference in refs {
                    if references.contains(&reference) {continue;}
                    let source=match self.reference_source(tx,&reference,true).await {
                        Ok(Some(source))=>source,Ok(None)|Err(Error::Denied|Error::Conflict)=>continue,Err(error)=>return Err(error),
                    };
                    let xp=&data["experience"];
                    experiences.push(json!({
                        "id":reference.experience_id,"kind":xp["kind"],"title":xp["title"],"summary":xp["summary"],"resolution":xp["resolution"],
                        "confidence":xp["confidence"],"user_authored":source.user_authored,
                        "created_at":row.try_get::<i64,_>("created_at").map_err(sql_error)?,
                        "raw_hints":row.try_get::<Value,_>("hints").map_err(sql_error)?,
                    }));
                    references.push(reference);
                }
            }
            Ok((experiences,references))
        })?;
        let mut request=MemoryConsolidationRequest {
            project_id:self.workspace.to_string(),session_id:job.session.to_string(),
            scopes:views.iter().map(|view|json!({
                "scope":view.document.scope,"revision":view.document.revision,"character_limit":self.limits.for_scope(view.document.scope.into()),
                "entries":view.document.entries.iter().map(|text|json!({"text":text,"managed":view.managed.contains_key(text),
                    "source_invalidated":view.suppressed.contains(text)})).collect::<Vec<_>>(),
            })).collect(), experiences:Vec::new(),user_changes:signals,correction:None,
        };
        loop {
            request.experiences = experiences.clone();
            if serde_json::to_vec(&request).map_err(decode_error)?.len()
                <= (extraction.maintenance_limits.maximum_input_bytes as usize)
                    .saturating_sub(16384)
            {
                break;
            }
            if !request.user_changes.is_empty() {
                request.user_changes.pop();
            } else if !experiences.is_empty() {
                experiences.pop();
                references.pop();
            } else {
                return Err(invalid(
                    "current Memory cannot fit the maintenance model profile",
                ));
            }
        }
        if references.is_empty() && views.iter().all(|view| view.suppressed.is_empty()) {
            return Ok(());
        }
        let digest = zuno_orchestration::sha256_json(&json!([
            references,
            request.user_changes,
            request.experiences
        ]));
        let global = views
            .iter()
            .find(|v| v.document.scope == MemoryScope::Global)
            .ok_or(Error::InvalidData)?;
        let project = views
            .iter()
            .find(|v| v.document.scope == MemoryScope::Project)
            .ok_or(Error::InvalidData)?;
        let batch = zuno_learning::MemoryJobInput {
            purpose: "memory".to_owned(),
            project_path: self.project_key.clone(),
            input_digest: digest.clone(),
            global_revision: global.document.revision,
            project_revision: project.document.revision,
        };
        let state = service.maintenance_state(self.workspace.as_str())?;
        if state.is_some_and(|state| {
            state.input_digest == digest
                && state.global_revision == global.document.revision
                && state.project_revision == project.document.revision
        }) {
            return Ok(());
        }
        let id = JobId::new(format!(
            "maintain_{}",
            zuno_orchestration::sha256_json(&json!([
                self.principal.owner(),
                self.workspace,
                batch,
                extraction.maintenance,
            ]))
        ))
        .map_err(decode_error)?;
        let context = MaintenanceContext {
            batch,
            views,
            references,
        };
        self.execute(async |tx| {
            let source_row=self.execution_row(tx,&job.id).await?;
            let source_job=JobId::new(source_row.try_get::<String,_>("source_job_id").map_err(sql_error)?).map_err(decode_error)?;
            let exists:bool=query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.learning_job WHERE tenant_id=$1 AND principal_id=$2 AND id=$3)")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(id.as_str())
                .fetch_one(&mut **tx).await.map_err(sql_error)?;
            if !exists {self.insert_execution(tx,NewExecution {
                id:&id,source_job:&source_job,session:&job.session,configuration:&extraction.maintenance,
                input:LearningInput::Maintenance(request),limits:&extraction.maintenance_limits,context:json!(context),
            }).await?;}
            Ok(())
        })
    }

    fn settle_maintenance(
        self: &Arc<Self>,
        job: &LearningExecution,
        lease: &zuno_application::learning::LearningExecutionLease,
        output: &MemoryConsolidation,
        context: &MaintenanceContext,
    ) -> Result<(), Error> {
        let service = self.service()?;
        let mut updates = Vec::new();
        for update in &output.updates {
            let evidence = update
                .evidence_ids
                .iter()
                .map(|id| {
                    context
                        .references
                        .iter()
                        .find(|r| r.experience_id == *id)
                        .cloned()
                        .ok_or(Error::Denied)
                })
                .collect::<Result<Vec<_>, _>>()?;
            updates.push(zuno_memory::MemoryMaintenanceUpdate {
                scope: update.scope.into(),
                action: update.action.into(),
                content: update.content.clone(),
                old_text: update.old_text.clone(),
                reason: update.reason.clone(),
                confidence: update.confidence,
                evidence,
            });
        }
        let now = self.execute(async |tx| database_time(tx).await.map_err(app_error))?;
        let record = LearningJobRecord {
            id: job.id.to_string(),
            project_id: Some(job.workspace.to_string()),
            session_id: Some(job.session.to_string()),
            source_message_id: None,
            kind: LearningJobKind::ProjectAggregation,
            extractor_version: None,
            idempotency_key: job.id.to_string(),
            status: LearningJobStatus::Running,
            attempt: lease.epoch as u32,
            owner_id: Some(lease.worker.to_string()),
            lease_expires: Some(lease.expires_at_ms),
            scheduled_at: now,
            payload: Some(json!(context.batch)),
            result: None,
            error: None,
            time_created: now,
            time_updated: now,
            time_completed: None,
            lease_token: Some(lease.token.clone()),
        };
        service.commit_maintenance(
            zuno_memory::MemoryMaintenanceContext {
                job: &record,
                lease: &LearningLease {
                    owner_id: lease.worker.to_string(),
                    token: lease.token.clone(),
                },
                views: &context.views,
                evidence: &context.references,
                input_digest: &context.batch.input_digest,
                now,
            },
            updates,
        )?;
        Ok(())
    }
}
