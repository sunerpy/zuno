use super::*;

pub(super) enum GatewayAdmission<'a> {
    Command(&'a zuno_application::environment::OperationAdmission),
    WorkspaceMerge(&'a zuno_application::workspace_merge::WorkspaceMergeAdmission),
}

fn approval_id(owner: &PrincipalKey, binding: &ApprovalBinding) -> String {
    format!(
        "apr_{}",
        zuno_orchestration::sha256_json(&json!([owner, binding.operation_id]))
    )
}
fn bound(job: &zuno_application::runtime::RuntimeJob, binding: &ApprovalBinding) -> bool {
    job.id == binding.job_id
        && job.session_id == binding.session_id
        && job.turn_id == binding.turn_id
}
fn compatible(record: &ApprovalRecord, decision: EnterpriseDecision) -> bool {
    match decision {
        EnterpriseDecision::Denied { .. } => false,
        EnterpriseDecision::Automatic => true,
        EnterpriseDecision::Human { audience } => {
            record.state != ApprovalState::Automatic
                && (audience == ApprovalAudience::Requester
                    || record.audience == ApprovalAudience::DesignatedApprover)
        }
    }
}

pub(super) async fn admit(
    store: &PostgresOrganizationStore,
    lease: &ExecutionLease,
    proposal: ApprovalProposal,
) -> Result<ApprovalRecord, ApplicationError> {
    proposal.validate()?;
    store.check_tenant(&lease.owner.tenant_id)?;
    let mut tx = owner_transaction(&store.pool, &lease.owner).await?;
    let job = verify_lease(&mut tx, lease).await?;
    if !bound(&job, &proposal.binding) {
        return Err(ApplicationError::Conflict);
    }
    let access = access_in(&mut tx, &lease.owner).await?;
    let decision = evaluate_enterprise(
        &access.policy,
        &access.member,
        &job.principal,
        proposal.facts,
    );
    let id = approval_id(&lease.owner, &proposal.binding);
    let exists: bool = query_scalar(
        "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.operation_approval WHERE tenant_id=$1 AND principal_id=$2 AND id=$3)",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(&id)
        .fetch_one(&mut *tx).await.map_err(database_error)?;
    if exists {
        let mut record = record_in(&mut tx, &lease.owner, &id, true).await?;
        if record.binding != proposal.binding {
            return Err(ApplicationError::Conflict);
        }
        if matches!(
            record.state,
            ApprovalState::Pending | ApprovalState::Automatic | ApprovalState::Approved
        ) {
            let time = database_time(&mut tx).await?;
            if time >= record.expires_at_ms {
                invalidate_in(&mut tx, &record, ApprovalState::Expired, "approval expired").await?;
                record.state = ApprovalState::Expired;
            } else if record.policy_revision != access.policy.revision
                || !compatible(&record, decision)
            {
                invalidate_in(
                    &mut tx,
                    &record,
                    ApprovalState::Invalidated,
                    "authorization requirements changed",
                )
                .await?;
                record.state = ApprovalState::Invalidated;
            }
        }
        tx.commit().await.map_err(database_error)?;
        return Ok(record);
    }
    let (state, audience) = match decision {
        EnterpriseDecision::Automatic => (ApprovalState::Automatic, ApprovalAudience::Requester),
        EnterpriseDecision::Human { audience } => (ApprovalState::Pending, audience),
        EnterpriseDecision::Denied { reason } => {
            emit(
                &mut tx,
                &job.principal,
                job.session_id.as_str(),
                "authorization.operation.denied",
                json!({"binding":proposal.binding,"reason":reason}),
            )
            .await?;
            tx.commit().await.map_err(database_error)?;
            return Err(ApplicationError::Forbidden);
        }
    };
    let time = database_time(&mut tx).await?;
    let expires = time
        .checked_add(i64::from(access.policy.approval_lifetime_seconds) * 1000)
        .ok_or(ApplicationError::Conflict)?;
    query(
        "INSERT INTO zuno_enterprise_preview.operation_approval(
           tenant_id,principal_id,id,job_id,session_id,operation_id,binding,requester,policy_revision,
           audience,state,presentation,created_at,expires_at,decided_at)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(&id)
        .bind(job.id.as_str()).bind(job.session_id.as_str()).bind(proposal.binding.operation_id.as_str())
        .bind(json!(proposal.binding)).bind(json!(job.principal))
        .bind(i64::try_from(access.policy.revision.get()).map_err(ApplicationError::storage)?)
        .bind(text(&audience)?).bind(text(&state)?).bind(proposal.presentation).bind(time).bind(expires)
        .bind((state==ApprovalState::Automatic).then_some(time))
        .execute(&mut *tx).await.map_err(database_error)?;
    emit(&mut tx,&job.principal,job.session_id.as_str(),"authorization.approval.created",json!({
        "approvalID":id,"binding":proposal.binding,"audience":audience,"state":state,"expiresAt":expires,
    })).await?;
    let record = record_in(&mut tx, &lease.owner, &id, false).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(record)
}

pub(super) async fn check_execution(
    store: &PostgresOrganizationStore,
    lease: &ExecutionLease,
    proposal: ApprovalProposal,
) -> Result<CheckedApproval, ApplicationError> {
    check_execution_with_admission(store, lease, proposal, None).await
}

pub(super) async fn check_execution_with_admission(
    store: &PostgresOrganizationStore,
    lease: &ExecutionLease,
    proposal: ApprovalProposal,
    admission: Option<GatewayAdmission<'_>>,
) -> Result<CheckedApproval, ApplicationError> {
    proposal.validate()?;
    store.check_tenant(&lease.owner.tenant_id)?;
    let mut tx = owner_transaction(&store.pool, &lease.owner).await?;
    let job = verify_lease(&mut tx, lease).await?;
    if !bound(&job, &proposal.binding) {
        return Err(ApplicationError::Conflict);
    }
    let access = access_in(&mut tx, &lease.owner).await?;
    let id = approval_id(&lease.owner, &proposal.binding);
    let record = record_in(&mut tx, &lease.owner, &id, true).await?;
    if record.binding != proposal.binding || record.requester != job.principal {
        return Err(ApplicationError::Conflict);
    }
    if !matches!(
        record.state,
        ApprovalState::Automatic | ApprovalState::Approved
    ) {
        return Err(ApplicationError::Forbidden);
    }
    let time = database_time(&mut tx).await?;
    if time >= record.expires_at_ms {
        invalidate_in(&mut tx, &record, ApprovalState::Expired, "approval expired").await?;
        tx.commit().await.map_err(database_error)?;
        return Err(ApplicationError::Forbidden);
    }
    let decision = evaluate_enterprise(
        &access.policy,
        &access.member,
        &job.principal,
        proposal.facts,
    );
    let mut valid =
        record.policy_revision == access.policy.revision && compatible(&record, decision);
    if record.state == ApprovalState::Approved {
        let decider = record
            .decided_by
            .as_ref()
            .ok_or(ApplicationError::Forbidden)?;
        set_owner(&mut tx, decider).await?;
        let current = match access_in(&mut tx, decider).await {
            Ok(access) => Some(access),
            Err(ApplicationError::NotFound) => None,
            Err(error) => return Err(error),
        };
        valid &= current.is_some_and(|access| {
            access.member.active
                && access.policy.revision == record.policy_revision
                && match record.audience {
                    ApprovalAudience::Requester => *decider == record.requester.owner(),
                    ApprovalAudience::DesignatedApprover => {
                        *decider != record.requester.owner()
                            && matches!(
                                access.member.role,
                                OrganizationRole::Approver | OrganizationRole::Administrator
                            )
                    }
                }
        });
        set_owner(&mut tx, &lease.owner).await?;
    }
    if !valid {
        invalidate_in(
            &mut tx,
            &record,
            ApprovalState::Invalidated,
            "current policy or approver no longer authorizes operation",
        )
        .await?;
        tx.commit().await.map_err(database_error)?;
        return Err(ApplicationError::Forbidden);
    }
    // The expiry copied into an incoming lease is data. Use the fenced database
    // deadline, including renewals, rather than trusting that caller field.
    let expires: i64 = query_scalar(
        "SELECT lease_expires FROM zuno_enterprise_preview.runtime_session WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
    ).bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(lease.session_id.as_str())
        .fetch_one(&mut *tx).await.map_err(database_error)?;
    let valid_until_ms = expires.min(record.expires_at_ms);
    if valid_until_ms <= database_time(&mut tx).await? {
        return Err(ApplicationError::LeaseLost);
    }
    let checked = CheckedApproval {
        approval_id: record.id,
        binding: record.binding,
        lease: ExecutionLease {
            expires_at_ms: expires,
            ..lease.clone()
        },
        valid_until_ms,
    };
    match admission {
        Some(GatewayAdmission::Command(admission)) => {
            crate::operation::admit_in(&mut tx, &checked, admission).await?
        }
        Some(GatewayAdmission::WorkspaceMerge(admission)) => {
            crate::workspace_merge::admit_in(&mut tx, &checked, admission).await?
        }
        None => {}
    }
    if valid_until_ms <= database_time(&mut tx).await? {
        return Err(ApplicationError::LeaseLost);
    }
    tx.commit().await.map_err(database_error)?;
    Ok(checked)
}
