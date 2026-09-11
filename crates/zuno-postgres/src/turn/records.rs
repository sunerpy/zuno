use super::*;

pub(super) fn message(row: PgRow) -> Result<MessageRecord, TurnError> {
    let mut data: Value = row.try_get("data").map_err(sql_error)?;
    data["id"] = json!(row.try_get::<String, _>("id").map_err(sql_error)?);
    data["sessionID"] = json!(row.try_get::<String, _>("session_id").map_err(sql_error)?);
    let record = MessageRecord::from_json(data)?;
    let role: String = row.try_get("role").map_err(sql_error)?;
    let expected = match record.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    };
    if role != expected
        || record.time_created != row.try_get::<i64, _>("time_created").map_err(sql_error)?
    {
        return Err(TurnStateError::InvalidData.into());
    }
    Ok(record)
}

pub(super) fn part(row: PgRow) -> Result<PartRecord, TurnError> {
    let mut data: Value = row.try_get("data").map_err(sql_error)?;
    data["id"] = json!(row.try_get::<String, _>("id").map_err(sql_error)?);
    data["sessionID"] = json!(row.try_get::<String, _>("session_id").map_err(sql_error)?);
    data["messageID"] = json!(row.try_get::<String, _>("message_id").map_err(sql_error)?);
    if data.get("type").and_then(Value::as_str)
        != Some(row.try_get::<&str, _>("kind").map_err(sql_error)?)
    {
        return Err(TurnStateError::InvalidData.into());
    }
    PartRecord::from_json(data, row.try_get("time_created").map_err(sql_error)?).map_err(Into::into)
}

