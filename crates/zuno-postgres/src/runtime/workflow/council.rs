//! Council policy on native Workflow and child Jobs. No independent task system.
use super::*;
use zuno_application::council::{CouncilDefinitionGrant, CouncilInvocation, CouncilStore};
use zuno_orchestration::{WorkflowNodeDescriptor, WorkflowTemplateDescriptor};

#[async_trait]
impl CouncilStore for PostgresRuntimeStore {
    async fn dispatch_council(
        &self,
        lease: &ExecutionLease,
        invocation: CouncilInvocation,
        grant: &CouncilDefinitionGrant,
    ) -> Result<WorkflowDispatch, ApplicationError> {
        if invocation.preset != grant.rules.preset.name
            || invocation.root.prompt.len() > 65536
            || grant.synthesis.workspace != ChildWorkspacePolicy::ModelOnly
        {
            return Err(ApplicationError::Forbidden);
        }
        zuno_engine::council::validate_policy(&grant.rules.preset)
            .map_err(ApplicationError::storage)?;
        let mut nodes = Vec::new();
        let mut grants = std::collections::BTreeMap::new();
        for seat in &grant.rules.preset.seats {
            let child = grant
                .seats
                .get(&seat.id)
                .ok_or(ApplicationError::Forbidden)?;
            let id = zuno_engine::council::seat_node(&seat.id);
            nodes.push(WorkflowNodeDescriptor {
                id: id.clone(),
                agent: child.selection.agent.clone(),
                prompt: Some(zuno_engine::council::seat_prompt(
                    &invocation.root.prompt,
                    &seat.id,
                    &seat.instruction,
                )),
                description: Some(format!("{} / {}", invocation.preset, seat.id)),
                depends_on: Vec::new(),
            });
            grants.insert(id, child.clone());
        }
        nodes.push(WorkflowNodeDescriptor {
            id: zuno_engine::council::SYNTHESIS_NODE.to_owned(),
            agent: grant.synthesis.selection.agent.clone(),
            prompt: Some("Synthesize the recorded Council results.".to_owned()),
            description: Some(format!("{} / synthesis", invocation.preset)),
            depends_on: Vec::new(),
        });
        grants.insert(
            zuno_engine::council::SYNTHESIS_NODE.to_owned(),
            grant.synthesis.clone(),
        );
        let template = format!("council:{}", invocation.preset);
        self.dispatch_workflow(
            lease,
            WorkflowInvocation {
                template: template.clone(),
                root: invocation.root,
            },
            &WorkflowDefinitionGrant {
                group: grant.group.clone(),
                template: WorkflowTemplateDescriptor {
                    name: template,
                    source_id: grant.rules.preset.source_id.clone(),
                    max_parallel: grant.rules.preset.max_parallel,
                    max_agents: nodes.len(),
                    nodes,
                },
                nodes: grants,
                council: Some(grant.rules.clone()),
            },
        )
        .await
    }
}

struct Seat {
    node: NodeRunId,
    id: String,
    child: JobId,
    status: String,
    attempts: i32,
    retry_at: Option<i64>,
    answer: Option<Value>,
    error: Option<String>,
    completion: Option<Value>,
    digest: Option<String>,
    phase: Option<String>,
    completed_at: Option<i64>,
}
struct Clock {
    state: String,
    seat_deadline: i64,
    deadline: i64,
    stop_deadline: Option<i64>,
    synthesis_deadline: Option<i64>,
}

