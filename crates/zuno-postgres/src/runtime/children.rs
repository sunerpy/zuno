//! Native child Jobs share admission, waiting and completion ownership with roots.

mod delivery;
pub(crate) use delivery::validate_completion_input;
pub(crate) use delivery::{completed, drain, ready};

use super::*;
use zuno_application::child::{
    ChildDefinitionGrant, ChildDelivery, ChildDispatch, ChildDispatchStore, ChildInvocation,
};
use zuno_types::identity::WaitId;
use zuno_types::wait::{WaitContinuation, WaitRef, WaitTarget};

struct ChildRecord {
    invocation: ChildInvocation,
    ticket: ChildDispatch,
    parent_job_id: JobId,
    parent_session_id: SessionId,
    configuration: ConfigurationRef,
    selection: zuno_application::runtime::JobInputSelection,
    state: String,
}

fn invalid(message: &str) -> ApplicationError {
    ApplicationError::Invalid(message.to_owned())
}

async fn read(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &JobId,
) -> Result<ChildRecord, ApplicationError> {
    let row = query("SELECT * FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let invocation: ChildInvocation =
        serde_json::from_value(row.try_get("invocation").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    invocation.validate()?;
    let wait: WaitRef = serde_json::from_value(row.try_get("reference").map_err(database_error)?)
        .map_err(ApplicationError::storage)?;
    wait.validate().map_err(invalid)?;
    let session_id = SessionId::new(
        row.try_get::<String, _>("child_session_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    let parent_job_id = JobId::new(
        row.try_get::<String, _>("parent_job_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    let parent_session_id = SessionId::new(
        row.try_get::<String, _>("parent_session_id")
            .map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)?;
    if wait.target != (WaitTarget::Child { job_id: id.clone() })
        || wait.invocation_id != invocation.invocation_id
        || wait.arguments_sha256 != invocation.arguments_sha256
    {
        return Err(invalid("stored child invocation identity disagrees"));
    }
    let configuration: ConfigurationRef =
        serde_json::from_value(row.try_get("definition").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    let selection: zuno_application::runtime::JobInputSelection =
        serde_json::from_value(row.try_get("selection").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
    configuration.validate()?;
    selection.validate()?;
    Ok(ChildRecord {
        ticket: ChildDispatch {
            job_id: id.clone(),
            session_id,
            wait,
            delivery: invocation.delivery,
        },
        invocation,
        parent_job_id,
        parent_session_id,
        configuration,
        selection,
        state: row.try_get("state").map_err(database_error)?,
    })
}

#[async_trait]
impl ChildDispatchStore for PostgresRuntimeStore {
    async fn delegation_depth(&self, lease: &ExecutionLease) -> Result<u32, ApplicationError> {
        self.check_owner(&lease.owner)?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let job = verify_lease(&mut tx, lease).await?;
        let access = crate::authorization::access_in(&mut tx, &lease.owner).await?;
        if zuno_permission::enterprise::actor_denial(&access.policy, &access.member, &job.principal)
            .is_some()
        {
            return Err(ApplicationError::Forbidden);
        }
        let depth = depth_in(&mut tx, &lease.owner, &job.session_id).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(depth)
    }

    async fn dispatch_child(
        &self,
        lease: &ExecutionLease,
        invocation: ChildInvocation,
        grant: &ChildDefinitionGrant,
    ) -> Result<ChildDispatch, ApplicationError> {
        self.check_owner(&lease.owner)?;
        invocation.validate()?;
        grant.validate()?;
        let mut tx = owner_transaction(&self.pool, &lease.owner).await?;
        let parent = verify_lease(&mut tx, lease).await?;
        if parent.configuration != grant.parent {
            return Err(ApplicationError::Forbidden);
        }
        let access = crate::authorization::access_in(&mut tx, &lease.owner).await?;
        if zuno_permission::enterprise::actor_denial(
            &access.policy,
            &access.member,
            &parent.principal,
        )
        .is_some()
        {
            return Err(ApplicationError::Forbidden);
        }
        let digest = zuno_orchestration::sha256_json(&json!([
            &invocation,
            grant.parent,
            grant.child,
            grant.selection
        ]));
        let existing = query(
            "SELECT job_id,request_digest FROM zuno_enterprise_preview.runtime_child
            WHERE tenant_id=$1 AND principal_id=$2 AND parent_job_id=$3 AND invocation_id=$4",
        )
        .bind(lease.owner.tenant_id.as_str())
        .bind(lease.owner.principal_id.as_str())
        .bind(parent.id.as_str())
        .bind(invocation.invocation_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?;
        if let Some(existing) = existing {
            if existing
                .try_get::<String, _>("request_digest")
                .map_err(database_error)?
                != digest
            {
                return Err(ApplicationError::Conflict);
            }
            let id = JobId::new(
                existing
                    .try_get::<String, _>("job_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?;
            let ticket = read(&mut tx, &lease.owner, &id).await?.ticket;
            tx.commit().await.map_err(database_error)?;
            return Ok(ticket);
        }
        let depth = depth_in(&mut tx, &lease.owner, &parent.session_id).await?;
        if depth >= grant.maximum_depth {
            return Err(ApplicationError::Forbidden);
        }
        let count:i64 = query_scalar("SELECT count(*) FROM zuno_enterprise_preview.runtime_child WHERE tenant_id=$1 AND principal_id=$2 AND parent_job_id=$3")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(parent.id.as_str())
            .fetch_one(&mut *tx).await.map_err(database_error)?;
        if count >= i64::from(grant.maximum_children) {
            return Err(ApplicationError::Forbidden);
        }
        if let Some(session) = &invocation.resume_session_id {
            resume_allowed(&mut tx, &parent, session).await?;
        }
        let key = zuno_orchestration::sha256_json(&json!([
            "child-job",
            lease.owner,
            parent.id,
            invocation.invocation_id
        ]));
        let id = JobId::new(format!("job_{key}")).map_err(ApplicationError::storage)?;
        let session_id = match &invocation.resume_session_id {
            Some(id) => id.clone(),
            None => SessionId::new(format!("ses_{key}")).map_err(ApplicationError::storage)?,
        };
        let wait = WaitRef {
            id: WaitId::new(format!("wait_{key}")).map_err(ApplicationError::storage)?,
            turn_id: parent.turn_id.clone(),
            invocation_id: invocation.invocation_id.clone(),
            arguments_sha256: invocation.arguments_sha256.clone(),
            target: WaitTarget::Child { job_id: id.clone() },
            continuation: WaitContinuation::CurrentTurn,
        };
        let delivery = match invocation.delivery {
            ChildDelivery::Foreground => "foreground",
            ChildDelivery::NextStep => "next_step",
            ChildDelivery::Quiet => "quiet",
        };
        let now = database_time(&mut tx).await?;
        query("INSERT INTO zuno_enterprise_preview.runtime_child(tenant_id,principal_id,job_id,parent_job_id,parent_session_id,child_session_id,
            invocation_id,logical_key,request_digest,invocation,definition,selection,reference,delivery,state,time_created,time_updated)
            VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'staged',$15,$15)")
            .bind(lease.owner.tenant_id.as_str()).bind(lease.owner.principal_id.as_str()).bind(id.as_str())
            .bind(parent.id.as_str()).bind(parent.session_id.as_str()).bind(session_id.as_str())
            .bind(invocation.invocation_id.as_str()).bind(&invocation.logical_key).bind(&digest)
            .bind(json!(invocation)).bind(json!(grant.child)).bind(json!(grant.selection)).bind(json!(wait)).bind(delivery).bind(now)
            .execute(&mut *tx).await.map_err(database_error)?;
        let record = read(&mut tx, &lease.owner, &id).await?;
        if invocation.delivery != ChildDelivery::Foreground {
            activate(&mut tx, &parent, &record).await?;
        }
        verify_lease(&mut tx, lease).await?;
        emit(&mut tx, &parent.principal, parent.session_id.as_str(), "runtime.child.dispatched",
            json!({"jobID":id,"invocationID":invocation.invocation_id,"sessionID":session_id,"delivery":delivery,"configuration":grant.child})).await?;
        tx.commit().await.map_err(database_error)?;
        Ok(record.ticket)
    }
}

async fn depth_in(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    session: &SessionId,
) -> Result<u32, ApplicationError> {
    let mut current = Some(session.to_string());
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = current {
        if seen.len() > 16 || !seen.insert(id.clone()) {
            return Err(invalid("invalid child session ancestry"));
        }
        current=query_scalar("SELECT parent_id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id).fetch_one(&mut **tx).await.map_err(database_error)?;
    }
    u32::try_from(seen.len().saturating_sub(1)).map_err(ApplicationError::storage)
}

async fn resume_allowed(
    tx: &mut Transaction<'_, Postgres>,
    parent: &RuntimeJob,
    session: &SessionId,
) -> Result<(), ApplicationError> {
    let allowed:bool=query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.session s
        JOIN zuno_enterprise_preview.runtime_session r ON r.tenant_id=s.tenant_id AND r.principal_id=s.principal_id AND r.session_id=s.id
        WHERE s.tenant_id=$1 AND s.principal_id=$2 AND s.id=$3 AND s.parent_id=$4 AND r.current_job_id IS NULL
        AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_job j WHERE j.tenant_id=s.tenant_id AND j.principal_id=s.principal_id
          AND j.session_id=s.id AND j.phase NOT IN('completed','failed','cancelled')))")
        .bind(parent.principal.tenant_id().as_str()).bind(parent.principal.principal_id().as_str()).bind(session.as_str()).bind(parent.session_id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    if !allowed {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}

pub(crate) async fn validate_delegation_input(
    tx: &mut Transaction<'_, Postgres>,
    job: &RuntimeJob,
    prompt: &Value,
) -> Result<(), ApplicationError> {
    let record = read(tx, &job.principal.owner(), &job.id).await?;
    if record.ticket.session_id != job.session_id
        || record.state != "active"
        || prompt.get("parentSessionID") != Some(&json!(record.parent_session_id))
        || prompt.get("parentJobID") != Some(&json!(record.parent_job_id))
        || prompt.pointer("/prompt/text").and_then(Value::as_str)
            != Some(record.invocation.prompt.as_str())
    {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}

/// Called only inside the parent's checkpoint transaction.
pub(crate) async fn activate_wait(
    tx: &mut Transaction<'_, Postgres>,
    parent: &RuntimeJob,
    reference: &WaitRef,
) -> Result<(), ApplicationError> {
    let WaitTarget::Child { job_id } = &reference.target else {
        return Ok(());
    };
    let record = read(tx, &parent.principal.owner(), job_id).await?;
    if record.parent_job_id != parent.id
        || record.parent_session_id != parent.session_id
        || record.ticket.wait != *reference
    {
        return Err(ApplicationError::Forbidden);
    }
    if record.state == "staged" {
        activate(tx, parent, &record).await?;
    }
    Ok(())
}

async fn activate(
    tx: &mut Transaction<'_, Postgres>,
    parent: &RuntimeJob,
    record: &ChildRecord,
) -> Result<(), ApplicationError> {
    let owner = parent.principal.owner();
    let id = &record.ticket.job_id;
    let child = &record.ticket.session_id;
    let now = database_time(tx).await?;
    let model = json!({"providerID":record.selection.model.provider_id,"modelID":record.selection.model.model_id});
    if record.invocation.resume_session_id.is_some() {
        resume_allowed(tx, parent, child).await?;
    } else {
        query("INSERT INTO zuno_enterprise_preview.session(tenant_id,principal_id,id,workspace_id,title,parent_id,agent,model,time_created,time_updated)
            SELECT tenant_id,principal_id,$4,workspace_id,$5,id,$6,$7,$8,$8 FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(parent.session_id.as_str()).bind(child.as_str())
            .bind(&record.invocation.description).bind(&record.selection.agent).bind(&model).bind(now).execute(&mut **tx).await.map_err(database_error)?;
        query("INSERT INTO zuno_enterprise_preview.session_memory_policy(tenant_id,principal_id,session_id,revision,use_memories,generate_private)
            SELECT tenant_id,principal_id,$4,1,use_memories,generate_private FROM zuno_enterprise_preview.session_memory_policy
            WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(parent.session_id.as_str()).bind(child.as_str())
            .execute(&mut **tx).await.map_err(database_error)?;
    }
    let suffix = id
        .as_str()
        .strip_prefix("job_")
        .ok_or_else(|| invalid("invalid child Job identity"))?;
    let turn = format!("turn_{suffix}");
    let input = format!("msg_{suffix}");
    let prompt = json!({"kind":"delegation","prompt":{"text":record.invocation.prompt,"files":[],"agents":[]},"agent":record.selection.agent,"model":model,
        "parentSessionID":parent.session_id,"parentJobID":parent.id});
    let input_sequence=emit(tx,&parent.principal,child.as_str(),"session.input.admitted",
        json!({"inputID":input,"prompt":prompt,"delivery":"queue","state":"queued","triggerKind":"automatic","timeCreated":now})).await?;
    query("INSERT INTO zuno_enterprise_preview.input(tenant_id,principal_id,session_id,id,request_key,prompt,state,admitted_sequence,time_created)
        VALUES($1,$2,$3,$4,$5,$6,'queued',$7,$8)")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(child.as_str()).bind(&input).bind(format!("child:{suffix}"))
        .bind(prompt).bind(input_sequence).bind(now).execute(&mut **tx).await.map_err(database_error)?;
    let subject = zuno_db::job::JobSubject::child_session(child.to_string());
    let delivery = if record.invocation.delivery == ChildDelivery::NextStep {
        "next-step"
    } else {
        "quiet"
    };
    let created = emit(
        tx,
        &parent.principal,
        parent.session_id.as_str(),
        "agent.job.created",
        json!({"jobID":id,"subject":subject.as_json(),"status":"queued","reportDelivery":delivery}),
    )
    .await?;
    query("INSERT INTO zuno_enterprise_preview.agent_job(tenant_id,principal_id,id,parent_session_id,subject_kind,subject_payload,status,report_delivery,created_seq,time_created,time_updated)
        VALUES($1,$2,$3,$4,'child-session',$5,'queued',$6,$7,$8,$8)")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(parent.session_id.as_str()).bind(subject.as_json())
        .bind(delivery).bind(created).bind(now).execute(&mut **tx).await.map_err(database_error)?;
    let version = input_version(tx, &owner, child.as_str()).await?;
    query("INSERT INTO zuno_enterprise_preview.runtime_job(tenant_id,principal_id,job_id,session_id,turn_id,input_id,request_digest,principal,configuration,phase,input_version,ready_at,time_created,time_updated)
        VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'ready',$10,$11,$11,$11)")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(child.as_str()).bind(turn).bind(input)
        .bind(zuno_orchestration::sha256_json(&json!(record.invocation))).bind(json!(parent.principal)).bind(json!(record.configuration))
        .bind(version).bind(now).execute(&mut **tx).await.map_err(database_error)?;
    let changed=query("UPDATE zuno_enterprise_preview.runtime_child SET state='active',activated_job_id=job_id,time_updated=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='staged'")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(now)
        .execute(&mut **tx).await.map_err(database_error)?.rows_affected();
    if changed != 1 {
        return Err(ApplicationError::Conflict);
    }
    Ok(())
}

/// A staged foreground intent has no executing child. The parent checkpoint
/// activates every retained wait before this cleanup; leftovers belong to
/// completed/failed/interrupted calls and must not reserve the logical key.
pub(crate) async fn retire_staged(
    tx: &mut Transaction<'_, Postgres>,
    parent: &RuntimeJob,
    now: i64,
) -> Result<(), ApplicationError> {
    let retired:Vec<String>=query_scalar("UPDATE zuno_enterprise_preview.runtime_child SET state='cancelled',time_updated=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND parent_job_id=$3 AND state='staged' RETURNING job_id")
        .bind(parent.principal.tenant_id().as_str()).bind(parent.principal.principal_id().as_str()).bind(parent.id.as_str()).bind(now)
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    if !retired.is_empty() {
        emit(tx,&parent.principal,parent.session_id.as_str(),"runtime.child.intent_retired",
            json!({"parentJobID":parent.id,"childJobIDs":retired,"reason":"parent boundary no longer retains these waits"})).await?;
    }
    Ok(())
}