pub(super) async fn find_message(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    id: &str,
) -> Result<Option<MessageRecord>, TurnError> {
    query("SELECT * FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(id)
        .fetch_optional(&mut **tx).await.map_err(sql_error)?.map(message).transpose()
}

pub(super) async fn find_part(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    id: &str,
) -> Result<Option<PartRecord>, TurnError> {
    query("SELECT * FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
        .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(id)
        .fetch_optional(&mut **tx).await.map_err(sql_error)?.map(part).transpose()
}

pub(super) async fn put_message(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    message: &MessageRecord,
    at: i64,
) -> Result<(), TurnError> {
    if message.session_id != scope.session_id {
        return Err(TurnStateError::Conflict.into());
    }
    let role = match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    };
    let changed = query(
        "INSERT INTO zuno_enterprise_preview.message(tenant_id,principal_id,session_id,id,role,data,time_created,time_updated)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT(tenant_id,principal_id,id) DO UPDATE SET data=excluded.data,time_updated=excluded.time_updated
         WHERE message.session_id=excluded.session_id AND message.role=excluded.role AND message.time_created=excluded.time_created",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .bind(&message.id).bind(role).bind(json!(message.data)).bind(message.time_created).bind(at)
        .execute(&mut **tx).await.map_err(sql_error)?.rows_affected();
    if changed != 1 {
        return Err(TurnStateError::Conflict.into());
    }
    Ok(())
}

pub(super) async fn put_part(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    part: &PartRecord,
    at: i64,
) -> Result<(), TurnError> {
    if part.session_id != scope.session_id {
        return Err(TurnStateError::Conflict.into());
    }
    let kind = part
        .data
        .get("type")
        .and_then(Value::as_str)
        .ok_or(TurnStateError::InvalidData)?;
    let changed = query(
        "INSERT INTO zuno_enterprise_preview.part(tenant_id,principal_id,session_id,message_id,id,kind,data,time_created,time_updated)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)
         ON CONFLICT(tenant_id,principal_id,id) DO UPDATE SET data=excluded.data,time_updated=excluded.time_updated
         WHERE part.session_id=excluded.session_id AND part.message_id=excluded.message_id
           AND part.kind=excluded.kind AND part.time_created=excluded.time_created",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .bind(&part.message_id).bind(&part.id).bind(kind).bind(json!(part.data)).bind(part.time_created).bind(at)
        .execute(&mut **tx).await.map_err(sql_error)?.rows_affected();
    if changed != 1 {
        return Err(TurnStateError::Conflict.into());
    }
    Ok(())
}

pub(super) async fn usage(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    previous: Option<zuno_db::session::MessageUsage>,
    current: zuno_db::session::MessageUsage,
    context_limit: Option<i64>,
) -> Result<(), TurnError> {
    if !current.reported {
        return Ok(());
    }
    let row = query(
        "SELECT tokens_known,
          EXISTS(SELECT 1 FROM zuno_enterprise_preview.message m WHERE m.tenant_id=s.tenant_id AND m.principal_id=s.principal_id
            AND m.session_id=s.id AND m.role='assistant' AND jsonb_typeof(m.data->'tokens')='object')
          AND NOT EXISTS(SELECT 1 FROM zuno_enterprise_preview.message m WHERE m.tenant_id=s.tenant_id AND m.principal_id=s.principal_id
            AND m.session_id=s.id AND m.role='assistant' AND jsonb_typeof(m.data->'tokens')='object'
            AND COALESCE(m.data#>>'{tokens,accounting}','') NOT IN('cache-inside-input','cache-beside-input')) AS all_known
         FROM zuno_enterprise_preview.session s WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_one(&mut **tx).await.map_err(sql_error)?;
    let was_known: bool = row.try_get("tokens_known").map_err(sql_error)?;
    let all_known: bool = row.try_get("all_known").map_err(sql_error)?;
    let mut recomputed_cost = None;
    if all_known && was_known {
        let old = previous
            .and_then(zuno_db::session::MessageUsage::normalized)
            .unwrap_or_default();
        let new = current.normalized().unwrap_or_default();
        query(
            "UPDATE zuno_enterprise_preview.session SET
               tokens_input=GREATEST(0,tokens_input+$4-$5),tokens_output=GREATEST(0,tokens_output+$6-$7),
               tokens_reasoning=GREATEST(0,tokens_reasoning+$8-$9),tokens_cache_read=GREATEST(0,tokens_cache_read+$10-$11),
               tokens_cache_write=GREATEST(0,tokens_cache_write+$12-$13)
             WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .bind(new.input).bind(old.input).bind(new.output).bind(old.output).bind(new.reasoning).bind(old.reasoning)
            .bind(new.cache_read).bind(old.cache_read).bind(new.cache_write).bind(old.cache_write)
            .execute(&mut **tx).await.map_err(sql_error)?;
    } else if all_known {
        // This path runs only when the previously incomplete accounting becomes
        // known. The normal case above updates by the exact message delta.
        let rows = query(
            "SELECT * FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND role='assistant'",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .fetch_all(&mut **tx).await.map_err(sql_error)?;
        let mut totals = zuno_db::session::Tokens::default();
        let mut total_cost = 0.0;
        for row in rows {
            let usage = zuno_db::session::MessageUsage::from_data(&message(row)?.data);
            total_cost += usage.cost;
            if let Some(tokens) = usage.normalized() {
                totals.input = totals.input.saturating_add(tokens.input);
                totals.output = totals.output.saturating_add(tokens.output);
                totals.reasoning = totals.reasoning.saturating_add(tokens.reasoning);
                totals.cache_read = totals.cache_read.saturating_add(tokens.cache_read);
                totals.cache_write = totals.cache_write.saturating_add(tokens.cache_write);
            }
        }
        recomputed_cost = Some(total_cost);
        query(
            "UPDATE zuno_enterprise_preview.session SET tokens_input=$4,tokens_output=$5,tokens_reasoning=$6,
             tokens_cache_read=$7,tokens_cache_write=$8 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .bind(totals.input).bind(totals.output).bind(totals.reasoning).bind(totals.cache_read).bind(totals.cache_write)
            .execute(&mut **tx).await.map_err(sql_error)?;
    }
    let at = database_time(tx).await.map_err(state_error)?;
    query(
        "UPDATE zuno_enterprise_preview.session SET cost=COALESCE($11,GREATEST(0.0,cost+$4-$5)),
           tokens_last_prompt=$6,tokens_context_limit=COALESCE($7,tokens_context_limit),tokens_accounting=$8,
           tokens_known=$9,tokens_estimated_pending_prompt=NULL,tokens_last_confirmed_at=$10
         WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .bind(current.cost).bind(previous.map_or(0.0,|usage|usage.cost)).bind(current.last_prompt_tokens())
        .bind(context_limit.filter(|limit|*limit>0)).bind(current.accounting.map(|accounting|accounting.as_str()))
        .bind(all_known).bind(at).bind(recomputed_cost).execute(&mut **tx).await.map_err(sql_error)?;
    Ok(())
}