fn policy(run: &Run) -> Result<&CouncilPlan, ApplicationError> {
    run.plan.council.as_ref().ok_or(ApplicationError::Conflict)
}
async fn clock(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    run: &Run,
) -> Result<Clock, ApplicationError> {
    let row = query("SELECT * FROM zuno_enterprise_preview.runtime_council WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 FOR UPDATE")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    Ok(Clock {
        state: row.try_get("state").map_err(database_error)?,
        seat_deadline: row.try_get("seat_deadline_at").map_err(database_error)?,
        deadline: row.try_get("deadline_at").map_err(database_error)?,
        stop_deadline: row.try_get("stop_deadline_at").map_err(database_error)?,
        synthesis_deadline: row
            .try_get("synthesis_deadline_at")
            .map_err(database_error)?,
    })
}
async fn seats(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    run: &Run,
) -> Result<Vec<Seat>, ApplicationError> {
    let rows = query("SELECT s.*,n.position,n.child_job_id,c.completion,c.completion_digest,r.phase,j.time_completed
        FROM zuno_enterprise_preview.runtime_council_seat s
        JOIN zuno_enterprise_preview.runtime_workflow_node n ON n.tenant_id=s.tenant_id AND n.principal_id=s.principal_id AND n.node_run_id=s.node_run_id
        JOIN zuno_enterprise_preview.runtime_child c ON c.tenant_id=n.tenant_id AND c.principal_id=n.principal_id AND c.job_id=n.child_job_id
        LEFT JOIN zuno_enterprise_preview.runtime_job r ON r.tenant_id=c.tenant_id AND r.principal_id=c.principal_id AND r.job_id=c.activated_job_id
        LEFT JOIN zuno_enterprise_preview.agent_job j ON j.tenant_id=r.tenant_id AND j.principal_id=r.principal_id AND j.id=r.job_id
        WHERE s.tenant_id=$1 AND s.principal_id=$2 AND s.run_id=$3 ORDER BY n.position")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    if rows.len() != policy(run)?.preset.seats.len() {
        return Err(ApplicationError::Conflict);
    }
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            if row.try_get::<i32, _>("position").map_err(database_error)? != index as i32
                || row
                    .try_get::<String, _>("seat_id")
                    .map_err(database_error)?
                    != policy(run)?.preset.seats[index].id
            {
                return Err(invalid("Council seat identity or order changed"));
            }
            let answer: Option<Value> = row.try_get("answer").map_err(database_error)?;
            let answer_digest: Option<String> =
                row.try_get("answer_digest").map_err(database_error)?;
            let completion: Option<Value> = row.try_get("completion").map_err(database_error)?;
            let digest: Option<String> =
                row.try_get("completion_digest").map_err(database_error)?;
            if answer.as_ref().map(zuno_orchestration::sha256_json) != answer_digest
                || completion.as_ref().map(zuno_orchestration::sha256_json) != digest
            {
                return Err(invalid("Council evidence digest changed"));
            }
            Ok(Seat {
                node: NodeRunId::new(
                    row.try_get::<String, _>("node_run_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
                id: row.try_get("seat_id").map_err(database_error)?,
                child: JobId::new(
                    row.try_get::<String, _>("child_job_id")
                        .map_err(database_error)?,
                )
                .map_err(ApplicationError::storage)?,
                status: row.try_get("status").map_err(database_error)?,
                attempts: row.try_get("attempts").map_err(database_error)?,
                retry_at: row.try_get("retry_after_at").map_err(database_error)?,
                answer,
                error: row.try_get("error").map_err(database_error)?,
                completion,
                digest,
                phase: row.try_get("phase").map_err(database_error)?,
                completed_at: row.try_get("time_completed").map_err(database_error)?,
            })
        })
        .collect()
}

