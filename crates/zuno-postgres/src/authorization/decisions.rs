use super::*;

fn visible(access: &OrganizationAccess, actor: &PrincipalScope, record: &ApprovalRecord) -> bool {
    actor.owner() == record.requester.owner()
        || access.member.role == OrganizationRole::Administrator
        || (access.member.role == OrganizationRole::Approver
            && record.audience == ApprovalAudience::DesignatedApprover)
}

pub(super) async fn read(
    store: &PostgresOrganizationStore,
    viewer: &PrincipalScope,
    id: &ApprovalId,
) -> Result<ApprovalRecord, ApplicationError> {
    store.check_tenant(viewer.tenant_id())?;
    let mut tx = owner_transaction(&store.pool, &viewer.owner()).await?;
    let record = read_in(&mut tx, viewer, id).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(record)
}

pub(crate) async fn read_in(
    tx: &mut Transaction<'_, Postgres>,
    viewer: &PrincipalScope,
    id: &ApprovalId,
) -> Result<ApprovalRecord, ApplicationError> {
    let access = access_in(tx, &viewer.owner()).await?;
    if !viewer_active(&access, viewer) {
        return Err(ApplicationError::Forbidden);
    }
    let (owner, _) = coordinates(tx, viewer.tenant_id(), id).await?;
    if owner != viewer.owner()
        && !matches!(
            access.member.role,
            OrganizationRole::Approver | OrganizationRole::Administrator
        )
    {
        return Err(ApplicationError::NotFound);
    }
    set_owner(tx, &owner).await?;
    let record = record_in(tx, &owner, id.as_str(), false).await?;
    if !visible(&access, viewer, &record) {
        return Err(ApplicationError::NotFound);
    }
    Ok(record)
}

pub(super) async fn answer(
    store: &PostgresOrganizationStore,
    actor: &PrincipalScope,
    request: AnswerApproval,
) -> Result<ApprovalRecord, ApplicationError> {
    store.check_tenant(actor.tenant_id())?;
    let actor_owner = actor.owner();
    let mut tx = owner_transaction(&store.pool, &actor_owner).await?;
    let access = access_in(&mut tx, &actor_owner).await?;
    if !viewer_active(&access, actor) {
        return Err(ApplicationError::Forbidden);
    }
    let client = actor.client_id().ok_or(ApplicationError::Forbidden)?;
    let digest = zuno_orchestration::sha256_json(&json!(request));
    let (owner, session) = coordinates(&mut tx, actor.tenant_id(), &request.approval_id).await?;
    set_owner(&mut tx, &owner).await?;
    // Match admission/execution lock order: session, then approval.
    lock_session(&mut tx, &owner, &session).await?;
    let record = record_in(&mut tx, &owner, request.approval_id.as_str(), true).await?;
    // A concurrent identical answer may have committed while this transaction
    // waited for the session. Read its receipt under the acquired lock.
    set_owner(&mut tx, &actor_owner).await?;
    let prior: Option<String> = query_scalar(
        "SELECT request_digest FROM zuno_enterprise_preview.approval_answer_receipt
         WHERE tenant_id=$1 AND principal_id=$2 AND client_id=$3 AND request_id=$4",
    )
    .bind(actor_owner.tenant_id.as_str())
    .bind(actor_owner.principal_id.as_str())
    .bind(client.as_str())
    .bind(request.request_id.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(database_error)?;
    set_owner(&mut tx, &owner).await?;
    if prior.as_ref().is_some_and(|prior| prior != &digest) {
        return Err(ApplicationError::Conflict);
    }
    if !can_approve(
        &access.policy,
        record.audience,
        &owner,
        actor,
        &access.member,
    ) {
        return Err(ApplicationError::Forbidden);
    }
    if prior.is_some() {
        tx.commit().await.map_err(database_error)?;
        return Ok(record);
    }
    if record.state != ApprovalState::Pending {
        return Err(ApplicationError::Conflict);
    }
    let requester_access = match access_in(&mut tx, &owner).await {
        Ok(access) => Some(access),
        Err(ApplicationError::NotFound) => None,
        Err(error) => return Err(error),
    };
    let requester_valid = requester_access.is_some_and(|access| {
        actor_denial(&access.policy, &access.member, &record.requester).is_none()
    });
    let time = database_time(&mut tx).await?;
    let job_phase: String = query_scalar(
        "SELECT phase FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(record.binding.job_id.as_str())
        .fetch_one(&mut *tx).await.map_err(database_error)?;
    if record.expires_at_ms <= time
        || record.policy_revision != access.policy.revision
        || !requester_valid
        || matches!(job_phase.as_str(), "completed" | "failed" | "cancelled")
    {
        let state = if record.expires_at_ms <= time {
            ApprovalState::Expired
        } else {
            ApprovalState::Invalidated
        };
        invalidate_in(
            &mut tx,
            &record,
            state,
            "approval no longer matches current task authority",
        )
        .await?;
        crate::runtime::waiting::approval_changed(&mut tx, &record).await?;
        tx.commit().await.map_err(database_error)?;
        return Err(ApplicationError::Forbidden);
    }
    let state = match request.answer {
        ApprovalAnswer::Approve => ApprovalState::Approved,
        ApprovalAnswer::Reject => ApprovalState::Rejected,
    };
    query(
        "UPDATE zuno_enterprise_preview.operation_approval SET state=$4,decided_by=$5,decided_at=$6
         WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(record.id.as_str())
    .bind(text(&state)?)
    .bind(json!(actor_owner))
    .bind(time)
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    emit(&mut tx,&record.requester,&session,"authorization.approval.answered",
        json!({"approvalID":record.id,"binding":record.binding,"actor":actor,"state":state,"requestID":request.request_id})).await?;
    set_owner(&mut tx, &actor_owner).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.approval_answer_receipt(
           tenant_id,principal_id,client_id,request_id,request_digest,approval_owner,approval_id)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(actor_owner.tenant_id.as_str())
    .bind(actor_owner.principal_id.as_str())
    .bind(client.as_str())
    .bind(request.request_id.as_str())
    .bind(digest)
    .bind(owner.principal_id.as_str())
    .bind(record.id.as_str())
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    set_owner(&mut tx, &owner).await?;
    let result = record_in(&mut tx, &owner, record.id.as_str(), false).await?;
    crate::runtime::waiting::approval_changed(&mut tx, &result).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(result)
}
