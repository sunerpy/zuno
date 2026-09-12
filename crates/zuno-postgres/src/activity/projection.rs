use super::*;
use sqlx_core::sql_str::AssertSqlSafe;
use zuno_application::activity as project;
use zuno_types::identity::{ApprovalId, JobId, PrincipalId, TenantId, TurnId};

pub(crate) async fn message(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    record: &zuno_db::message::MessageRecord,
) -> Result<(), ApplicationError> {
    let mut data = record.data.clone();
    let at = source_origin(connection, owner, &record.id, &mut data)
        .await?
        .unwrap_or(record.time_created);
    let usage = zuno_db::session::MessageUsage::from_data(&data)
        .normalized()
        .map(|tokens| {
            Ok::<_, ApplicationError>(NormalizedUsage {
                input: counter(tokens.input)?,
                output: counter(tokens.output)?,
                reasoning: counter(tokens.reasoning)?,
                cache_read: counter(tokens.cache_read)?,
                cache_write: counter(tokens.cache_write)?,
            })
        })
        .transpose()?;
    let item = project::message(&record.id, at, &data, usage)?;
    publish(connection, owner, &record.session_id, item).await
}

pub(crate) async fn part(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    record: &zuno_db::message::PartRecord,
) -> Result<(), ApplicationError> {
    let parent = query(
        "SELECT data,execution_job_id FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&record.session_id).bind(&record.message_id)
        .fetch_one(&mut *connection).await.map_err(database_error)?;
    let parent_data: Value = parent.try_get("data").map_err(database_error)?;
    let execution_job: Option<String> =
        parent.try_get("execution_job_id").map_err(database_error)?;
    let role = project::role(parent_data.as_object().ok_or(ApplicationError::Conflict)?)?;
    let mut data = record.data.clone();
    if data.get("type").and_then(Value::as_str) == Some("reasoning")
        && data
            .get("metadata")
            .and_then(|metadata| metadata.get("providerReasoning"))
            .is_some()
    {
        let duplicate: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2
             AND session_id=$3 AND message_id=$4 AND kind='reasoning' AND id<>$5
             AND data->>'text'=$6 AND NOT COALESCE(data->'metadata' ? 'providerReasoning',false))",
        ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&record.session_id)
            .bind(&record.message_id).bind(&record.id).bind(data.get("text").and_then(Value::as_str).unwrap_or_default())
            .fetch_one(&mut *connection).await.map_err(database_error)?;
        if duplicate {
            return Ok(());
        }
    }
    source_origin(connection, owner, &record.message_id, &mut data).await?;
    let Some(mut item) = project::part(
        &record.id,
        &record.message_id,
        record.time_created,
        role,
        &data,
    )?
    else {
        return Ok(());
    };
    if let SessionItem::Invocation { invocation } = &mut item.item {
        if let Some(job) = execution_job {
            enrich_invocation(connection, owner, &record.session_id, &job, invocation).await?;
        } else if invocation.state == InvocationState::Running {
            invocation.state = InvocationState::Uncertain;
        }
    }
    publish(connection, owner, &record.session_id, item).await
}

