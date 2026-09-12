use super::*;
use zuno_memory::remote::MemoryPolicy;

impl TransactionMemory {
    pub(super) async fn ensure_session(&self, tx: &mut Tx, session: &str) -> Result<(), Error> {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.session_id.as_str() != session)
        {
            return Err(Error::Denied);
        }
        let found: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3 AND workspace_id=$4)",
        ).bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
            .bind(session).bind(self.workspace.as_str()).fetch_one(&mut **tx).await.map_err(sql_error)?;
        if found { Ok(()) } else { Err(Error::Denied) }
    }

    pub(super) async fn policy(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
    ) -> Result<MemoryPolicy, Error> {
        let row = if let Some(session) = session {
            self.ensure_session(tx, session).await?;
            query("SELECT revision,use_memories,generate_private FROM zuno_enterprise_preview.session_memory_policy
                WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?
        } else {
            query("SELECT revision,use_memories,generate_private FROM zuno_enterprise_preview.memory_policy
                WHERE tenant_id=$1 AND principal_id=$2")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                .fetch_optional(&mut **tx).await.map_err(sql_error)?
        };
        row.map(|row| {
            Ok(MemoryPolicy {
                revision: u64::try_from(row.try_get::<i64, _>("revision").map_err(sql_error)?)
                    .map_err(decode_error)?,
                use_memories: row.try_get("use_memories").map_err(sql_error)?,
                generate_private: row.try_get("generate_private").map_err(sql_error)?,
            })
        })
        .transpose()
        .map(|policy| policy.unwrap_or_default())
    }

    pub(super) async fn use_enabled(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
    ) -> Result<bool, Error> {
        let owner = self.policy(tx, None).await?;
        if let Some(session) = session {
            Ok(owner.use_memories && self.policy(tx, Some(session)).await?.use_memories)
        } else {
            Ok(owner.use_memories)
        }
    }

    pub(super) async fn require_generation(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
    ) -> Result<(), Error> {
        let owner = self.policy(tx, None).await?;
        if !owner.generate_private {
            return Err(Error::Denied);
        }
        if let Some(session) = session {
            let policy = self.policy(tx, Some(session)).await?;
            // Absent session override inherits explicit owner consent. A session
            // can narrow that consent; it cannot widen the owner's decision.
            if policy.revision > 0 && !policy.generate_private {
                return Err(Error::Denied);
            }
        }
        Ok(())
    }

    pub(super) async fn set_policy(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
        expected: u64,
        use_memories: bool,
        generate_private: bool,
    ) -> Result<MemoryPolicy, Error> {
        self.host_only()?;
        if self.policy(tx, session).await?.revision != expected {
            return Err(Error::Conflict);
        }
        let revision = expected
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(Error::Conflict)?;
        if let Some(session) = session {
            query("INSERT INTO zuno_enterprise_preview.session_memory_policy(tenant_id,principal_id,session_id,revision,use_memories,generate_private)
                VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(tenant_id,principal_id,session_id) DO UPDATE SET
                  revision=excluded.revision,use_memories=excluded.use_memories,generate_private=excluded.generate_private")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
                .bind(revision).bind(use_memories).bind(generate_private).execute(&mut **tx).await.map_err(sql_error)?;
        } else {
            query("INSERT INTO zuno_enterprise_preview.memory_policy(tenant_id,principal_id,revision,use_memories,generate_private)
                VALUES($1,$2,$3,$4,$5) ON CONFLICT(tenant_id,principal_id) DO UPDATE SET
                  revision=excluded.revision,use_memories=excluded.use_memories,generate_private=excluded.generate_private")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                .bind(revision).bind(use_memories).bind(generate_private).execute(&mut **tx).await.map_err(sql_error)?;
        }
        if !generate_private {
            let now = database_time(tx).await.map_err(app_error)?;
            query("UPDATE zuno_enterprise_preview.learning_job SET status='skipped',
                result='{\"reason\":\"generation_disabled\"}',time_updated=$4
                WHERE tenant_id=$1 AND principal_id=$2 AND ($3::text IS NULL OR session_id=$3)
                  AND status='queued' AND (kind='extraction' OR (kind='project_aggregation' AND payload->>'purpose'='memory'))")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
                .bind(now).execute(&mut **tx).await.map_err(sql_error)?;
        }
        self.policy(tx, session).await
    }
}
