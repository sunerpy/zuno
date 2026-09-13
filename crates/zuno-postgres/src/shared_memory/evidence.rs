use super::*;
use zuno_memory::remote::MemoryEvidenceOrigin;
use zuno_types::identity::{PrincipalId, PrincipalKey};

async fn private_evidence(
    tx: &mut Tx<'_>,
    owner: &PrincipalKey,
    workspace: &WorkspaceId,
    id: &str,
    expected: &str,
) -> Result<Option<(String, SharedEvidenceKind)>, ApplicationError> {
    let row=query("SELECT origin,excerpt,digest,source_digest,forgotten FROM zuno_enterprise_preview.memory_evidence
        WHERE tenant_id=$1 AND principal_id=$2 AND workspace_id=$3 AND id=$4 FOR SHARE")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(workspace.as_str()).bind(id)
        .fetch_optional(&mut **tx).await.map_err(database_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row
        .try_get::<bool, _>("forgotten")
        .map_err(database_error)?
        || row.try_get::<String, _>("digest").map_err(database_error)? != expected
    {
        return Ok(None);
    }
    let origin: MemoryEvidenceOrigin =
        serde_json::from_value(row.try_get("origin").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let source = crate::memory::evidence::source_in(tx, owner, &origin, workspace.as_str())
        .await
        .map_err(ApplicationError::storage)?;
    let Some(source) = source else {
        return Ok(None);
    };
    let excerpt: String = row.try_get("excerpt").map_err(database_error)?;
    if source.digest
        != row
            .try_get::<String, _>("source_digest")
            .map_err(database_error)?
        || !source.text.contains(&excerpt)
        || digest(&json!([owner, workspace, origin, source.digest, excerpt])) != expected
    {
        return Ok(None);
    }
    Ok(Some((
        excerpt,
        if source.user_authored {
            SharedEvidenceKind::UserStatement
        } else {
            SharedEvidenceKind::SuccessfulOperation
        },
    )))
}
pub(super) async fn grant(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    space: &MemorySpaceId,
    id: &RequestId,
) -> Result<SharedEvidenceGrant, ApplicationError> {
    let row=query("SELECT * FROM zuno_enterprise_preview.shared_memory_evidence WHERE tenant_id=$1 AND space_id=$2 AND id=$3 FOR SHARE")
        .bind(principal.tenant_id().as_str()).bind(space.as_str()).bind(id.as_str())
        .fetch_optional(&mut **tx).await.map_err(database_error)?.ok_or(ApplicationError::NotFound)?;
    let author = PrincipalId::new(
        row.try_get::<String, _>("principal_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    let actor: PrincipalScope =
        serde_json::from_value(row.try_get("actor").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let workspace = WorkspaceId::new(
        row.try_get::<String, _>("workspace_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    let active: bool = row.try_get("active").map_err(database_error)?;
    let evidence_id: String = row.try_get("evidence_id").map_err(database_error)?;
    let expected: String = row.try_get("evidence_digest").map_err(database_error)?;
    let excerpt: String = row.try_get("excerpt").map_err(database_error)?;
    let kind: SharedEvidenceKind = serde_json::from_value(
        row.try_get::<String, _>("source_kind")
            .map_err(database_error)?
            .into(),
    )
    .map_err(ApplicationError::storage)?;
    if actor.principal_id() != &author || actor.tenant_id() != principal.tenant_id() {
        return Err(ApplicationError::Conflict);
    }
    // The grant is selected under the caller's namespace policy before any
    // private read. Only a validity bit and the already shared excerpt escape.
    let mut current = false;
    if active {
        crate::set_owner(tx, &actor.owner()).await?;
        let result = async {
            let access = crate::authorization::access_in(tx, &actor.owner()).await?;
            if actor_denial(&access.policy, &access.member, &actor).is_some()
                || actor
                    .client_id()
                    .is_none_or(|id| !access.policy.approval_apps.contains(id))
            {
                return Ok(false);
            }
            if role(tx, &actor, space, false).await.is_err() {
                return Ok(false);
            }
            let found =
                private_evidence(tx, &actor.owner(), &workspace, &evidence_id, &expected).await?;
            Ok::<_, ApplicationError>(
                found.is_some_and(|value| value.0 == excerpt && value.1 == kind),
            )
        }
        .await;
        crate::set_owner(tx, &principal.owner()).await?;
        current = result?;
    }
    Ok(SharedEvidenceGrant {
        id: id.clone(),
        space_id: space.clone(),
        author,
        revision: counter(row.try_get("revision").map_err(database_error)?)?,
        kind,
        excerpt,
        evidence_digest: expected,
        active,
        current,
    })
}
#[async_trait]
impl SharedEvidenceStore for PostgresSharedMemoryStore {
    async fn share(
        &self,
        principal: &PrincipalScope,
        id: &MemorySpaceId,
        request: ShareMemoryEvidence,
    ) -> Result<SharedEvidenceGrant, ApplicationError> {
        if request.evidence_id.is_empty()
            || request.evidence_id.len() > 128
            || request.expected_digest.len() != 64
        {
            return Err(invalid());
        }
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, true).await? == OrganizationRole::Administrator;
        lock(&mut tx, principal, id).await?;
        let space = read(&mut tx, principal, id, admin).await?;
        if !space.enabled || space.role == SharedMemoryRole::Reader {
            return Err(ApplicationError::Forbidden);
        }
        let hash = digest(&json!(["share-evidence", id, request]));
        if let Some(value) = prior(&mut tx, principal, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let (excerpt, kind) = private_evidence(
            &mut tx,
            &principal.owner(),
            &space.workspace_id,
            &request.evidence_id,
            &request.expected_digest,
        )
        .await?
        .ok_or(ApplicationError::Forbidden)?;
        let count:i64=query_scalar("SELECT count(*) FROM zuno_enterprise_preview.shared_memory_evidence WHERE tenant_id=$1 AND space_id=$2")
            .bind(principal.tenant_id().as_str()).bind(id.as_str()).fetch_one(&mut *tx).await.map_err(database_error)?;
        if count >= 1024 {
            return Err(ApplicationError::Invalid(
                "shared evidence grant capacity reached".to_owned(),
            ));
        }
        let grant_id = RequestId::new(format!(
            "sme_{}",
            digest(&json!([principal.owner(), id, request.request_id]))
        ))
        .map_err(ApplicationError::storage)?;
        query("INSERT INTO zuno_enterprise_preview.shared_memory_evidence
            (tenant_id,space_id,id,principal_id,workspace_id,evidence_id,evidence_digest,excerpt,source_kind,actor,revision,active)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,1,true)")
            .bind(principal.tenant_id().as_str()).bind(id.as_str()).bind(grant_id.as_str()).bind(principal.principal_id().as_str())
            .bind(space.workspace_id.as_str()).bind(&request.evidence_id).bind(&request.expected_digest).bind(excerpt).bind(text(&kind)?)
            .bind(json!(principal)).execute(&mut *tx).await.map_err(database_error)?;
        let value = grant(&mut tx, principal, id, &grant_id).await?;
        evidence_receipt(
            &mut tx,
            principal,
            id,
            &request.request_id,
            "share-evidence",
            &value,
            &hash,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn revoke(
        &self,
        principal: &PrincipalScope,
        space: &MemorySpaceId,
        id: &RequestId,
        request: RevokeSharedEvidence,
    ) -> Result<SharedEvidenceGrant, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        actor(&mut tx, principal, true).await?;
        lock(&mut tx, principal, space).await?;
        let hash = digest(&json!(["revoke-evidence", space, id, request]));
        if let Some(value) = prior(&mut tx, principal, &request.request_id, &hash).await? {
            return Ok(value);
        }
        let old = grant(&mut tx, principal, space, id).await?;
        if old.author != *principal.principal_id() {
            return Err(ApplicationError::Forbidden);
        }
        if old.revision != request.expected_revision {
            return Err(ApplicationError::Conflict);
        }
        let next = old.revision.0.checked_add(1).ok_or_else(invalid)?;
        query("UPDATE zuno_enterprise_preview.shared_memory_evidence SET active=false,revision=$4 WHERE tenant_id=$1 AND space_id=$2 AND id=$3")
            .bind(principal.tenant_id().as_str()).bind(space.as_str()).bind(id.as_str()).bind(number(next)?)
            .execute(&mut *tx).await.map_err(database_error)?;
        let value = grant(&mut tx, principal, space, id).await?;
        evidence_receipt(
            &mut tx,
            principal,
            space,
            &request.request_id,
            "revoke-evidence",
            &value,
            &hash,
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        Ok(value)
    }
    async fn list(
        &self,
        principal: &PrincipalScope,
        space: &MemorySpaceId,
        after: Option<&RequestId>,
        limit: PageSize,
    ) -> Result<SharedEvidencePage, ApplicationError> {
        let mut tx = owner_transaction(&self.backend.pool, &principal.owner()).await?;
        let admin = actor(&mut tx, principal, false).await? == OrganizationRole::Administrator;
        read(&mut tx, principal, space, admin).await?;
        let ids:Vec<String>=query_scalar("SELECT id FROM zuno_enterprise_preview.shared_memory_evidence WHERE tenant_id=$1 AND space_id=$2 AND ($3::text IS NULL OR id>$3) ORDER BY id LIMIT $4")
            .bind(principal.tenant_id().as_str()).bind(space.as_str()).bind(after.map(RequestId::as_str)).bind(i64::from(limit.get())+1)
            .fetch_all(&mut *tx).await.map_err(database_error)?;
        let more = ids.len() > usize::from(limit.get());
        let mut items = Vec::new();
        for id in ids.into_iter().take(usize::from(limit.get())) {
            items.push(
                grant(
                    &mut tx,
                    principal,
                    space,
                    &RequestId::new(id).map_err(ApplicationError::storage)?,
                )
                .await?,
            );
        }
        let after = more.then(|| items.last().map(|g| g.id.clone())).flatten();
        tx.commit().await.map_err(database_error)?;
        Ok(SharedEvidencePage { items, after })
    }
}
async fn evidence_receipt(
    tx: &mut Tx<'_>,
    principal: &PrincipalScope,
    space: &MemorySpaceId,
    request: &RequestId,
    operation: &str,
    value: &SharedEvidenceGrant,
    hash: &str,
) -> Result<(), ApplicationError> {
    query("INSERT INTO zuno_enterprise_preview.shared_memory_request(tenant_id,principal_id,request_id,request_digest,response) VALUES($1,$2,$3,$4,$5)")
        .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.as_str()).bind(hash)
        .bind(json!(value)).execute(&mut **tx).await.map_err(database_error)?;
    let now = database_time(tx).await?;
    query("INSERT INTO zuno_enterprise_preview.shared_memory_evidence_audit(tenant_id,principal_id,request_id,space_id,operation,data,time_created)
        VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(principal.tenant_id().as_str()).bind(principal.principal_id().as_str()).bind(request.as_str()).bind(space.as_str())
        .bind(operation).bind(json!({"actor":principal,"result":value,"requestDigest":hash})).bind(now)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}