async fn source_origin(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    message: &str,
    data: &mut serde_json::Map<String, Value>,
) -> Result<Option<i64>, ApplicationError> {
    let origin = query(
        "SELECT prompt,state,time_created FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(message)
        .fetch_optional(connection).await.map_err(database_error)?;
    let Some(origin) = origin else {
        return Ok(None);
    };
    let prompt: Value = origin.try_get("prompt").map_err(database_error)?;
    data.insert(
        "activityOrigin".to_owned(),
        prompt.get("kind").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "activityState".to_owned(),
        json!(
            origin
                .try_get::<String, _>("state")
                .map_err(database_error)?
        ),
    );
    if let Some(text) = prompt.pointer("/prompt/text").and_then(Value::as_str) {
        data.insert("activityText".to_owned(), json!(text));
    }
    Ok(Some(
        origin.try_get("time_created").map_err(database_error)?,
    ))
}

async fn enrich_invocation(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    job: &str,
    invocation: &mut Invocation,
) -> Result<(), ApplicationError> {
    let phase: String = query_scalar(
        "SELECT phase FROM zuno_enterprise_preview.runtime_job WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND job_id=$4",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(job)
        .fetch_one(&mut *connection).await.map_err(database_error)?;
    let wait = query(
        "SELECT w.reference,j.phase FROM zuno_enterprise_preview.runtime_wait w
         JOIN zuno_enterprise_preview.runtime_job j ON j.tenant_id=w.tenant_id AND j.principal_id=w.principal_id AND j.job_id=w.job_id
         WHERE w.tenant_id=$1 AND w.principal_id=$2 AND w.session_id=$3 AND w.reference->>'invocationId'=$4
           AND w.job_id=$5 AND w.state IN('pending','ready') ORDER BY w.time_updated DESC,w.id DESC LIMIT 1",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(invocation.id.as_str())
        .bind(job).fetch_optional(&mut *connection).await.map_err(database_error)?;
    if let Some(wait) = wait {
        let reference: zuno_types::wait::WaitRef =
            serde_json::from_value(wait.try_get("reference").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        let target = match reference.target {
            zuno_types::wait::WaitTarget::Approval { approval_id } => {
                WaitingFor::Approval { approval_id }
            }
            zuno_types::wait::WaitTarget::Child { job_id } => WaitingFor::Child { job_id },
            zuno_types::wait::WaitTarget::Operation { operation_id } => {
                WaitingFor::Operation { operation_id }
            }
            zuno_types::wait::WaitTarget::Timer { deadline_ms } => WaitingFor::Timer {
                deadline: counter(deadline_ms)?,
            },
            zuno_types::wait::WaitTarget::UserInput { request_id } => WaitingFor::UserInput {
                question_id: request_id.to_string(),
            },
        };
        if matches!(
            invocation.state,
            InvocationState::Queued | InvocationState::Running
        ) {
            invocation.state = match wait.try_get::<&str, _>("phase").map_err(database_error)? {
                "cancelled" => {
                    if matches!(target, WaitingFor::Operation { .. }) {
                        InvocationState::Uncertain
                    } else {
                        InvocationState::Cancelled
                    }
                }
                "uncertain" => InvocationState::Uncertain,
                _ => InvocationState::Waiting,
            };
            invocation.waiting_for = Some(target);
        }
    }
    if matches!(
        phase.as_str(),
        "cancelled" | "failed" | "uncertain" | "completed"
    ) {
        invocation.state = match invocation.state {
            InvocationState::Queued => InvocationState::Cancelled,
            InvocationState::Running => InvocationState::Uncertain,
            other => other,
        };
    }
    let operation = query(
        "SELECT admission,completion FROM zuno_enterprise_preview.gateway_operation
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND invocation_id=$4 AND job_id=$5 ORDER BY time_admitted DESC LIMIT 1",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(invocation.id.as_str())
        .bind(job).fetch_optional(&mut *connection).await.map_err(database_error)?;
    if let Some(operation) = operation {
        let admission: zuno_application::environment::OperationAdmission =
            serde_json::from_value(operation.try_get("admission").map_err(database_error)?)
                .map_err(ApplicationError::storage)?;
        invocation.location = ExecutionLocation::Enterprise {
            environment_id: admission.environment.spec.id,
        };
        // Gateway admission requires observed confinement; it is not inferred
        // from a requested profile or from a model-authored result string.
        invocation.isolation = Isolation::Enforced;
        let completion: Option<Value> = operation.try_get("completion").map_err(database_error)?;
        if let Some(completion) = completion {
            let completion: zuno_application::environment::OperationCompletion =
                serde_json::from_value(completion).map_err(ApplicationError::storage)?;
            if completion.receipt.phase == zuno_application::environment::OperationPhase::Cancelled
            {
                invocation.state = InvocationState::Cancelled;
                invocation.waiting_for = None;
            }
        }
    }
    Ok(())
}

async fn refresh_parts(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
) -> Result<(), ApplicationError> {
    // Only unsettled public calls need overlays when wait/Job state changes.
    let rows = query(
        "SELECT p.* FROM zuno_enterprise_preview.part p
         WHERE p.tenant_id=$1 AND p.principal_id=$2 AND p.session_id=$3 AND p.kind='tool'
           AND p.data->'state'->>'status' IN('pending','running') ORDER BY p.time_created,p.id",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(session)
    .fetch_all(&mut *connection)
    .await
    .map_err(database_error)?;
    for row in rows {
        let record = decode_part(row)?;
        part(connection, owner, &record).await?;
    }
    Ok(())
}

pub(crate) async fn event(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    kind: &str,
    data: &Value,
) -> Result<(), ApplicationError> {
    if kind == "session.input.admitted"
        && let (Some(id), Some(prompt), Some(at)) = (
            data.get("inputID").and_then(Value::as_str),
            data.get("prompt"),
            data.get("timeCreated").and_then(Value::as_i64),
        )
    {
        publish_input(connection, owner, session, id, prompt, "queued", at).await?;
    } else if matches!(
        kind,
        "session.input.consumed" | "session.input.cancelled" | "session.input.failed"
    ) && let Some(id) = data.get("inputID").and_then(Value::as_str)
    {
        let row = query("SELECT prompt,state,time_created FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(id)
            .fetch_one(&mut *connection).await.map_err(database_error)?;
        publish_input(
            connection,
            owner,
            session,
            id,
            &row.try_get("prompt").map_err(database_error)?,
            row.try_get("state").map_err(database_error)?,
            row.try_get("time_created").map_err(database_error)?,
        )
        .await?;
    }
    if matches!(
        kind,
        "authorization.approval.created"
            | "authorization.approval.answered"
            | "authorization.approval.invalidated"
    ) && let Some(id) = data.get("approvalID").and_then(Value::as_str)
    {
        approval(connection, owner, session, id).await?;
    }
    if matches!(
        kind,
        "runtime.job.admitted"
            | "runtime.attempt.started"
            | "runtime.checkpoint.committed"
            | "runtime.job.finished"
            | "runtime.wait.ready"
    ) && let Some(id) = data.get("jobID").and_then(Value::as_str)
    {
        job(connection, owner, session, id).await?;
    }
    if matches!(
        kind,
        "runtime.checkpoint.committed"
            | "runtime.job.finished"
            | "runtime.wait.ready"
            | "runtime.operation.completed"
    ) {
        refresh_parts(connection, owner, session).await?;
    }
    Ok(())
}

pub(crate) async fn execution_changed(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    id: &str,
) -> Result<(), ApplicationError> {
    job(connection, owner, session, id).await?;
    refresh_parts(connection, owner, session).await
}

async fn publish_input(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    id: &str,
    prompt: &Value,
    state: &str,
    at: i64,
) -> Result<(), ApplicationError> {
    let Some(text) = prompt.pointer("/prompt/text").and_then(Value::as_str) else {
        return Ok(());
    };
    let data = json!({"role":"user","activityText":text,"activityOrigin":prompt.get("kind"),"activityState":state});
    let record = project::message(id, at, data.as_object().expect("object"), None)?;
    publish(connection, owner, session, record).await
}

async fn approval(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    id: &str,
) -> Result<(), ApplicationError> {
    let row = query(
        "SELECT job_id,state,presentation,created_at FROM zuno_enterprise_preview.operation_approval
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
    ).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(session).bind(id)
        .fetch_one(&mut *connection).await.map_err(database_error)?;
    let status = match row.try_get::<&str, _>("state").map_err(database_error)? {
        "pending" => ApprovalStatus::Pending,
        "approved" | "automatic" => ApprovalStatus::Approved,
        "rejected" => ApprovalStatus::Rejected,
        "expired" => ApprovalStatus::Expired,
        "revoked" | "invalidated" => ApprovalStatus::Revoked,
        _ => return Err(ApplicationError::Conflict),
    };
    let record = ItemRecord {
        id: format!("approval:{id}"),
        parent_id: None,
        created_at: counter(row.try_get("created_at").map_err(database_error)?)?,
        item: SessionItem::Approval {
            approval_id: ApprovalId::new(id).map_err(ApplicationError::storage)?,
            job_id: JobId::new(row.try_get::<String, _>("job_id").map_err(database_error)?)
                .map_err(ApplicationError::storage)?,
            status,
            presentation: vec![ContentBlock::Structured {
                value: project::bounded_json(
                    row.try_get("presentation").map_err(database_error)?,
                    64 * 1024,
                )?,
            }],
        },
        // Audience/organization policy are checked by the current approval API,
        // not cached as a transferable right in this immutable frame.
        actions: Vec::new(),
    };
    publish(connection, owner, session, record).await
}

async fn job(
    connection: &mut PgConnection,
    owner: &PrincipalKey,
    session: &str,
    id: &str,
) -> Result<(), ApplicationError> {
    let row = query(
        "SELECT turn_id,phase,time_created FROM zuno_enterprise_preview.runtime_job
         WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND job_id=$4",
    )
    .bind(owner.tenant_id.as_str())
    .bind(owner.principal_id.as_str())
    .bind(session)
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(database_error)?;
    let Some(row) = row else {
        return Ok(());
    };
    let state = match row.try_get::<&str, _>("phase").map_err(database_error)? {
        "ready" => WorkState::Pending,
        "running" => WorkState::Active,
        "waiting" => WorkState::Waiting,
        "paused" => WorkState::Paused,
        "completed" => WorkState::Completed,
        "failed" => WorkState::Failed,
        "cancelled" => WorkState::Cancelled,
        "uncertain" => WorkState::Uncertain,
        _ => return Err(ApplicationError::Conflict),
    };
    let job_id = JobId::new(id).map_err(ApplicationError::storage)?;
    let actions = if matches!(
        state,
        WorkState::Pending | WorkState::Active | WorkState::Waiting | WorkState::Paused
    ) {
        vec![UiAction::Interrupt {
            job_id: job_id.clone(),
            turn_id: TurnId::new(
                row.try_get::<String, _>("turn_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
        }]
    } else {
        Vec::new()
    };
    publish(
        connection,
        owner,
        session,
        ItemRecord {
            id: format!("job:{id}"),
            parent_id: None,
            created_at: counter(row.try_get("time_created").map_err(database_error)?)?,
            item: SessionItem::Background {
                job_id,
                label: "Agent turn".to_owned(),
                state,
            },
            actions,
        },
    )
    .await
}

fn decode_part(
    row: sqlx_postgres::PgRow,
) -> Result<zuno_db::message::PartRecord, ApplicationError> {
    let mut data: Value = row.try_get("data").map_err(database_error)?;
    data["id"] = json!(row.try_get::<String, _>("id").map_err(database_error)?);
    data["sessionID"] = json!(
        row.try_get::<String, _>("session_id")
            .map_err(database_error)?
    );
    data["messageID"] = json!(
        row.try_get::<String, _>("message_id")
            .map_err(database_error)?
    );
    zuno_db::message::PartRecord::from_json(
        data,
        row.try_get("time_created").map_err(database_error)?,
    )
    .map_err(ApplicationError::storage)
}

/// Executed only by the schema owner inside the format migration transaction.
/// RLS is restored before the marker moves; runtime startup cannot run backfill.
pub(crate) async fn backfill(connection: &mut PgConnection) -> Result<(), ApplicationError> {
    sqlx_core::raw_sql::raw_sql(
        "ALTER TABLE zuno_enterprise_preview.session NO FORCE ROW LEVEL SECURITY",
    )
    .execute(&mut *connection)
    .await
    .map_err(database_error)?;
    let sessions = query("SELECT tenant_id,principal_id,id FROM zuno_enterprise_preview.session ORDER BY tenant_id,principal_id,id")
        .fetch_all(&mut *connection).await.map_err(database_error)?;
    for row in sessions {
        let owner = PrincipalKey {
            tenant_id: TenantId::new(
                row.try_get::<String, _>("tenant_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
            principal_id: PrincipalId::new(
                row.try_get::<String, _>("principal_id")
                    .map_err(database_error)?,
            )
            .map_err(ApplicationError::storage)?,
        };
        let session: String = row.try_get("id").map_err(database_error)?;
        query(
            "SELECT set_config('zuno.tenant_id',$1,true),set_config('zuno.principal_id',$2,true)",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(database_error)?;
        let rows = query("SELECT * FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 ORDER BY time_created,id")
            .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&session).fetch_all(&mut *connection).await.map_err(database_error)?;
        for row in rows {
            let mut data: Value = row.try_get("data").map_err(database_error)?;
            let id: String = row.try_get("id").map_err(database_error)?;
            data["id"] = json!(id);
            data["sessionID"] = json!(session);
            let record = zuno_db::message::MessageRecord::from_json(data)
                .map_err(ApplicationError::storage)?;
            message(connection, &owner, &record).await?;
            let parts = query("SELECT * FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND message_id=$4 ORDER BY time_created,id")
                .bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&session).bind(&id)
                .fetch_all(&mut *connection).await.map_err(database_error)?;
            for row in parts {
                part(connection, &owner, &decode_part(row)?).await?;
            }
        }
        let pending = query(
            "SELECT i.id,i.prompt,i.state,i.time_created FROM zuno_enterprise_preview.input i
             WHERE i.tenant_id=$1 AND i.principal_id=$2 AND i.session_id=$3
               AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.message m
                 WHERE m.tenant_id=i.tenant_id AND m.principal_id=i.principal_id AND m.id=i.id)
             ORDER BY i.admitted_sequence",
        )
        .bind(owner.tenant_id.as_str())
        .bind(owner.principal_id.as_str())
        .bind(&session)
        .fetch_all(&mut *connection)
        .await
        .map_err(database_error)?;
        for row in pending {
            publish_input(
                connection,
                &owner,
                &session,
                row.try_get("id").map_err(database_error)?,
                &row.try_get("prompt").map_err(database_error)?,
                row.try_get("state").map_err(database_error)?,
                row.try_get("time_created").map_err(database_error)?,
            )
            .await?;
        }
        for (table, column, kind) in [
            ("runtime_job", "job_id", "job"),
            ("operation_approval", "id", "approval"),
        ] {
            let ids: Vec<String> = sqlx_core::query_scalar::query_scalar(AssertSqlSafe(format!(
                "SELECT {column} FROM zuno_enterprise_preview.{table} WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 ORDER BY {column}"
            ))).bind(owner.tenant_id.as_str()).bind(owner.principal_id.as_str()).bind(&session)
                .fetch_all(&mut *connection).await.map_err(database_error)?;
            for id in ids {
                if kind == "job" {
                    job(connection, &owner, &session, &id).await?;
                } else {
                    approval(connection, &owner, &session, &id).await?;
                }
            }
        }
    }
    sqlx_core::raw_sql::raw_sql(
        "ALTER TABLE zuno_enterprise_preview.session FORCE ROW LEVEL SECURITY",
    )
    .execute(connection)
    .await
    .map_err(database_error)?;
    Ok(())
}
