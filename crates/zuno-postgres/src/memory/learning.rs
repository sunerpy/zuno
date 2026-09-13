//! Bounded owner transactions for background learning. Model I/O stays outside
//! these transactions and outside the control-plane process.
use super::*;
use serde::{Deserialize, Serialize};
use zuno_learning::distributed::*;
use zuno_types::identity::{JobId, PrincipalKey, TenantId};

mod journal;
mod settlement;
mod store;
mod wake;
use store::NewExecution;

#[derive(Clone)]
pub struct PostgresLearningRuntime {
    memory: PostgresMemoryBackend,
    tenant: TenantId,
    grants: Vec<MemoryLearningGrant>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FrozenSource {
    reference: String,
    origin: zuno_memory::remote::MemoryEvidenceOrigin,
    source_digest: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractionContext {
    sources: Vec<FrozenSource>,
    maintenance: zuno_application::runtime::ConfigurationRef,
    maintenance_limits: LearningExecutionLimits,
}

impl PostgresLearningRuntime {
    pub fn new(
        memory: PostgresMemoryBackend,
        tenant: TenantId,
        grants: Vec<MemoryLearningGrant>,
    ) -> Result<Self, Error> {
        if grants.is_empty() || grants.len() > 64 {
            return Err(invalid(
                "learning requires 1–64 installed source definitions",
            ));
        }
        for (index, grant) in grants.iter().enumerate() {
            grant.source.validate().map_err(app_error)?;
            grant.extraction.validate().map_err(app_error)?;
            grant.maintenance.validate().map_err(app_error)?;
            grant.extraction_limits.validate().map_err(app_error)?;
            grant.maintenance_limits.validate().map_err(app_error)?;
            if grants[..index]
                .iter()
                .any(|prior| prior.source == grant.source)
            {
                return Err(invalid("duplicate learning source definition"));
            }
        }
        Ok(Self {
            memory,
            tenant,
            grants,
        })
    }

    /// Recover missed post-turn admissions by scanning completed, opted-in root
    /// Jobs. A stable ID deduplicates this scan across control-plane instances.
    pub async fn schedule(&self, maximum: u32) -> Result<u32, Error> {
        if !(1..=32).contains(&maximum) {
            return Err(invalid("invalid learning scan bound"));
        }
        let owners: Vec<String> =
            query_scalar("SELECT principal_id FROM zuno_enterprise_preview.dispatch_owners($1)")
                .bind(self.tenant.as_str())
                .fetch_all(&self.memory.backend.pool)
                .await
                .map_err(sql_error)?;
        let mut inserted = 0;
        for principal_id in owners {
            let owner = PrincipalKey {
                tenant_id: self.tenant.clone(),
                principal_id: zuno_types::identity::PrincipalId::new(principal_id)
                    .map_err(decode_error)?,
            };
            let mut tx = owner_transaction(&self.memory.backend.pool, &owner)
                .await
                .map_err(app_error)?;
            let actor: Option<Value> = query_scalar(
                "SELECT automation_actor FROM zuno_enterprise_preview.memory_policy
                 WHERE tenant_id=$1 AND principal_id=$2 AND automatic_private AND generate_private",
            )
            .bind(owner.tenant_id.as_str())
            .bind(owner.principal_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?
            .flatten();
            tx.commit().await.map_err(sql_error)?;
            let Some(actor) = actor else {
                continue;
            };
            let actor: PrincipalScope = serde_json::from_value(actor).map_err(decode_error)?;
            if actor.owner() != owner {
                return Err(Error::Denied);
            }
            for grant in &self.grants {
                if inserted >= maximum {
                    return Ok(inserted);
                }
                let mut tx = owner_transaction(&self.memory.backend.pool, &owner)
                    .await
                    .map_err(app_error)?;
                let roots: Vec<String> = query_scalar(
                    "SELECT r.job_id FROM zuno_enterprise_preview.runtime_job r
                     JOIN zuno_enterprise_preview.session s ON s.tenant_id=r.tenant_id AND s.principal_id=r.principal_id AND s.id=r.session_id
                     JOIN zuno_enterprise_preview.memory_policy p ON p.tenant_id=r.tenant_id AND p.principal_id=r.principal_id
                     WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.phase='completed'
                       AND r.configuration=$3 AND s.workspace_id=$4 AND s.parent_id IS NULL
                       AND r.time_created>=p.automation_since
                       AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.learning_execution e
                         WHERE e.tenant_id=r.tenant_id AND e.principal_id=r.principal_id AND e.source_job_id=r.job_id AND e.phase='extraction')
                     ORDER BY r.time_created,r.job_id LIMIT $5",
                ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(json!(grant.source))
                    .bind(grant.workspace.as_str()).bind(i64::from(maximum-inserted))
                    .fetch_all(&mut *tx).await.map_err(sql_error)?;
                let mut selected = Vec::new();
                for id in roots {
                    selected.push(
                        crate::runtime::read_job(&mut tx, &owner, &id)
                            .await
                            .map_err(app_error)?,
                    );
                }
                tx.commit().await.map_err(sql_error)?;
                for job in selected {
                    let grant = grant.clone();
                    let result = self
                        .memory
                        .automate(
                            actor.clone(),
                            grant.workspace.clone(),
                            job.session_id.clone(),
                            move |provider| {
                                provider.execute(async |tx| {
                                    provider.enqueue_extraction(tx, &job, &grant).await
                                })
                            },
                        )
                        .await;
                    match result {
                        Ok(true) => inserted += 1,
                        Ok(false) | Err(Error::Denied | Error::Conflict) => {}
                        Err(error) => return Err(error),
                    }
                }
                if inserted < maximum {
                    match self.refresh_maintenance(&actor, grant).await {
                        Ok(true) => inserted += 1,
                        Ok(false) | Err(Error::Denied | Error::Conflict) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        Ok(inserted)
    }
}

impl TransactionMemory {
    async fn enqueue_extraction(
        &self,
        tx: &mut Tx,
        source: &zuno_application::runtime::RuntimeJob,
        grant: &MemoryLearningGrant,
    ) -> Result<bool, Error> {
        use zuno_db::learning_source::{LearningSource, LearningSourceField, LearningSourceKind};
        use zuno_memory::remote::MemoryEvidenceOrigin;
        let current = crate::runtime::read_job(tx, &self.principal.owner(), source.id.as_str())
            .await
            .map_err(app_error)?;
        if current.phase != zuno_application::runtime::JobPhase::Completed
            || current.configuration != grant.source
            || current.session_id != source.session_id
        {
            return Err(Error::Conflict);
        }
        // Selection can race a disable/re-enable decision while waiting for the
        // owner transaction. The new consent does not cover an older root Job.
        let within_consent:bool=query_scalar("SELECT EXISTS(
            SELECT 1 FROM zuno_enterprise_preview.runtime_job r
            JOIN zuno_enterprise_preview.memory_policy p ON p.tenant_id=r.tenant_id AND p.principal_id=r.principal_id
            JOIN zuno_enterprise_preview.session s ON s.tenant_id=r.tenant_id AND s.principal_id=r.principal_id AND s.id=r.session_id
            WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.job_id=$3 AND r.phase='completed'
              AND s.parent_id IS NULL AND p.automatic_private AND p.generate_private AND r.time_created>=p.automation_since)")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(source.id.as_str())
            .fetch_one(&mut **tx).await.map_err(sql_error)?;
        if !within_consent {
            return Ok(false);
        }
        let id = JobId::new(format!(
            "learn_{}",
            zuno_orchestration::sha256_json(&json!([
                self.principal.owner(),
                source.id,
                grant.extraction,
                zuno_learning::LEARNING_EXTRACTOR_VERSION
            ]))
        ))
        .map_err(decode_error)?;
        let exists: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.learning_job
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3)",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(sql_error)?;
        if exists {
            return Ok(false);
        }
        let operations: Vec<String> = query_scalar(
            "SELECT operation_id FROM zuno_enterprise_preview.gateway_operation
             WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND completion IS NOT NULL
             ORDER BY operation_id LIMIT 64",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(source.id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(sql_error)?;
        let mut origins = vec![MemoryEvidenceOrigin::UserInput {
            session_id: source.session_id.clone(),
            input_id: source.input_id.clone(),
        }];
        let mut truncated = operations.len() > 63;
        origins.extend(
            operations
                .into_iter()
                .take(63)
                .map(|id| {
                    zuno_types::identity::OperationId::new(id)
                        .map(|operation_id| MemoryEvidenceOrigin::Operation { operation_id })
                        .map_err(decode_error)
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        let mut sources = Vec::new();
        let mut frozen = Vec::new();
        for origin in origins {
            let Some(value) = self.source(tx, &origin, self.workspace.as_str()).await? else {
                continue;
            };
            let text = zuno_error::ProviderError::sanitize_diagnostic(&value.text, &[]);
            truncated |= text.len() < value.text.len();
            if text.trim().is_empty() {
                continue;
            }
            let reference = format!(
                "source_{}",
                zuno_orchestration::sha256_json(&json!([self.principal.owner(), origin]))
            );
            sources.push(LearningSource {
                reference_id: reference.clone(),
                source_id: reference.clone(),
                message_id: source.input_id.to_string(),
                kind: if value.user_authored {
                    LearningSourceKind::User
                } else {
                    LearningSourceKind::Tool
                },
                field: if value.user_authored {
                    LearningSourceField::Text
                } else {
                    LearningSourceField::Output
                },
                source_digest: value.digest.clone(),
                content_digest: zuno_db::learning_source::digest(&text),
                content: text,
                tool: (!value.user_authored).then(|| "environment_command".to_owned()),
                arguments: None,
                proves_success: !value.user_authored,
            });
            frozen.push(FrozenSource {
                reference,
                origin,
                source_digest: value.digest,
            });
        }
        let request = zuno_learning::ExtractionRequest {
            project_id: self.workspace.to_string(),
            session_id: source.session_id.to_string(),
            source_message_id: source.id.to_string(),
            transcript: String::new(),
            had_tool_calls: sources.iter().any(|s| s.proves_success),
            had_artifacts: false,
            recovered_from_error: false,
            user_corrected: false,
            explicit_feedback: false,
            sources,
            sources_truncated: truncated,
        }
        .bounded((grant.extraction_limits.maximum_input_bytes as usize).saturating_sub(16384))
        .map_err(|_| invalid("learning source exceeds the configured model input limit"))?;
        frozen.retain(|f| {
            request
                .sources
                .iter()
                .any(|s| s.reference_id == f.reference)
        });
        let context = ExtractionContext {
            sources: frozen,
            maintenance: grant.maintenance.clone(),
            maintenance_limits: grant.maintenance_limits.clone(),
        };
        self.insert_execution(
            tx,
            NewExecution {
                id: &id,
                source_job: &source.id,
                session: &source.session_id,
                configuration: &grant.extraction,
                input: LearningInput::Extraction(request),
                limits: &grant.extraction_limits,
                context: json!(context),
            },
        )
        .await?;
        Ok(true)
    }
}

impl PostgresMemoryBackend {
    async fn automate<T: Send + 'static>(
        &self,
        principal: PrincipalScope,
        workspace: WorkspaceId,
        session: SessionId,
        work: impl FnOnce(Arc<TransactionMemory>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Unavailable)?;
        let mut tx = scoped_transaction(&self.backend.pool, &principal)
            .await
            .map_err(app_error)?;
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(zuno_orchestration::sha256_json(&json!([
                "memory",
                principal.owner()
            ])))
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let provider = Arc::new(TransactionMemory {
            transaction: Mutex::new(Some(tx)),
            runtime: Handle::current(),
            principal,
            project_key: format!("project:{}", workspace.as_str()),
            workspace,
            lease: None,
            automation_session: Some(session),
            limits: self.limits,
            deadline: tokio::time::Instant::now() + self.transaction_timeout,
        });
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = (|| {
                let session = provider.automation_session.as_ref().ok_or(Error::Denied)?;
                provider.execute(async |tx| {
                    provider
                        .require_automation(tx, Some(session.as_str()))
                        .await
                })?;
                let output = work(provider.clone())?;
                provider.execute(async |tx| {
                    provider
                        .require_automation(tx, Some(session.as_str()))
                        .await
                })?;
                let tx = provider
                    .transaction
                    .lock()
                    .map_err(|_| Error::Unavailable)?
                    .take()
                    .ok_or(Error::Denied)?;
                provider
                    .runtime
                    .block_on(tokio::time::timeout_at(provider.deadline, async {
                        tx.commit().await.map_err(sql_error)
                    }))
                    .map_err(|_| Error::Unavailable)??;
                Ok(output)
            })();
            if result.is_err() {
                provider.rollback()?;
            }
            result
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}
