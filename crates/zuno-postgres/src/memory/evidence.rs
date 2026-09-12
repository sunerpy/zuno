use super::*;
use zuno_application::environment::{OperationCompletion, OperationPhase};
use zuno_db::memory_evidence::MemoryEvidenceReference;
use zuno_memory::remote::MemoryEvidenceOrigin;

pub(super) struct Source {
    pub digest: String,
    pub text: String,
    pub session: String,
    pub user_authored: bool,
}

impl TransactionMemory {
    /// No cached "verified" flag substitutes for these durable source facts.
    pub(super) async fn source(
        &self,
        tx: &mut Tx,
        origin: &MemoryEvidenceOrigin,
        workspace: &str,
    ) -> Result<Option<Source>, Error> {
        let result = match origin {
            MemoryEvidenceOrigin::UserInput {
                session_id,
                input_id,
            } => {
                let row = query("SELECT i.prompt FROM zuno_enterprise_preview.input i
                    JOIN zuno_enterprise_preview.session s ON s.tenant_id=i.tenant_id AND s.principal_id=i.principal_id AND s.id=i.session_id
                    WHERE i.tenant_id=$1 AND i.principal_id=$2 AND i.session_id=$3 AND i.id=$4 AND s.workspace_id=$5 FOR SHARE OF i")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                    .bind(session_id.as_str()).bind(input_id.as_str()).bind(workspace)
                    .fetch_optional(&mut **tx).await.map_err(sql_error)?;
                let Some(row) = row else {
                    return Ok(None);
                };
                let prompt: Value = row.try_get("prompt").map_err(sql_error)?;
                if prompt["kind"] != "user" {
                    return Ok(None);
                }
                let Some(text) = prompt.pointer("/prompt/text").and_then(Value::as_str) else {
                    return Ok(None);
                };
                Source {
                    digest: zuno_orchestration::sha256_json(&prompt),
                    text: text.to_owned(),
                    session: session_id.to_string(),
                    user_authored: true,
                }
            }
            MemoryEvidenceOrigin::Operation { operation_id } => {
                let row = query("SELECT o.completion,o.completion_digest,o.session_id FROM zuno_enterprise_preview.gateway_operation o
                    JOIN zuno_enterprise_preview.session s ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.id=o.session_id
                    WHERE o.tenant_id=$1 AND o.principal_id=$2 AND o.operation_id=$3 AND s.workspace_id=$4 FOR SHARE OF o")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(operation_id.as_str())
                    .bind(workspace).fetch_optional(&mut **tx).await.map_err(sql_error)?;
                let Some(row) = row else {
                    return Ok(None);
                };
                let Some(raw) = row
                    .try_get::<Option<Value>, _>("completion")
                    .map_err(sql_error)?
                else {
                    return Ok(None);
                };
                let completion: OperationCompletion =
                    serde_json::from_value(raw.clone()).map_err(decode_error)?;
                completion.validate().map_err(app_error)?;
                let digest = zuno_orchestration::sha256_json(&raw);
                if row
                    .try_get::<Option<String>, _>("completion_digest")
                    .map_err(sql_error)?
                    .as_deref()
                    != Some(&digest)
                    || completion.lease.owner != self.principal.owner()
                    || completion.operation.id != *operation_id
                    || completion.receipt.phase != OperationPhase::Completed
                    || completion.receipt.exit_code != Some(0)
                    || completion.receipt.cancellation_requested
                    || completion.output_truncated
                {
                    return Ok(None);
                }
                // Keep chunk boundaries; joining unrelated stdout/stderr fragments
                // must not manufacture an excerpt which never existed.
                let text = completion
                    .output
                    .iter()
                    .map(|chunk| std::str::from_utf8(&chunk.bytes))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(decode_error)?
                    .join("\n");
                Source {
                    digest,
                    text,
                    session: row.try_get("session_id").map_err(sql_error)?,
                    user_authored: false,
                }
            }
        };
        Ok(Some(result))
    }

    pub(super) async fn record_evidence(
        &self,
        tx: &mut Tx,
        origin: MemoryEvidenceOrigin,
        excerpt: &str,
    ) -> Result<MemoryEvidenceReference, Error> {
        if excerpt.trim().is_empty() || excerpt.len() > 2048 {
            return Err(invalid("Memory excerpt must contain 1–2048 bytes"));
        }
        let source = self
            .source(tx, &origin, self.workspace.as_str())
            .await?
            .ok_or(Error::Denied)?;
        if !source.text.contains(excerpt) {
            return Err(invalid(
                "Memory excerpt is absent from its authoritative source",
            ));
        }
        let digest = zuno_orchestration::sha256_json(&json!([
            self.principal.owner(),
            self.workspace,
            origin,
            source.digest,
            excerpt
        ]));
        let id = format!("evidence_{digest}");
        let now = database_time(tx).await.map_err(app_error)?;
        query("INSERT INTO zuno_enterprise_preview.memory_evidence(tenant_id,principal_id,id,workspace_id,origin,excerpt,digest,source_digest,time_created)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(tenant_id,principal_id,id) DO NOTHING")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(&id).bind(self.workspace.as_str())
            .bind(json!(origin)).bind(excerpt).bind(&digest).bind(source.digest).bind(now).execute(&mut **tx).await.map_err(sql_error)?;
        // Explicit forgetting is not undone by a retried capture.
        let forgotten: bool = query_scalar("SELECT forgotten FROM zuno_enterprise_preview.memory_evidence WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(&id)
            .fetch_one(&mut **tx).await.map_err(sql_error)?;
        if forgotten {
            return Err(Error::Conflict);
        }
        Ok(MemoryEvidenceReference {
            experience_id: id,
            digest,
        })
    }

    pub(super) async fn reference_source(
        &self,
        tx: &mut Tx,
        reference: &MemoryEvidenceReference,
        for_write: bool,
    ) -> Result<Option<Source>, Error> {
        let row = query("SELECT workspace_id,origin,excerpt,digest,source_digest,forgotten FROM zuno_enterprise_preview.memory_evidence
            WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(&reference.experience_id)
            .fetch_optional(&mut **tx).await.map_err(sql_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<bool, _>("forgotten").map_err(sql_error)?
            || reference.digest != row.try_get::<String, _>("digest").map_err(sql_error)?
        {
            return Ok(None);
        }
        let workspace: String = row.try_get("workspace_id").map_err(sql_error)?;
        if for_write && workspace != self.workspace.as_str() {
            return Ok(None);
        }
        let origin: MemoryEvidenceOrigin =
            serde_json::from_value(row.try_get("origin").map_err(sql_error)?)
                .map_err(decode_error)?;
        let Some(source) = self.source(tx, &origin, &workspace).await? else {
            return Ok(None);
        };
        let excerpt: String = row.try_get("excerpt").map_err(sql_error)?;
        if source.digest
            != row
                .try_get::<String, _>("source_digest")
                .map_err(sql_error)?
            || !source.text.contains(&excerpt)
            || reference.digest
                != zuno_orchestration::sha256_json(&json!([
                    self.principal.owner(),
                    workspace,
                    origin,
                    source.digest,
                    excerpt
                ]))
        {
            return Ok(None);
        }
        if for_write {
            self.require_generation(tx, Some(&source.session)).await?;
        }
        Ok(Some(source))
    }

    pub(super) async fn references_current(
        &self,
        tx: &mut Tx,
        references: &[MemoryEvidenceReference],
        for_write: bool,
    ) -> Result<bool, Error> {
        if references.is_empty() || references.len() > 128 {
            return Ok(false);
        }
        for reference in references {
            if self
                .reference_source(tx, reference, for_write)
                .await?
                .is_none()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
