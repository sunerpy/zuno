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
            query("SELECT revision,use_memories,generate_private,automatic_private FROM zuno_enterprise_preview.session_memory_policy
                WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?
        } else {
            query("SELECT revision,use_memories,generate_private,automatic_private FROM zuno_enterprise_preview.memory_policy
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
                automatic_private: row.try_get("automatic_private").map_err(sql_error)?,
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
        let before = self.policy(tx, session).await?;
        if before.revision != expected {
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
            if let Some(session) = session {
                query("UPDATE zuno_enterprise_preview.session_memory_policy SET automatic_private=false
                    WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
                    .execute(&mut **tx).await.map_err(sql_error)?;
            } else {
                query("UPDATE zuno_enterprise_preview.memory_policy SET automatic_private=false,automation_actor=NULL,automation_since=NULL
                    WHERE tenant_id=$1 AND principal_id=$2")
                    .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                    .execute(&mut **tx).await.map_err(sql_error)?;
            }
            self.stop_automatic_jobs(tx, session, "generation_disabled")
                .await?;
        }
        self.policy(tx, session).await
    }

    pub(super) async fn require_automation(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
    ) -> Result<(), Error> {
        self.require_generation(tx, session).await?;
        let owner = self.policy(tx, None).await?;
        if !owner.automatic_private {
            return Err(Error::Denied);
        }
        if let Some(session) = session {
            let policy = self.policy(tx, Some(session)).await?;
            if policy.revision > 0 && !policy.automatic_private {
                return Err(Error::Denied);
            }
        }
        let actor: Option<Value> = query_scalar(
            "SELECT automation_actor FROM zuno_enterprise_preview.memory_policy
             WHERE tenant_id=$1 AND principal_id=$2",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(sql_error)?
        .flatten();
        let actor: PrincipalScope =
            serde_json::from_value(actor.ok_or(Error::Denied)?).map_err(decode_error)?;
        if actor.owner() != self.principal.owner() {
            return Err(Error::Denied);
        }
        let access = crate::authorization::access_in(tx, &actor.owner())
            .await
            .map_err(app_error)?;
        if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, &actor)
            .is_some()
            || !zuno_permission::enterprise::can_approve(
                &access.policy,
                zuno_permission::enterprise::ApprovalAudience::Requester,
                &self.principal.owner(),
                &actor,
                &access.member,
            )
        {
            return Err(Error::Denied);
        }
        Ok(())
    }

    pub(super) async fn set_automation(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
        expected: u64,
        enabled: bool,
    ) -> Result<MemoryPolicy, Error> {
        self.host_only()?;
        let policy = self.policy(tx, session).await?;
        if policy.revision != expected {
            return Err(Error::Conflict);
        }
        if enabled {
            self.require_generation(tx, session).await?;
        }
        let revision = expected
            .checked_add(1)
            .and_then(|n| i64::try_from(n).ok())
            .ok_or(Error::Conflict)?;
        if let Some(session) = session {
            // An absent override inherits generation before introducing its
            // independently restrictive automation choice.
            let generation = if policy.revision == 0 {
                self.policy(tx, None).await?.generate_private
            } else {
                policy.generate_private
            };
            query("INSERT INTO zuno_enterprise_preview.session_memory_policy
                (tenant_id,principal_id,session_id,revision,use_memories,generate_private,automatic_private)
                VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(tenant_id,principal_id,session_id) DO UPDATE SET
                revision=excluded.revision,automatic_private=excluded.automatic_private")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
                .bind(revision).bind(policy.use_memories).bind(generation).bind(enabled)
                .execute(&mut **tx).await.map_err(sql_error)?;
        } else {
            let now = database_time(tx).await.map_err(app_error)?;
            query("INSERT INTO zuno_enterprise_preview.memory_policy
                (tenant_id,principal_id,revision,use_memories,generate_private,automatic_private,automation_actor,automation_since)
                VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(tenant_id,principal_id) DO UPDATE SET
                revision=excluded.revision,automatic_private=excluded.automatic_private,automation_actor=excluded.automation_actor,
                automation_since=CASE WHEN excluded.automatic_private THEN COALESCE(memory_policy.automation_since,excluded.automation_since) ELSE NULL END")
                .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str())
                .bind(revision).bind(policy.use_memories).bind(policy.generate_private).bind(enabled)
                .bind(enabled.then(|| json!(self.principal)))
                .bind(enabled.then_some(now))
                .execute(&mut **tx).await.map_err(sql_error)?;
        }
        if !enabled {
            self.stop_automatic_jobs(tx, session, "automation_disabled")
                .await?;
        }
        self.policy(tx, session).await
    }

    async fn stop_automatic_jobs(
        &self,
        tx: &mut Tx,
        session: Option<&str>,
        reason: &str,
    ) -> Result<(), Error> {
        let now = database_time(tx).await.map_err(app_error)?;
        let jobs: Vec<String> = query_scalar("UPDATE zuno_enterprise_preview.learning_job SET status='skipped',
            owner_id=NULL,lease_token=NULL,lease_expires=NULL,result=$5,time_updated=$4
            WHERE tenant_id=$1 AND principal_id=$2 AND ($3::text IS NULL OR session_id=$3)
              AND status IN('queued','running')
              AND (kind='extraction' OR (kind='project_aggregation' AND payload->>'purpose'='memory'))
            RETURNING id")
            .bind(self.principal.tenant_id().as_str()).bind(self.principal.principal_id().as_str()).bind(session)
            .bind(now).bind(json!({"reason":reason})).fetch_all(&mut **tx).await.map_err(sql_error)?;
        query(
            "UPDATE zuno_enterprise_preview.learning_model_request SET state='unknown'
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=ANY($3) AND state='prepared'",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(&jobs)
        .execute(&mut **tx)
        .await
        .map_err(sql_error)?;
        query(
            "UPDATE zuno_enterprise_preview.learning_execution
            SET charged_tokens=charged_tokens+reserved_tokens,reserved_tokens=0
            WHERE tenant_id=$1 AND principal_id=$2 AND job_id=ANY($3)",
        )
        .bind(self.principal.tenant_id().as_str())
        .bind(self.principal.principal_id().as_str())
        .bind(jobs)
        .execute(&mut **tx)
        .await
        .map_err(sql_error)?;
        Ok(())
    }
}
