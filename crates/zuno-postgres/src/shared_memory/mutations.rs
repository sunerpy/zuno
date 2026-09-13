use super::*;

impl PostgresSharedMemoryStore {
    pub(super) async fn configure_space(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ConfigureSharedMemory,
    ) -> Result<SharedMemorySpace, ApplicationError> {
        request.validate()?;
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        if actor(&mut tx, principal, true).await? != OrganizationRole::Administrator {
            return Err(ApplicationError::Forbidden);
        }
        query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(digest(&json!([
                "shared-memory-configuration",
                principal.tenant_id(),
                request.workspace_id
            ])))
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
        lock(&mut tx, principal, id).await?;
        let hash = digest(&json!(["configure", id, request]));
        if let Some(value) = prior(&mut tx, principal, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let old:Option<i64>=query_scalar("SELECT policy_revision FROM zuno_enterprise_preview.shared_memory_space WHERE tenant_id=$1 AND id=$2 FOR UPDATE")
            .bind(principal.tenant_id().as_str()).bind(id.as_str()).fetch_optional(&mut *tx).await.map_err(database_error)?;
        if old.unwrap_or(0) != number(request.expected_revision.0)? {
            return Err(ApplicationError::Conflict);
        }
        if old.is_some() {
            let current = read(&mut tx, principal, id, true).await?;
            if current.workspace_id != request.workspace_id
                || zuno_memory::scope::char_count(&zuno_memory::render::serialize(&current.entries))
                    > request.character_limit as usize
            {
                return Err(ApplicationError::Conflict);
            }
            query(
                "UPDATE zuno_enterprise_preview.shared_memory_space SET title=$3,enabled=$4,
                policy_revision=policy_revision+1,character_limit=$5 WHERE tenant_id=$1 AND id=$2",
            )
            .bind(principal.tenant_id().as_str())
            .bind(id.as_str())
            .bind(&request.title)
            .bind(request.enabled)
            .bind(request.character_limit as i32)
            .execute(&mut *tx)
            .await
            .map_err(database_error)?;
            query("DELETE FROM zuno_enterprise_preview.shared_memory_member WHERE tenant_id=$1 AND space_id=$2")
                .bind(principal.tenant_id().as_str()).bind(id.as_str()).execute(&mut *tx).await.map_err(database_error)?;
        } else {
            let count:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.shared_memory_space WHERE tenant_id=$1 AND workspace_id=$2")
                .bind(principal.tenant_id().as_str()).bind(request.workspace_id.as_str())
                .fetch_one(&mut *tx).await.map_err(database_error)?;
            if count >= 32 {
                return Err(ApplicationError::Invalid(
                    "configure at most 32 shared Memory spaces per workspace".to_owned(),
                ));
            }
            query("INSERT INTO zuno_enterprise_preview.shared_memory_space
                (tenant_id,id,workspace_id,title,enabled,policy_revision,document_revision,entries,content_digest,character_limit)
                VALUES($1,$2,$3,$4,$5,1,1,'[]',$6,$7)")
                .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(request.workspace_id.as_str()).bind(&request.title)
                .bind(request.enabled).bind(digest(&Vec::<String>::new())).bind(request.character_limit as i32)
                .execute(&mut *tx).await.map_err(database_error)?;
            revision(&mut tx, principal, id, 1, &[], "create", None).await?;
        }
        for member in &request.members {
            // The tenant administrator may assign namespace roles only to
            // existing, active organization members.
            crate::set_owner(
                &mut tx,
                &zuno_types::identity::PrincipalKey {
                    tenant_id: principal.tenant_id().clone(),
                    principal_id: member.principal_id.clone(),
                },
            )
            .await?;
            let access = crate::authorization::access_in(
                &mut tx,
                &zuno_types::identity::PrincipalKey {
                    tenant_id: principal.tenant_id().clone(),
                    principal_id: member.principal_id.clone(),
                },
            )
            .await?;
            crate::set_owner(&mut tx, &principal.owner()).await?;
            if !access.member.active {
                return Err(ApplicationError::Forbidden);
            }
            query("INSERT INTO zuno_enterprise_preview.shared_memory_member(tenant_id,space_id,principal_id,role) VALUES($1,$2,$3,$4)")
                .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(member.principal_id.as_str()).bind(text(&member.role)?)
                .execute(&mut *tx).await.map_err(database_error)?;
        }
        let pending: Vec<String> = query_scalar(
            "SELECT id FROM zuno_enterprise_preview.shared_memory_change
            WHERE tenant_id=$1 AND space_id=$2 AND state='pending' ORDER BY id",
        )
        .bind(principal.tenant_id().as_str())
        .bind(id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(database_error)?;
        for candidate in pending {
            let mut candidate = change(
                &mut tx,
                principal,
                id,
                &RequestId::new(candidate).map_err(ApplicationError::storage)?,
            )
            .await?;
            candidate.state = SharedMemoryChangeState::Invalidated;
            candidate.state_digest = change_digest(&candidate);
            query("UPDATE zuno_enterprise_preview.shared_memory_change SET state='invalidated',data=$4,state_digest=$5
                WHERE tenant_id=$1 AND space_id=$2 AND id=$3")
                .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(candidate.id.as_str())
                .bind(json!(candidate)).bind(&candidate.state_digest).execute(&mut *tx).await.map_err(database_error)?;
        }
        let value = read(&mut tx, principal, id, true).await?;
        receipt(
            &mut tx,
            principal,
            id,
            &request.request_id,
            "configure",
            &value,
            &hash,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    pub(super) async fn propose_change(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ProposeSharedMemory,
    ) -> Result<SharedMemoryChange, ApplicationError> {
        request.validate()?;
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, false).await? == OrganizationRole::Administrator;
        lock(&mut tx, principal, id).await?;
        let current = read(&mut tx, principal, id, admin).await?;
        if !current.enabled || current.role == SharedMemoryRole::Reader {
            return Err(ApplicationError::Forbidden);
        }
        let hash = digest(&json!(["propose", id, request]));
        if let Some(value) = prior(&mut tx, principal, &request.request_id, &hash).await? {
            return Ok(value);
        }
        if request.expected_revision != current.document_revision {
            return Err(ApplicationError::Conflict);
        }
        let pending: i64 = query_scalar(
            "SELECT count(*) FROM zuno_enterprise_preview.shared_memory_change
            WHERE tenant_id=$1 AND space_id=$2 AND state='pending'",
        )
        .bind(principal.tenant_id().as_str())
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(database_error)?;
        if pending >= 256 {
            return Err(ApplicationError::Invalid(
                "review pending shared Memory changes before submitting more".to_owned(),
            ));
        }
        let operations = request
            .edits
            .iter()
            .map(|edit| match edit {
                SharedMemoryEdit::Add { content } => zuno_memory::Operation::add(content),
                SharedMemoryEdit::Replace { old_text, content } => {
                    zuno_memory::Operation::replace(old_text, content)
                }
                SharedMemoryEdit::Remove { old_text } => zuno_memory::Operation::remove(old_text),
            })
            .collect::<Vec<_>>();
        // The same entry validator used by private/local Memory owns threat,
        // unique locator, deduplication and final character-cap semantics.
        let after = zuno_memory::store::preview_entries(
            zuno_memory::Scope::Project,
            current.character_limit as usize,
            &current.entries,
            &operations,
        )
        .map_err(|_| invalid())?;
        let mut value = SharedMemoryChange {
            id: RequestId::new(format!(
                "smc_{}",
                digest(&json!([principal.owner(), id, request.request_id]))
            ))
            .map_err(ApplicationError::storage)?,
            space_id: id.clone(),
            author: principal.principal_id().clone(),
            base_revision: current.document_revision,
            policy_revision: current.policy_revision,
            before: current.entries,
            after,
            reason: request.reason.clone(),
            state: SharedMemoryChangeState::Pending,
            state_digest: String::new(),
            decided_by: None,
            applied_revision: None,
        };
        value.state_digest = change_digest(&value);
        query(
            "INSERT INTO zuno_enterprise_preview.shared_memory_change
            (tenant_id,space_id,id,author,base_revision,policy_revision,state,data,state_digest)
            VALUES($1,$2,$3,$4,$5,$6,'pending',$7,$8)",
        )
        .bind(principal.tenant_id().as_str())
        .bind(id.as_str())
        .bind(value.id.as_str())
        .bind(principal.principal_id().as_str())
        .bind(number(value.base_revision.0)?)
        .bind(number(value.policy_revision.0)?)
        .bind(json!(value))
        .bind(&value.state_digest)
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
        receipt(
            &mut tx,
            principal,
            id,
            &request.request_id,
            "propose",
            &value,
            &hash,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    pub(super) async fn review_change(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ReviewSharedMemory,
    ) -> Result<SharedMemoryChange, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, true).await? == OrganizationRole::Administrator;
        lock(&mut tx, principal, id).await?;
        let current = read(&mut tx, principal, id, admin).await?;
        if !current.enabled || current.role != SharedMemoryRole::Reviewer {
            return Err(ApplicationError::Forbidden);
        }
        let hash = digest(&json!(["review", id, request]));
        if let Some(value) = prior(&mut tx, principal, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let mut value = change(&mut tx, principal, id, &request.change_id).await?;
        if value.author == *principal.principal_id() {
            return Err(ApplicationError::Forbidden);
        }
        if value.state_digest != request.expected_state
            || value.policy_revision != current.policy_revision
        {
            return Err(ApplicationError::Conflict);
        }
        let (state, entries, operation) = match request.decision {
            SharedMemoryDecision::Apply
                if value.state == SharedMemoryChangeState::Pending
                    && value.base_revision == current.document_revision
                    && value.before == current.entries =>
            {
                (
                    SharedMemoryChangeState::Applied,
                    Some(value.after.clone()),
                    "apply",
                )
            }
            SharedMemoryDecision::Reject if value.state == SharedMemoryChangeState::Pending => {
                (SharedMemoryChangeState::Rejected, None, "reject")
            }
            SharedMemoryDecision::Undo
                if value.state == SharedMemoryChangeState::Applied
                    && value.applied_revision == Some(current.document_revision)
                    && value.after == current.entries =>
            {
                (
                    SharedMemoryChangeState::Undone,
                    Some(value.before.clone()),
                    "undo",
                )
            }
            _ => return Err(ApplicationError::Conflict),
        };
        if let Some(entries) = entries {
            let next = current
                .document_revision
                .0
                .checked_add(1)
                .ok_or_else(invalid)?;
            query("UPDATE zuno_enterprise_preview.shared_memory_space SET entries=$3,content_digest=$4,document_revision=$5
                WHERE tenant_id=$1 AND id=$2 AND document_revision=$6")
                .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(json!(entries)).bind(digest(&entries))
                .bind(number(next)?).bind(number(current.document_revision.0)?).execute(&mut *tx).await.map_err(database_error)?;
            revision(
                &mut tx,
                principal,
                id,
                next,
                &entries,
                operation,
                Some(&value.id),
            )
            .await?;
            if state == SharedMemoryChangeState::Applied {
                value.applied_revision = Some(Counter(next));
            }
        }
        value.state = state;
        value.decided_by = Some(principal.principal_id().clone());
        value.state_digest = change_digest(&value);
        query("UPDATE zuno_enterprise_preview.shared_memory_change SET state=$4,data=$5,state_digest=$6
            WHERE tenant_id=$1 AND space_id=$2 AND id=$3")
            .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(value.id.as_str())
            .bind(text(&state)?).bind(json!(value)).bind(&value.state_digest).execute(&mut *tx).await.map_err(database_error)?;
        receipt(
            &mut tx,
            principal,
            id,
            &request.request_id,
            operation,
            &value,
            &hash,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
}
async fn revision(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    id: &MemorySpaceId,
    version: u64,
    entries: &[String],
    operation: &str,
    change: Option<&RequestId>,
) -> Result<(), ApplicationError> {
    let now = database_time(tx).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.shared_memory_revision
        (tenant_id,space_id,revision,entries,content_digest,change_id,actor,operation,time_created)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(principal.tenant_id().as_str())
    .bind(id.as_str())
    .bind(number(version)?)
    .bind(json!(entries))
    .bind(digest(&entries))
    .bind(change.map(RequestId::as_str))
    .bind(principal.principal_id().as_str())
    .bind(operation)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    Ok(())
}
