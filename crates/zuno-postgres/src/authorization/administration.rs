use super::*;
use zuno_types::identity::RequestId;

#[derive(serde::Serialize)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
enum Change {
    Member(OrganizationMember),
    Policy(OrganizationPolicy),
}

pub(super) async fn member(
    store: &PostgresOrganizationStore,
    actor: &PrincipalScope,
    request: UpdateOrganizationMember,
) -> Result<OrganizationMutationReceipt, ApplicationError> {
    if request.member.owner.tenant_id != *actor.tenant_id() {
        return Err(ApplicationError::Forbidden);
    }
    mutate(
        store,
        actor,
        request.request_id,
        request.expected_revision,
        Change::Member(request.member),
    )
    .await
}
pub(super) async fn policy(
    store: &PostgresOrganizationStore,
    actor: &PrincipalScope,
    request: UpdateOrganizationPolicy,
) -> Result<OrganizationMutationReceipt, ApplicationError> {
    if !request.policy.is_valid()
        || request.policy.tenant_id != *actor.tenant_id()
        || request.expected_revision.get().checked_add(1) != Some(request.policy.revision.get())
    {
        return Err(ApplicationError::Invalid(
            "invalid organization policy replacement".to_owned(),
        ));
    }
    mutate(
        store,
        actor,
        request.request_id,
        request.expected_revision,
        Change::Policy(request.policy),
    )
    .await
}

async fn mutate(
    store: &PostgresOrganizationStore,
    actor: &PrincipalScope,
    request: RequestId,
    expected: NonZeroU64,
    change: Change,
) -> Result<OrganizationMutationReceipt, ApplicationError> {
    store.check_tenant(actor.tenant_id())?;
    let owner = actor.owner();
    let mut tx = owner_transaction(&store.pool, &owner).await?;
    // Organization updates never take session locks. Approval writers take
    // shared policy locks, so revocation and admission have a clear ordering.
    query("SELECT revision FROM zuno_enterprise_preview.organization_policy WHERE tenant_id=$1 FOR UPDATE")
        .bind(actor.tenant_id().as_str()).fetch_one(&mut *tx).await.map_err(database_error)?;
    let access = access_in(&mut tx, &owner).await?;
    // A concurrent identical request may already have advanced the revision.
    // Reading its receipt uses current membership/app authority; a new mutation
    // still requires the caller's original revision below.
    let current_actor = PrincipalScope::new(
        actor.tenant_id().clone(),
        actor.principal_id().clone(),
        actor.kind(),
        actor.client_id().cloned(),
        access.policy.revision,
    );
    if access.member.role != OrganizationRole::Administrator
        || !can_approve(
            &access.policy,
            ApprovalAudience::Requester,
            &owner,
            &current_actor,
            &access.member,
        )
    {
        return Err(ApplicationError::Forbidden);
    }
    let client = actor.client_id().ok_or(ApplicationError::Forbidden)?;
    let operation = match change {
        Change::Member(_) => "organization.member",
        Change::Policy(_) => "organization.policy",
    };
    let digest = zuno_orchestration::sha256_json(&json!([expected, change]));
    let prior=query(
        "SELECT request_digest,resource_id FROM zuno_enterprise_preview.request_receipt
         WHERE tenant_id=$1 AND principal_id=$2 AND client_id=$3 AND operation=$4 AND request_id=$5",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(client.as_str()).bind(operation).bind(request.as_str())
        .fetch_optional(&mut *tx).await.map_err(database_error)?;
    if let Some(prior) = prior {
        if prior
            .try_get::<String, _>("request_digest")
            .map_err(database_error)?
            != digest
        {
            return Err(ApplicationError::Conflict);
        }
        let version = prior
            .try_get::<String, _>("resource_id")
            .map_err(database_error)?
            .parse::<i64>()
            .map_err(ApplicationError::storage)?;
        let receipt = OrganizationMutationReceipt {
            request_id: request,
            revision: nonzero(version)?,
        };
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    if actor.policy_revision() != access.policy.revision {
        return Err(ApplicationError::Forbidden);
    }
    if access.policy.revision != expected {
        return Err(ApplicationError::Conflict);
    }
    let next = expected
        .get()
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(ApplicationError::Conflict)?;
    match &change {
        Change::Member(member) => {
            set_owner(&mut tx, &member.owner).await?;
            query(
                "INSERT INTO zuno_enterprise_preview.organization_member(tenant_id,principal_id,role,active)
                 VALUES($1,$2,$3,$4) ON CONFLICT(tenant_id,principal_id) DO UPDATE SET role=EXCLUDED.role,active=EXCLUDED.active",
            ).bind(member.owner.tenant_id.as_str()).bind(member.owner.principal_id.as_str()).bind(text(&member.role)?).bind(member.active)
                .execute(&mut *tx).await.map_err(database_error)?;
            set_owner(&mut tx, &owner).await?;
            query("UPDATE zuno_enterprise_preview.organization_policy SET revision=$2 WHERE tenant_id=$1")
                .bind(owner.tenant_id.as_str()).bind(next).execute(&mut *tx).await.map_err(database_error)?;
        }
        Change::Policy(policy) => {
            query(
                "UPDATE zuno_enterprise_preview.organization_policy SET revision=$2,allowed_apps=$3,approval_apps=$4,
                   auto_read_apps=$5,approval_lifetime_seconds=$6 WHERE tenant_id=$1",
            ).bind(owner.tenant_id.as_str()).bind(next).bind(json!(policy.allowed_apps)).bind(json!(policy.approval_apps))
                .bind(json!(policy.auto_read_apps)).bind(i32::try_from(policy.approval_lifetime_seconds).map_err(ApplicationError::storage)?)
                .execute(&mut *tx).await.map_err(database_error)?;
        }
    }
    let time = database_time(&mut tx).await?;
    query(
        "INSERT INTO zuno_enterprise_preview.organization_audit(tenant_id,id,actor,type,data,time_created)
         VALUES($1,$2,$3,$4,$5,$6)",
    ).bind(owner.tenant_id.as_str()).bind(format!("org_evt_{}",Uuid::new_v4().simple())).bind(json!(actor)).bind(operation)
        .bind(json!({"requestID":request,"previousRevision":expected,"revision":next,"change":change})).bind(time)
        .execute(&mut *tx).await.map_err(database_error)?;
    query(
        "INSERT INTO zuno_enterprise_preview.request_receipt(tenant_id,principal_id,client_id,operation,request_id,request_digest,resource_id)
         VALUES($1,$2,$3,$4,$5,$6,$7)",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(client.as_str()).bind(operation)
        .bind(request.as_str()).bind(digest).bind(next.to_string()).execute(&mut *tx).await.map_err(database_error)?;
    tx.commit().await.map_err(database_error)?;
    Ok(OrganizationMutationReceipt {
        request_id: request,
        revision: nonzero(next)?,
    })
}
