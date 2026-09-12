//! Short dependency, cancellation and completion transactions.
use super::*;

async fn node_phases(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
) -> Result<Vec<NodePhase>, ApplicationError> {
    let owner = coordinator.principal.owner();
    let rows = query("SELECT n.node_id,n.position,n.child_job_id,c.state AS child_state,c.completion,c.completion_digest,r.phase
        FROM zuno_enterprise_preview.runtime_workflow_node n
        JOIN zuno_enterprise_preview.runtime_child c ON c.tenant_id=n.tenant_id AND c.principal_id=n.principal_id AND c.job_id=n.child_job_id
        LEFT JOIN zuno_enterprise_preview.runtime_job r ON r.tenant_id=c.tenant_id AND r.principal_id=c.principal_id AND r.job_id=c.activated_job_id
        WHERE n.tenant_id=$1 AND n.principal_id=$2 AND n.run_id=$3 ORDER BY n.position")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    if rows.len() != run.plan.nodes.len() {
        return Err(invalid("workflow node set is incomplete"));
    }
    let mut phases = Vec::new();
    let mut updates = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        if row
            .try_get::<String, _>("node_id")
            .map_err(database_error)?
            != run.plan.nodes[index].id
            || row.try_get::<i32, _>("position").map_err(database_error)? != index as i32
        {
            return Err(invalid("workflow node identity changed"));
        }
        let child: String = row.try_get("child_state").map_err(database_error)?;
        let phase: Option<String> = row.try_get("phase").map_err(database_error)?;
        let state = match (child.as_str(), phase.as_deref()) {
            ("staged", None) => NodePhase::Pending,
            ("cancelled", None) => NodePhase::Cancelled,
            (_, Some("ready" | "running")) => NodePhase::Running,
            (_, Some("waiting" | "paused")) => NodePhase::Waiting,
            (_, Some("completed")) => NodePhase::Completed,
            (_, Some("failed")) => NodePhase::Failed,
            (_, Some("cancelled")) => NodePhase::Cancelled,
            (_, Some("uncertain")) => NodePhase::Uncertain,
            _ => return Err(invalid("workflow node state disagrees with its native Job")),
        };
        let result: Option<Value> = row.try_get("completion").map_err(database_error)?;
        let digest: Option<String> = row.try_get("completion_digest").map_err(database_error)?;
        if result.as_ref().map(zuno_orchestration::sha256_json) != digest {
            return Err(invalid("workflow node completion digest changed"));
        }
        if matches!(
            state,
            NodePhase::Completed | NodePhase::Failed | NodePhase::Uncertain
        ) && result.is_none()
        {
            return Err(invalid(
                "workflow terminal node lacks a committed completion",
            ));
        }
        updates.push(json!({"position":index,"state":state,"result":result,"digest":digest}));
        phases.push(state);
    }
    let now = database_time(tx).await?;
    query("UPDATE zuno_enterprise_preview.runtime_workflow_node n
        SET state=v.state,result=NULLIF(v.result,'null'::jsonb),result_digest=v.digest,time_updated=$5
        FROM jsonb_to_recordset($4) AS v(position integer,state text,result jsonb,digest text)
        WHERE n.tenant_id=$1 AND n.principal_id=$2 AND n.run_id=$3 AND n.position=v.position")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .bind(json!(updates)).bind(now).execute(&mut **tx).await.map_err(database_error)?;
    Ok(phases)
}

