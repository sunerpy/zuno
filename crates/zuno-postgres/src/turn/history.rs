use super::*;

pub(super) async fn unfinished(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
) -> Result<Vec<PartRecord>, TurnError> {
    query(
        "SELECT * FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND kind='tool' AND COALESCE(data#>>'{state,status}','') NOT IN('completed','error') ORDER BY id",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_all(&mut **tx).await.map_err(sql_error)?.into_iter().map(records::part).collect()
}

pub(super) async fn uncertain(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
) -> Result<bool, TurnError> {
    query_scalar(
        "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND kind='tool' AND data#>>'{state,outcome}'='uncertain'
         AND (data#>'{state,uncertain,reconciledAtMs}' IS NULL OR data#>'{state,uncertain,reconciledAtMs}'='null'::jsonb))",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_one(&mut **tx).await.map_err(sql_error)
}

pub(super) async fn retained(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
) -> Result<Vec<MessageWithParts>, TurnError> {
    let rows = query(
        "SELECT * FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 ORDER BY time_created,id",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_all(&mut **tx).await.map_err(sql_error)?;
    let mut messages = rows
        .into_iter()
        .map(|row| {
            Ok(MessageWithParts {
                info: records::message(row)?,
                parts: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, TurnError>>()?;
    // Only markers and candidate summary text are decoded before selecting the
    // retained suffix. A discarded head's tool/reasoning payload stays unread.
    let sparse = query(
        "SELECT p.* FROM zuno_enterprise_preview.part p JOIN zuno_enterprise_preview.message m
           ON m.tenant_id=p.tenant_id AND m.principal_id=p.principal_id AND m.id=p.message_id AND m.session_id=p.session_id
         WHERE p.tenant_id=$1 AND p.principal_id=$2 AND p.session_id=$3
           AND (p.kind='compaction' OR (p.kind='text' AND m.data->'summary'='true'::jsonb))
         ORDER BY p.time_created,p.id",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_all(&mut **tx).await.map_err(sql_error)?;
    let positions = messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message.info.id.clone(), index))
        .collect::<std::collections::BTreeMap<_, _>>();
    for row in sparse {
        let part = records::part(row)?;
        let index = positions
            .get(&part.message_id)
            .ok_or(TurnStateError::InvalidData)?;
        messages[*index].parts.push(part);
    }
    let checkpoint = zuno_engine::compaction::checkpoint::latest_checkpoint(&messages)
        .map(|checkpoint| (checkpoint.tail_index, checkpoint.summary.info.id.clone()));
    if let Some((start, _)) = &checkpoint {
        messages.drain(..*start);
    }
    messages.retain(|message| {
        zuno_engine::compaction::checkpoint::visible_in_history(
            message,
            checkpoint.as_ref().map(|(_, id)| id.as_str()),
        )
    });
    for message in &mut messages {
        message.parts.clear();
    }
    let ids = messages
        .iter()
        .map(|message| message.info.id.clone())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(messages);
    }
    let rows = query(
        "SELECT * FROM zuno_enterprise_preview.part WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND message_id=ANY($4) ORDER BY time_created,id",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(&ids)
        .fetch_all(&mut **tx).await.map_err(sql_error)?;
    let positions = messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message.info.id.clone(), index))
        .collect::<std::collections::BTreeMap<_, _>>();
    for row in rows {
        let part = records::part(row)?;
        let index = positions
            .get(&part.message_id)
            .ok_or(TurnStateError::InvalidData)?;
        messages[*index].parts.push(part);
    }
    Ok(messages)
}

pub(super) async fn developer_contexts(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
    history: &[MessageWithParts],
    known: &DeveloperContexts,
) -> Result<DeveloperContexts, TurnError> {
    let mut contexts = DeveloperContexts::new();
    for id in zuno_engine::r#loop::historical_developer_boundary_targets(history) {
        if known.contains_key(&id) {
            continue;
        }
        let direct = history
            .iter()
            .find(|message| message.info.id == id)
            .and_then(|message| message.info.data.get("precedingDeveloperPromptReceiptID"))
            .and_then(Value::as_str);
        let receipt: Option<String> = if let Some(id) = direct {
            Some(id.to_owned())
        } else {
            query_scalar(
                "SELECT data->>'promptReceiptID' FROM zuno_enterprise_preview.event
                 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND type='session.provider.request'
                   AND version=1 AND data->>'status'='started' AND data->>'assistantMessageID'=$4
                   AND jsonb_typeof(data->'promptReceiptID')='string'
                 ORDER BY sequence DESC LIMIT 1",
            ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(&id)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?
        };
        let context = if let Some(receipt) = receipt {
            let value: Option<Value> = query_scalar(
                "SELECT data FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4",
            ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(receipt)
                .fetch_optional(&mut **tx).await.map_err(sql_error)?;
            value
                .as_ref()
                .and_then(zuno_engine::r#loop::historical_developer_context_from_prompt)
        } else {
            None
        };
        contexts.insert(id, context);
    }
    Ok(contexts)
}

pub(super) async fn legacy(
    tx: &mut Transaction<'_, Postgres>,
    scope: &TurnStateScope,
) -> Result<LegacyToolSchemas, TurnError> {
    let mut snapshots = LegacyToolSchemas::new();
    let rows = query(
        "SELECT data FROM zuno_enterprise_preview.event WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3
         AND type='session.provider.request' AND version=1 AND data->>'status'='started'
         AND jsonb_typeof(data#>'{orchestrationSnapshot,tools}')='array' ORDER BY sequence",
    ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
        .fetch_all(&mut **tx).await.map_err(sql_error)?;
    for row in rows {
        let data: Value = row.try_get("data").map_err(sql_error)?;
        let Some(message) = data.get("assistantMessageID").and_then(Value::as_str) else {
            continue;
        };
        let Some(tools) = data
            .pointer("/orchestrationSnapshot/tools")
            .and_then(Value::as_array)
        else {
            continue;
        };
        let tools = tools
            .iter()
            .filter_map(|tool| {
                Some(zuno_orchestration::ToolSchemaIdentity {
                    name: tool.get("name")?.as_str()?.to_owned(),
                    description_sha256: tool.get("descriptionSha256")?.as_str()?.to_owned(),
                    schema_sha256: tool.get("schemaSha256")?.as_str()?.to_owned(),
                    replay_schema_sha256: tool
                        .get("replaySchemaSha256")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    ui_intent: tool
                        .get("uiIntent")
                        .and_then(Value::as_str)
                        .unwrap_or("generic")
                        .to_owned(),
                })
            })
            .map(|tool| (tool.name.clone(), tool))
            .collect::<std::collections::BTreeMap<_, _>>();
        if !tools.is_empty() {
            snapshots.insert(message.to_owned(), tools);
        }
    }
    Ok(snapshots)
}