pub(super) async fn activate(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    let preset = &policy(run)?.preset;
    let now = database_time(tx).await?;
    let deadline = now
        .checked_add(preset.deadline_ms as i64)
        .ok_or(ApplicationError::Conflict)?;
    let seat_deadline = deadline - preset.synthesis_policy.timeout_ms as i64;
    query(
        "INSERT INTO zuno_enterprise_preview.runtime_council
        (tenant_id,principal_id,run_id,state,started_at,seat_deadline_at,deadline_at)
        VALUES($1,$2,$3,'seats',$4,$5,$6)",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(run.id.as_str())
    .bind(now)
    .bind(seat_deadline)
    .bind(deadline)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    for (index, seat) in preset.seats.iter().enumerate() {
        query("INSERT INTO zuno_enterprise_preview.runtime_council_seat(tenant_id,principal_id,run_id,node_run_id,seat_id,status)
            SELECT tenant_id,principal_id,run_id,node_run_id,$5,'pending' FROM zuno_enterprise_preview.runtime_workflow_node
            WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 AND position=$4")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str()).bind(index as i32).bind(&seat.id)
            .execute(&mut **tx).await.map_err(database_error)?;
    }
    advance(tx, coordinator, run).await
}

async fn update_seat(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    seat: &Seat,
    status: &str,
    answer: Option<Value>,
    error: Option<&str>,
    retry_at: Option<i64>,
) -> Result<(), ApplicationError> {
    query("UPDATE zuno_enterprise_preview.runtime_council_seat SET status=$4,answer=$5,answer_digest=$6,error=$7,retry_after_at=$8
        WHERE tenant_id=$1 AND principal_id=$2 AND node_run_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(seat.node.as_str())
        .bind(status).bind(&answer).bind(answer.as_ref().map(zuno_orchestration::sha256_json)).bind(error).bind(retry_at)
        .execute(&mut **tx).await.map_err(database_error)?;
    let state = match status {
        "pending" => "pending",
        "running" | "retrying" => "running",
        "waiting" => "waiting",
        "completed" => "completed",
        "invalid" | "failed" => "failed",
        "timed_out" | "cancelled" => "cancelled",
        "uncertain" => "uncertain",
        _ => return Err(ApplicationError::Conflict),
    };
    query("UPDATE zuno_enterprise_preview.runtime_workflow_node SET state=$4,result=$5,result_digest=$6,time_updated=$7
        WHERE tenant_id=$1 AND principal_id=$2 AND node_run_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(seat.node.as_str())
        .bind(state).bind(&seat.completion).bind(&seat.digest).bind(database_time(tx).await?)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

async fn observe(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    run: &Run,
    clock: &Clock,
) -> Result<Vec<Seat>, ApplicationError> {
    let preset = &policy(run)?.preset;
    let now = database_time(tx).await?;
    for seat in seats(tx, owner, run).await? {
        if !matches!(seat.status.as_str(), "running" | "waiting" | "retrying") {
            continue;
        }
        match seat.phase.as_deref() {
            Some("completed") => {
                let raw = seat.completion.as_ref().ok_or(ApplicationError::Conflict)?;
                let text = raw
                    .pointer("/payload/text")
                    .and_then(Value::as_str)
                    .ok_or(ApplicationError::Conflict)?;
                if seat
                    .completed_at
                    .is_none_or(|time| time > clock.seat_deadline)
                {
                    update_seat(
                        tx,
                        owner,
                        &seat,
                        "timed_out",
                        None,
                        Some("seat completed after its deadline"),
                        None,
                    )
                    .await?;
                } else {
                    match zuno_engine::council::parse_generic_council_answer(
                        text,
                        preset.seat_output_bytes,
                    ) {
                        Ok(answer) => {
                            update_seat(
                                tx,
                                owner,
                                &seat,
                                "completed",
                                Some(json!(answer)),
                                None,
                                None,
                            )
                            .await?
                        }
                        Err(error) => {
                            let retry = seat.attempts <= preset.retry_policy.max_retries as i32
                                && now < clock.seat_deadline;
                            let retry_at = retry.then(|| {
                                now.saturating_add(100_i64 << seat.attempts.min(3))
                                    .min(clock.seat_deadline)
                            });
                            update_seat(
                                tx,
                                owner,
                                &seat,
                                "invalid",
                                None,
                                Some(&error.to_string()),
                                retry_at,
                            )
                            .await?;
                        }
                    }
                }
                query("UPDATE zuno_enterprise_preview.runtime_council_attempt SET observed_digest=$4
                    WHERE tenant_id=$1 AND principal_id=$2 AND child_job_id=$3 AND observed_digest IS NULL")
                    .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(seat.child.as_str()).bind(&seat.digest)
                    .execute(&mut **tx).await.map_err(database_error)?;
            }
            Some("failed") => {
                update_seat(
                    tx,
                    owner,
                    &seat,
                    "failed",
                    None,
                    Some("seat Job failed"),
                    None,
                )
                .await?
            }
            Some("cancelled") => {
                let status = if now >= clock.seat_deadline || clock.state == "stopping" {
                    "timed_out"
                } else {
                    "cancelled"
                };
                update_seat(
                    tx,
                    owner,
                    &seat,
                    status,
                    None,
                    Some("seat execution stopped"),
                    None,
                )
                .await?;
            }
            Some("uncertain") => {
                update_seat(
                    tx,
                    owner,
                    &seat,
                    "uncertain",
                    None,
                    Some("seat outcome requires inspection"),
                    None,
                )
                .await?
            }
            Some("waiting" | "paused") => {
                update_seat(tx, owner, &seat, "waiting", None, None, None).await?
            }
            _ => {}
        }
    }
    seats(tx, owner, run).await
}

async fn admit(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    seat: &Seat,
    deadline: i64,
    repair: bool,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    let attempt = seat.attempts + 1;
    let (child, prompt, sources) = if repair {
        let definition = policy(run)?
            .repairs
            .get(&seat.id)
            .ok_or(ApplicationError::Forbidden)?;
        let prior = seat.completion.as_ref().ok_or(ApplicationError::Conflict)?;
        let text = prior
            .pointer("/payload/text")
            .and_then(Value::as_str)
            .ok_or(ApplicationError::Conflict)?;
        let prompt = zuno_engine::council::repair_prompt(&run.plan.invocation.root.prompt, text);
        let key = zuno_orchestration::sha256_json(&json!([run.id, seat.node, attempt]));
        let grant = ChildDefinitionGrant {
            parent: coordinator.configuration.clone(),
            child: definition.configuration.clone(),
            selection: definition.selection.clone(),
            maximum_depth: definition.maximum_depth,
            maximum_children: definition.maximum_children,
            workspace: ChildWorkspacePolicy::ModelOnly,
        };
        let child = children::stage_in(tx, coordinator, ChildInvocation {
            invocation_id: InvocationId::new(format!("repair_{key}")).map_err(ApplicationError::storage)?,
            arguments_sha256: zuno_orchestration::sha256_json(&json!([seat.node, attempt, seat.digest])),
            logical_key: format!("council-repair:{key}"), prompt: prompt.clone(),
            description: format!("{} / {} / format correction", policy(run)?.preset.name, seat.id),
            delivery: ChildDelivery::Quiet, resume_session_id: None,
            presentation: json!({"kind":"council_repair","runId":run.id,"seatId":seat.id,"sourceJob":seat.child}),
        }, &grant, false).await?;
        query("UPDATE zuno_enterprise_preview.runtime_workflow_node SET child_job_id=$4,state='pending',result=NULL,result_digest=NULL,
            input_prompt=NULL,input_digest=NULL,input_sources=NULL WHERE tenant_id=$1 AND principal_id=$2 AND node_run_id=$3")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(seat.node.as_str()).bind(child.job_id.as_str())
            .execute(&mut **tx).await.map_err(database_error)?;
        (
            child.job_id,
            prompt,
            json!([{"jobId":seat.child,"completionDigest":seat.digest}]),
        )
    } else {
        let record = children::read(tx, &owner, &seat.child).await?;
        (seat.child.clone(), record.invocation.prompt, json!([]))
    };
    super::inputs::freeze_input(tx, &owner, &run.id, &child, &prompt, sources).await?;
    let record = children::read(tx, &owner, &child).await?;
    children::activate(tx, coordinator, &record).await?;
    query(
        "UPDATE zuno_enterprise_preview.runtime_workflow_node SET state='running',time_updated=$4
        WHERE tenant_id=$1 AND principal_id=$2 AND node_run_id=$3",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(seat.node.as_str())
    .bind(database_time(tx).await?)
    .execute(&mut **tx)
    .await
    .map_err(database_error)?;
    query("UPDATE zuno_enterprise_preview.runtime_job SET deadline_at=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(child.as_str()).bind(deadline)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("INSERT INTO zuno_enterprise_preview.runtime_council_attempt(tenant_id,principal_id,node_run_id,attempt,child_job_id,kind)
        VALUES($1,$2,$3,$4,$5,$6)")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(seat.node.as_str()).bind(attempt)
        .bind(child.as_str()).bind(if repair { "repair" } else { "seat" }).execute(&mut **tx).await.map_err(database_error)?;
    query("UPDATE zuno_enterprise_preview.runtime_council_seat SET status=$4,attempts=$5,retry_after_at=NULL
        WHERE tenant_id=$1 AND principal_id=$2 AND node_run_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(seat.node.as_str())
        .bind(if repair { "retrying" } else { "running" }).bind(attempt).execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

enum Effects {
    Clear,
    Pending,
    Uncertain,
}
async fn stopped_effects(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    run: &Run,
) -> Result<Effects, ApplicationError> {
    let rows=query("SELECT o.completion,o.completion_digest,o.producer FROM (
        SELECT tenant_id,principal_id,job_id,completion,completion_digest,'command' AS producer FROM zuno_enterprise_preview.gateway_operation
        UNION ALL SELECT tenant_id,principal_id,job_id,completion,completion_digest,'workspace_merge' AS producer
          FROM zuno_enterprise_preview.gateway_merge_operation WHERE admitted) o
        JOIN zuno_enterprise_preview.runtime_stop s ON s.tenant_id=o.tenant_id AND s.principal_id=o.principal_id AND s.job_id=o.job_id
        JOIN zuno_enterprise_preview.runtime_council_attempt a ON a.tenant_id=s.tenant_id AND a.principal_id=s.principal_id AND a.child_job_id=s.root_job_id
        JOIN zuno_enterprise_preview.runtime_council_seat c ON c.tenant_id=a.tenant_id AND c.principal_id=a.principal_id AND c.node_run_id=a.node_run_id
        WHERE o.tenant_id=$1 AND o.principal_id=$2 AND c.run_id=$3 LIMIT 1025")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    if rows.len() > 1024 {
        return Ok(Effects::Uncertain);
    }
    let mut pending = false;
    for row in rows {
        let raw: Option<Value> = row.try_get("completion").map_err(database_error)?;
        let digest: Option<String> = row.try_get("completion_digest").map_err(database_error)?;
        if raw.as_ref().map(zuno_orchestration::sha256_json) != digest {
            return Err(invalid("Council operation evidence digest changed"));
        }
        let Some(raw) = raw else {
            pending = true;
            continue;
        };
        if row
            .try_get::<String, _>("producer")
            .map_err(database_error)?
            == "workspace_merge"
        {
            let completion: zuno_application::workspace_merge::WorkspaceMergeCompletion =
                serde_json::from_value(raw).map_err(ApplicationError::storage)?;
            completion.validate()?;
            continue;
        }
        let completion: zuno_application::environment::OperationCompletion =
            serde_json::from_value(raw).map_err(ApplicationError::storage)?;
        completion.validate()?;
        if !matches!(
            completion.receipt.phase,
            zuno_application::environment::OperationPhase::Completed
                | zuno_application::environment::OperationPhase::Cancelled
        ) {
            return Ok(Effects::Uncertain);
        }
    }
    Ok(if pending {
        Effects::Pending
    } else {
        Effects::Clear
    })
}

async fn expire_seats(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    seats: &[Seat],
    now: i64,
    deadline: i64,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    for seat in seats {
        if matches!(
            seat.status.as_str(),
            "completed" | "failed" | "timed_out" | "cancelled" | "uncertain"
        ) || (seat.status == "invalid" && seat.retry_at.is_none())
        {
            continue;
        }
        let record = children::read(tx, &owner, &seat.child).await?;
        if record.state == "active" {
            let job = read_job(tx, &owner, seat.child.as_str()).await?;
            super::super::control::cancel_tree_in(tx, &job, "Council seat deadline expired", now)
                .await?;
        } else if record.state == "staged" {
            query(
                "UPDATE zuno_enterprise_preview.runtime_child SET state='cancelled',time_updated=$4
                WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3 AND state='staged'",
            )
            .bind(owner.tenant_id.as_str())
            .bind(owner.principal_id.as_str())
            .bind(seat.child.as_str())
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(database_error)?;
        }
        update_seat(
            tx,
            &owner,
            seat,
            "timed_out",
            None,
            Some("Council seat deadline expired"),
            None,
        )
        .await?;
    }
    query("UPDATE zuno_enterprise_preview.runtime_council SET state='stopping',stop_deadline_at=COALESCE(stop_deadline_at,$4)
        WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .bind(now.saturating_add(zuno_engine::council::CANCELLATION_GRACE_MS).min(deadline)).execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

async fn finish(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    state: &str,
    error: Option<&str>,
) -> Result<(), ApplicationError> {
    if state != "completed" {
        super::coordinator::stop_nodes(tx, coordinator, run, error.unwrap_or(state)).await?;
    }
    query("UPDATE zuno_enterprise_preview.runtime_council SET state=$4 WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3")
        .bind(coordinator.principal.tenant_id().as_str()).bind(coordinator.principal.principal_id().as_str())
        .bind(run.id.as_str()).bind(state).execute(&mut **tx).await.map_err(database_error)?;
    super::coordinator::finish(tx, coordinator, run, state, error).await
}

async fn synthesis_job(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    run: &Run,
) -> Result<JobId, ApplicationError> {
    let id: String = query_scalar(
        "SELECT child_job_id FROM zuno_enterprise_preview.runtime_workflow_node
        WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3 AND node_id=$4",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(run.id.as_str())
    .bind(zuno_engine::council::SYNTHESIS_NODE)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    JobId::new(id).map_err(ApplicationError::storage)
}

async fn synthesis_output(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    id: &JobId,
) -> Result<String, ApplicationError> {
    let record = children::read(tx, owner, id).await?;
    if record.state != "completed" {
        return Err(ApplicationError::Conflict);
    }
    let row = query(
        "SELECT completion,completion_digest FROM zuno_enterprise_preview.runtime_child
        WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    let raw: Value = row.try_get("completion").map_err(database_error)?;
    let digest: String = row.try_get("completion_digest").map_err(database_error)?;
    if zuno_orchestration::sha256_json(&raw) != digest {
        return Err(invalid("Council synthesis digest changed"));
    }
    Ok(raw
        .pointer("/payload/text")
        .and_then(Value::as_str)
        .ok_or(ApplicationError::Conflict)?
        .to_owned())
}

pub(super) async fn advance(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
) -> Result<(), ApplicationError> {
    let owner = coordinator.principal.owner();
    let mut clock = clock(tx, &owner, run).await?;
    let now = database_time(tx).await?;
    if clock.state == "synthesis" {
        let synthesis_deadline = clock.synthesis_deadline.ok_or(ApplicationError::Conflict)?;
        let id = synthesis_job(tx, &owner, run).await?;
        let job = read_job(tx, &owner, id.as_str()).await?;
        return match job.phase {
            JobPhase::Completed => {
                if synthesis_output(tx, &owner, &id).await?.trim().is_empty() {
                    return finish(
                        tx,
                        coordinator,
                        run,
                        "failed",
                        Some("Council synthesis returned no public result"),
                    )
                    .await;
                }
                let completed: i64 = query_scalar("SELECT time_completed FROM zuno_enterprise_preview.agent_job WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
                    .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str())
                    .fetch_one(&mut **tx).await.map_err(database_error)?;
                if completed <= synthesis_deadline {
                    finish(tx, coordinator, run, "completed", None).await
                } else {
                    finish(
                        tx,
                        coordinator,
                        run,
                        "failed",
                        Some("Council synthesis deadline expired"),
                    )
                    .await
                }
            }
            JobPhase::Uncertain => {
                finish(
                    tx,
                    coordinator,
                    run,
                    "uncertain",
                    Some("Council synthesis outcome requires inspection"),
                )
                .await
            }
            JobPhase::Cancelled | JobPhase::Failed => {
                finish(
                    tx,
                    coordinator,
                    run,
                    "failed",
                    Some("Council synthesis failed"),
                )
                .await
            }
            _ if now >= synthesis_deadline => {
                finish(
                    tx,
                    coordinator,
                    run,
                    "failed",
                    Some("Council synthesis deadline expired"),
                )
                .await
            }
            _ => Ok(()),
        };
    }
    if !matches!(clock.state.as_str(), "seats" | "stopping") {
        return Ok(());
    }
    let mut current = observe(tx, &owner, run, &clock).await?;
    if current.iter().any(|seat| seat.status == "uncertain") {
        return finish(
            tx,
            coordinator,
            run,
            "uncertain",
            Some("a Council seat has an uncertain outcome"),
        )
        .await;
    }
    if current.iter().any(|seat| seat.status == "cancelled") {
        return finish(
            tx,
            coordinator,
            run,
            "cancelled",
            Some("a Council seat was cancelled"),
        )
        .await;
    }
    if now >= clock.seat_deadline && clock.state == "seats" {
        expire_seats(tx, coordinator, run, &current, now, clock.deadline).await?;
        clock = self::clock(tx, &owner, run).await?;
        current = seats(tx, &owner, run).await?;
    }
    if clock.state == "stopping" {
        match stopped_effects(tx, &owner, run).await? {
            Effects::Uncertain => {
                return finish(
                    tx,
                    coordinator,
                    run,
                    "uncertain",
                    Some("Council cancellation has an uncertain external operation receipt"),
                )
                .await;
            }
            Effects::Pending => {
                if clock.stop_deadline.is_some_and(|deadline| now >= deadline) {
                    return finish(
                        tx,
                        coordinator,
                        run,
                        "uncertain",
                        Some("Council cancellation did not confirm external operation outcomes"),
                    )
                    .await;
                }
                return Ok(());
            }
            Effects::Clear => {}
        }
    }
    let preset = &policy(run)?.preset;
    if clock.state == "seats" {
        let occupied = current
            .iter()
            .filter(|seat| {
                matches!(seat.status.as_str(), "running" | "waiting" | "retrying")
                    || (seat.status == "invalid" && seat.retry_at.is_some())
            })
            .count();
        let mut capacity = preset.max_parallel.saturating_sub(occupied);
        for seat in &current {
            if seat.status == "invalid"
                && seat
                    .retry_at
                    .is_some_and(|time| time <= now && time < clock.seat_deadline)
            {
                admit(tx, coordinator, run, seat, clock.seat_deadline, true).await?;
            } else if seat.status == "pending" && capacity > 0 {
                admit(tx, coordinator, run, seat, clock.seat_deadline, false).await?;
                capacity -= 1;
            }
        }
        current = seats(tx, &owner, run).await?;
        if current.iter().any(|seat| {
            matches!(
                seat.status.as_str(),
                "pending" | "running" | "waiting" | "retrying"
            ) || (seat.status == "invalid" && seat.retry_at.is_some())
        }) {
            return Ok(());
        }
    }
    let valid = current
        .iter()
        .filter(|seat| seat.status == "completed")
        .count();
    if valid < preset.quorum {
        return finish(
            tx,
            coordinator,
            run,
            "failed",
            Some("Council quorum was not reached"),
        )
        .await;
    }
    if now >= clock.deadline {
        return finish(
            tx,
            coordinator,
            run,
            "failed",
            Some("Council deadline expired before synthesis"),
        )
        .await;
    }
    let seat_values = current.iter().map(|seat| json!({
        "id":seat.id,"status":seat.status,"attempts":seat.attempts,"answer":seat.answer,"error":seat.error,
    })).collect::<Vec<_>>();
    let payload = json!({"question":run.plan.invocation.root.prompt,"quorum":preset.quorum,"seats":seat_values});
    let encoded = payload.to_string();
    if encoded.len() > preset.synthesis_policy.max_input_bytes {
        return finish(
            tx,
            coordinator,
            run,
            "failed",
            Some("Council synthesis input exceeds its configured bound"),
        )
        .await;
    }
    let id = synthesis_job(tx, &owner, run).await?;
    let prompt = format!(
        "Synthesize the recorded Council result data. Explain agreement, dissent, risks and recommendation. Invalid or missing seats are not votes; do not invent evidence or perform external actions.\n\n{encoded}"
    );
    let sources = json!(
        current
            .iter()
            .map(
                |seat| json!({"nodeId":seat.node,"jobId":seat.child,"completionDigest":seat.digest})
            )
            .collect::<Vec<_>>()
    );
    super::inputs::freeze_input(tx, &owner, &run.id, &id, &prompt, sources).await?;
    let record = children::read(tx, &owner, &id).await?;
    children::activate(tx, coordinator, &record).await?;
    let deadline = now
        .saturating_add(preset.synthesis_policy.timeout_ms as i64)
        .min(clock.deadline);
    query("UPDATE zuno_enterprise_preview.runtime_job SET deadline_at=$4 WHERE tenant_id=$1 AND principal_id=$2 AND job_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(id.as_str()).bind(deadline)
        .execute(&mut **tx).await.map_err(database_error)?;
    query("UPDATE zuno_enterprise_preview.runtime_council SET state='synthesis',synthesis_deadline_at=$4 WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str()).bind(deadline)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(())
}

pub(super) async fn summary(
    tx: &mut Transaction<'_, Postgres>,
    coordinator: &RuntimeJob,
    run: &Run,
    phase: &str,
) -> Result<String, ApplicationError> {
    let owner = coordinator.principal.owner();
    let preset = &policy(run)?.preset;
    let exists: bool = query_scalar("SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.runtime_council WHERE tenant_id=$1 AND principal_id=$2 AND run_id=$3)")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(run.id.as_str())
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let seats = if exists {
        self::seats(tx, &owner, run).await?
    } else {
        Vec::new()
    };
    let synthesis = if phase == "completed" {
        if seats
            .iter()
            .filter(|seat| seat.status == "completed")
            .count()
            < preset.quorum
        {
            return Err(invalid("Council completion lacks a validated quorum"));
        }
        let id = synthesis_job(tx, &owner, run).await?;
        Some(synthesis_output(tx, &owner, &id).await?)
    } else {
        None
    };
    let mut result = json!({"kind":"council","preset":preset.name,"runId":run.id,"status":phase,"quorum":preset.quorum,
        "seats":seats.iter().map(|seat| json!({"id":seat.id,"jobId":seat.child,"status":seat.status,"attempts":seat.attempts,
            "answer":seat.answer,"error":seat.error})).collect::<Vec<_>>(),"synthesis":synthesis});
    if result.to_string().len() > 16000 {
        for seat in result["seats"]
            .as_array_mut()
            .ok_or(ApplicationError::Conflict)?
        {
            seat.as_object_mut()
                .ok_or(ApplicationError::Conflict)?
                .remove("answer");
        }
        result["synthesis"] = Value::Null;
        result["outputsOmitted"] = json!(true);
    }
    Ok(result.to_string())
}

/// Deadline handling precedes generic lease recovery so a deliberate timeout
/// cannot be mistaken for an unexplained Worker loss.
pub(in crate::runtime) async fn expire(
    tx: &mut Transaction<'_, Postgres>,
    owner: &PrincipalKey,
    now: i64,
) -> Result<(), ApplicationError> {
    let rows = query("SELECT w.job_id FROM zuno_enterprise_preview.runtime_council c
        JOIN zuno_enterprise_preview.runtime_workflow w ON w.tenant_id=c.tenant_id AND w.principal_id=c.principal_id AND w.run_id=c.run_id
        JOIN zuno_enterprise_preview.session s ON s.tenant_id=w.tenant_id AND s.principal_id=w.principal_id AND s.id=w.parent_session_id
        WHERE c.tenant_id=$1 AND c.principal_id=$2 AND w.state='active'
          AND ((c.state='seats' AND c.seat_deadline_at<=$3) OR c.state='stopping' OR (c.state='synthesis' AND c.synthesis_deadline_at<=$3))
        ORDER BY c.deadline_at,c.run_id LIMIT 8 FOR UPDATE OF s SKIP LOCKED")
        .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(now)
        .fetch_all(&mut **tx).await.map_err(database_error)?;
    for row in rows {
        let id = JobId::new(row.try_get::<String, _>("job_id").map_err(database_error)?)
            .map_err(ApplicationError::storage)?;
        let run = read(tx, owner, &id).await?;
        let job = read_job(tx, owner, id.as_str()).await?;
        super::super::control::lock_session(tx, &job).await?;
        Box::pin(super::coordinator::advance(tx, &job, &run)).await?;
    }
    Ok(())
}