pub(super) async fn advance(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    let access = crate::authorization::access_in(tx, &owner).await?;
    if zuno_permission::enterprise::actor_denial(
        &access.policy,
        &access.member,
        &coordinator.principal,
    )
    .is_some()
    {
        stop_nodes(tx, coordinator, run, "workflow authority was revoked").await?;
        return finish(
            tx,
            coordinator,
            run,
            "failed",
            Some("workflow authority was revoked"),
        )
        .await;
    }
    let phases = node_phases(tx, coordinator, run).await?;
    match run
        .plan
        .graph()?
        .decide(&phases)
        .map_err(ApplicationError::storage)?
    {
        Decision::Dispatch(indices) => {
            for index in indices {
                let child: String = query_scalar(
                    "SELECT child_job_id FROM zuno_enterprise_preview.runtime_workflow_node
                    WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 AND position=$4",
                )
                .bind(owner.tenant_id.as_str())
                .bind(owner.principal_id.as_str())
                .bind(run.id.as_str())
                .bind(index as i32)
                .fetch_one(&mut **tx)
                .await
                .map_err(database_error)?;
                let id = JobId::new(child).map_err(ApplicationError::storage)?;
                let record = children::read(tx, &owner, &id).await?;
                if record.state != "staged"
                    || record.ticket.workspace == ChildWorkspaceState::Pending
                {
                    return Err(ApplicationError::Conflict);
                }
                resolve_input(tx, coordinator, run, index, &id, &record.invocation.prompt).await?;
                children::activate(tx, coordinator, &record).await?;
            }
            node_phases(tx, coordinator, run).await?;
            Ok(())
        }
        Decision::Waiting => Ok(()),
        Decision::Completed => finish(tx, coordinator, run, "completed", None).await,
        Decision::Stopped { index, phase } => {
            let reason = format!(
                "workflow node {} ended as {phase:?}",
                run.plan.nodes[index].id
            );
            stop_nodes(tx, coordinator, run, &reason).await?;
            finish(
                tx,
                coordinator,
                run,
                if phase == NodePhase::Uncertain {
                    "uncertain"
                } else {
                    "failed"
                },
                Some(&reason),
            )
            .await
        }
    }
}

async fn stop_nodes(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    reason: &str,
) -> Result<(), ApplicationError> {
    let view = dispatch_view(tx, &coordinator.principal.owner(), run).await?;
    let time = database_time(tx).await?;
    for node in view.nodes {
        let record = children::read(tx, &coordinator.principal.owner(), &node.child.job_id).await?;
        if matches!(record.state.as_str(), "active" | "uncertain") {
            let child = read_job(
                tx,
                &coordinator.principal.owner(),
                node.child.job_id.as_str(),
            )
            .await?;
            super::control::cancel_tree_in(tx, &child, reason, time).await?;
        }
    }
    children::retire_staged(tx, coordinator, time).await?;
    node_phases(tx, coordinator, run).await?;
    Ok(())
}

async fn finish(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    phase: &str,
    error: Option<&str>,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    let now = database_time(tx).await?;
    set_state(tx, coordinator, run, phase).await?;
    let summary = result(tx, coordinator, phase)
        .await?
        .ok_or(ApplicationError::Conflict)?;
    let result: Value = serde_json::from_str(&summary).map_err(ApplicationError::storage)?;
    let sequence = emit(
        tx,
        &coordinator.principal,
        coordinator.session_id.as_str(),
        "agent.job.settled",
        json!({"jobID":coordinator.id,"status":phase,"error":error,"runId":run.id}),
    )
    .await?;
    query("UPDATE zuno_enterprise_preview.agent_job SET status=$4,error=$5,settled_seq=$6,time_completed=$7,time_updated=$7,result=$8
        WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(coordinator.id.as_str())
        .bind(phase).bind(error).bind(sequence).bind(now).bind(&result).execute(&mut **tx).await.map_err(database_error)?;
    children::completed(tx, coordinator, phase, None, error, sequence, now).await?;
    query(
        "UPDATE zuno_enterprise_preview.runtime_job SET phase=$4,time_updated=$5
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(coordinator.id.as_str())
    .bind(phase)
    .bind(now)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    query("UPDATE zuno_enterprise_preview.runtime_session SET current_job_id=CASE WHEN $4='uncertain' THEN current_job_id ELSE NULL END
        WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND current_job_id=$5")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(coordinator.session_id.as_str())
        .bind(phase).bind(coordinator.id.as_str()).execute(&mut **tx).await.map_err(database_error)?;
    emit(
        tx,
        &coordinator.principal,
        coordinator.session_id.as_str(),
        "runtime.job.finished",
        json!({"jobID":coordinator.id,"phase":phase,"runId":run.id}),
    )
    .await?;
    Ok(())
}

/// Reconstruct the group result from its ordered, digest-checked child facts.
pub(in crate::runtime) async fn result(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    phase: &str,
) -> Result<Option<String>, ApplicationError> {
    if identity(tx, &coordinator.principal.owner(), &coordinator.id)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let run = read(tx, &coordinator.principal.owner(), &coordinator.id).await?;
    let phases = node_phases(tx, coordinator, &run).await?;
    if phase == "completed" && phases.iter().any(|state| *state != NodePhase::Completed) {
        return Err(invalid(
            "workflow success requires every native node to complete",
        ));
    }
    let rows = query("SELECT node_id,child_job_id,state,result FROM zuno_enterprise_preview.runtime_workflow_node
        WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 ORDER BY position")
        .bind(coordinator.principal.tenant_id().as_str()).bind(coordinator.principal.principal_id().as_str()).bind(run.id.as_str())
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    let mut nodes = Vec::new();
    for row in rows {
        let result: Option<Value> = row.try_get("result").map_err(database_error)?;
        nodes.push(json!({
            "id":row.try_get::<String,_>("node_id").map_err(database_error)?,
            "jobId":row.try_get::<String,_>("child_job_id").map_err(database_error)?,
            "state":row.try_get::<String,_>("state").map_err(database_error)?,
            "output":result.as_ref().and_then(|value| value.pointer("/payload/text")).and_then(Value::as_str),
        }));
    }
    let mut result =
        json!({"workflow":run.plan.template.name,"runId":run.id,"status":phase,"nodes":nodes});
    if result.to_string().len() > 32000 {
        for node in result["nodes"]
            .as_array_mut()
            .ok_or(ApplicationError::Conflict)?
        {
            node.as_object_mut()
                .ok_or(ApplicationError::Conflict)?
                .remove("output");
        }
        result["outputsOmitted"] = json!(true);
    }
    Ok(Some(result.to_string()))
}

/// Owner-scoped bounded scan; row locks last for one coordination transaction.
pub(in crate::runtime) async fn drain(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
) -> Result<(), ApplicationError> {
    let rows = query("SELECT w.job_id,w.parent_job_id FROM zuno_enterprise_preview.runtime_workflow w
        JOIN zuno_enterprise_preview.session s ON s.tenant_id=w.tenant_id AND s.principal_id=w.principal_id AND s.id=w.parent_session_id
        WHERE w.tenant_id=$1 AND w.principal_id=$2 AND w.state IN('preparing','prepared','active')
        ORDER BY w.last_coordinated_at,w.run_id LIMIT 8 FOR UPDATE OF s SKIP LOCKED")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).fetch_all(&mut **tx).await.map_err(database_error)?;
    for row in rows {
        let id = JobId::new(row.try_get::<String, _>("job_id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
        let run = read(tx, owner, &id).await?;
        let now = database_time(tx).await?;
        query("UPDATE zuno_enterprise_preview.runtime_workflow SET last_coordinated_at=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(now)
            .execute(&mut **tx).await.map_err(database_error)?;
        let child = children::read(tx, owner, &id).await?;
        if child.state == "staged" || child.state == "cancelled" {
            if child.state == "cancelled" {
                let parent = read_job(tx, owner, run.parent.as_str()).await?;
                set_state(tx, &parent, &run, "cancelled").await?;
            }
            continue;
        }
        let coordinator = read_job(tx, owner, id.as_str()).await?;
        super::control::lock_session(tx, &coordinator).await?;
        if matches!(
            coordinator.phase,
            JobPhase::Cancelled | JobPhase::Failed | JobPhase::Uncertain
        ) {
            set_state(
                tx,
                &coordinator,
                &run,
                match coordinator.phase {
                    JobPhase::Uncertain => "uncertain",
                    JobPhase::Failed => "failed",
                    _ => "cancelled",
                },
            )
            .await?;
        } else if run.state == "active" {
            Box::pin(advance(tx, &coordinator, &run)).await?;
        } else {
            let parent = read_job(tx, owner, run.parent.as_str()).await?;
            if parent.phase != JobPhase::Running {
                let now = database_time(tx).await?;
                Box::pin(super::control::cancel_tree_in(
                    tx,
                    &coordinator,
                    "parent did not retain the prepared workflow",
                    now,
                ))
                .await?;
                set_state(tx, &coordinator, &run, "cancelled").await?;
            }
        }
    }
    Ok(())
}
