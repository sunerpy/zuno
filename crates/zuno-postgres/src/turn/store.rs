use super::*;

#[async_trait]
impl TurnPersistence for PostgresTurnPersistence {
    async fn context_usage(
        &self,
        scope: &TurnStateScope,
    ) -> Result<zuno_engine::context_usage::ContextUsageSeed, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let seed = context::seed(&mut tx, scope).await?;
        self.commit_transaction(tx).await?;
        Ok(seed)
    }

    async fn commit_context_usage(
        &self,
        scope: &TurnStateScope,
        update: &zuno_types::context_usage::ContextUsageWrite,
    ) -> Result<(), TurnError> {
        let (mut tx, job) = self.transaction(scope).await?;
        context::write(&mut tx, scope, &job, update).await?;
        self.commit_transaction(tx).await
    }

    async fn start_provider_request(
        &self,
        scope: &TurnStateScope,
        commit: zuno_engine::state::ProviderRequestCommit,
    ) -> Result<zuno_engine::state::ProviderRequestReceipt, TurnError> {
        let (mut tx, job) = self.transaction(scope).await?;
        if commit.assistant.session_id != scope.session_id
            || commit.assistant.role != MessageRole::Assistant
            || commit.context.before.tracker.snapshot().session_id != scope.session_id
            || commit.context.identity.turn_id.as_deref() != Some(job.turn_id.as_str())
            || commit.event.event_type != "session.provider.request"
            || commit
                .event
                .properties
                .get("turnID")
                .and_then(Value::as_str)
                != Some(job.turn_id.as_str())
            || commit
                .event
                .properties
                .get("requestID")
                .and_then(Value::as_str)
                != Some(commit.context.identity.request_id.as_str())
            || commit
                .assistant
                .data
                .get("requestID")
                .and_then(Value::as_str)
                != Some(commit.context.identity.request_id.as_str())
        {
            return Err(TurnStateError::Conflict.into());
        }
        let existing = query(
            "SELECT * FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&commit.assistant.id)
            .fetch_optional(&mut *tx).await.map_err(sql_error)?;
        if let Some(existing) = existing {
            let previous = records::message(existing)?;
            if previous.session_id != scope.session_id
                || previous.role != MessageRole::Assistant
                || previous.time_created != commit.assistant.time_created
                || previous.data.contains_key("requestID")
                || previous
                    .data
                    .get("time")
                    .and_then(|time| time.get("completed"))
                    .is_some()
            {
                return Err(TurnStateError::Conflict.into());
            }
        }
        let at = database_time(&mut tx).await.map_err(state_error)?;
        records::put_message(&mut tx, scope, &commit.assistant, at).await?;
        let estimated = i64::try_from(commit.estimated_prompt_tokens)
            .map_err(|_| TurnStateError::InvalidData)?;
        let limit = commit
            .context_limit
            .map(i64::try_from)
            .transpose()
            .map_err(|_| TurnStateError::InvalidData)?;
        query("UPDATE zuno_enterprise_preview.session SET tokens_estimated_pending_prompt=$4,tokens_context_limit=$5 WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .bind(estimated).bind(limit).execute(&mut *tx).await.map_err(sql_error)?;
        let event = event(&mut tx, &job, commit.event).await?;
        let update = commit.context.with_sequence(
            u64::try_from(event.sequence).map_err(|_| TurnStateError::InvalidData)?,
        )?;
        context::write(&mut tx, scope, &job, &update).await?;
        self.commit_transaction(tx).await?;
        Ok(zuno_engine::state::ProviderRequestReceipt {
            event,
            context: update.tracker,
        })
    }

    async fn applicable_inputs(
        &self,
        scope: &TurnStateScope,
        candidates: &[String],
    ) -> Result<Vec<String>, TurnError> {
        let (mut tx, job) = self.transaction(scope).await?;
        let eligible: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.input_execution_receipt r
             JOIN zuno_enterprise_preview.input i ON i.tenant_id=r.tenant_id AND i.principal_id=r.principal_id AND i.id=r.input_id
             WHERE r.tenant_id=$1 AND r.principal_id=$2 AND r.session_id=$3 AND r.input_id=$4
               AND r.state='recorded' AND i.state='consumed' AND (r.turn_id IS NULL OR r.turn_id=$5))",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .bind(job.input_id.as_str()).bind(job.turn_id.as_str()).fetch_one(&mut *tx).await.map_err(sql_error)?;
        self.commit_transaction(tx).await?;
        Ok(
            if eligible && candidates.iter().any(|id| id == job.input_id.as_str()) {
                vec![job.input_id.to_string()]
            } else {
                Vec::new()
            },
        )
    }

    async fn mark_inputs_applied(
        &self,
        scope: &TurnStateScope,
        turn_id: &str,
        input_ids: &[String],
        _at_ms: i64,
    ) -> Result<(), TurnError> {
        let (mut tx, job) = self.transaction(scope).await?;
        if turn_id != job.turn_id.as_str() || input_ids.iter().any(|id| id != job.input_id.as_str())
        {
            return Err(TurnStateError::Conflict.into());
        }
        if !input_ids.is_empty() {
            let at = database_time(&mut tx).await.map_err(state_error)?;
            let changed = query(
                "UPDATE zuno_enterprise_preview.input_execution_receipt SET state='applied',turn_id=$5,applied_at=COALESCE(applied_at,$6),time_updated=$6
                 WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND input_id=$4
                   AND state='recorded' AND (turn_id IS NULL OR turn_id=$5)",
            ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
                .bind(job.input_id.as_str()).bind(turn_id).bind(at).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
            if changed == 0 {
                let applied: bool = query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM zuno_enterprise_preview.input_execution_receipt
                     WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND input_id=$4 AND turn_id=$5 AND state='applied')",
                ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
                    .bind(job.input_id.as_str()).bind(turn_id).fetch_one(&mut *tx).await.map_err(sql_error)?;
                if !applied {
                    return Err(TurnStateError::Conflict.into());
                }
                return self.commit_transaction(tx).await;
            }
            event(
                &mut tx,
                &job,
                NewSessionEvent::new(
                    "session.input.applied",
                    json!({
                        "inputID":job.input_id,"turnID":job.turn_id,"appliedAt":at,
                    })
                    .as_object()
                    .expect("fixed envelope")
                    .clone(),
                )?,
            )
            .await?;
        }
        self.commit_transaction(tx).await
    }

    async fn session(&self, scope: &TurnStateScope) -> Result<TurnSession, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let parent_id: Option<String> = query_scalar(
            "SELECT parent_id FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        self.commit_transaction(tx).await?;
        Ok(TurnSession {
            id: scope.session_id.clone(),
            parent_id,
            directory: self.executor_directory.clone(),
        })
    }

    async fn clock(&self, scope: &TurnStateScope) -> Result<i64, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let time = database_time(&mut tx).await.map_err(state_error)?;
        self.commit_transaction(tx).await?;
        Ok(time)
    }

    async fn touch(&self, scope: &TurnStateScope) -> Result<(), TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let time = database_time(&mut tx).await.map_err(state_error)?;
        query("UPDATE zuno_enterprise_preview.session SET time_updated=GREATEST(time_updated,$4) WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(time)
            .execute(&mut *tx).await.map_err(sql_error)?;
        self.commit_transaction(tx).await
    }

    async fn repair_history(&self, scope: &TurnStateScope) -> Result<usize, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let at = database_time(&mut tx).await.map_err(state_error)?;
        let unfinished = history::unfinished(&mut tx, scope).await?;
        if !unfinished.is_empty()
            && let Some(event) = journal::latest(&mut tx, scope).await?
            && zuno_engine::advance::protects_unfinished(&event, &unfinished)
                .map_err(|_| TurnStateError::InvalidData)?
        {
            return Err(TurnStateError::Conflict.into());
        }
        let mut count = 0;
        for part in unfinished {
            if let Some(part) = zuno_engine::r#loop::repair_unanswered_tool_part(part, at) {
                records::put_part(&mut tx, scope, &part, at).await?;
                count += 1;
            }
        }
        self.commit_transaction(tx).await?;
        Ok(count)
    }

    async fn has_uncertain_calls(&self, scope: &TurnStateScope) -> Result<bool, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let value = history::uncertain(&mut tx, scope).await?;
        self.commit_transaction(tx).await?;
        Ok(value)
    }

    async fn history(&self, scope: &TurnStateScope) -> Result<Vec<MessageWithParts>, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let value = history::retained(&mut tx, scope).await?;
        self.commit_transaction(tx).await?;
        Ok(value)
    }

    async fn legacy_tool_schemas(
        &self,
        scope: &TurnStateScope,
    ) -> Result<LegacyToolSchemas, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let value = history::legacy(&mut tx, scope).await?;
        self.commit_transaction(tx).await?;
        Ok(value)
    }

    async fn developer_contexts(
        &self,
        scope: &TurnStateScope,
        messages: &[MessageWithParts],
        known: &DeveloperContexts,
    ) -> Result<DeveloperContexts, TurnError> {
        if messages
            .iter()
            .any(|message| message.info.session_id != scope.session_id)
        {
            return Err(TurnStateError::Conflict.into());
        }
        let (mut tx, _) = self.transaction(scope).await?;
        let value = history::developer_contexts(&mut tx, scope, messages, known).await?;
        self.commit_transaction(tx).await?;
        Ok(value)
    }

    async fn context_epoch(&self, scope: &TurnStateScope) -> Result<i64, TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let value = query_scalar(
            "SELECT context_epoch FROM zuno_enterprise_preview.session WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        self.commit_transaction(tx).await?;
        Ok(value)
    }

    async fn commit_assistant(
        &self,
        scope: &TurnStateScope,
        commit: &AssistantCommit,
    ) -> Result<(), TurnError> {
        if commit.message.session_id != scope.session_id {
            return Err(TurnStateError::Conflict.into());
        }
        let (mut tx, job) = self.transaction(scope).await?;
        let previous = records::find_message(&mut tx, scope, &commit.message.id).await?;
        let mut parts = Vec::with_capacity(commit.parts.len());
        for part in &commit.parts {
            if let Some(previous) = records::find_part(&mut tx, scope, &part.id).await? {
                parts.push(previous);
            }
        }
        zuno_db::assistant_commit::validate_commit(commit, previous.as_ref(), &parts)?;
        records::put_message(&mut tx, scope, &commit.message, commit.persisted_at_ms).await?;
        for part in &commit.parts {
            records::put_part(&mut tx, scope, part, commit.persisted_at_ms).await?;
        }
        records::usage(
            &mut tx,
            scope,
            previous
                .as_ref()
                .map(|message| zuno_db::session::MessageUsage::from_data(&message.data)),
            zuno_db::session::MessageUsage::from_data(&commit.message.data),
            commit.context_limit,
        )
        .await?;
        if let Some(update) = &commit.context_usage {
            context::write(&mut tx, scope, &job, update).await?;
        }
        self.commit_transaction(tx).await
    }

    async fn append_event(
        &self,
        scope: &TurnStateScope,
        draft: NewSessionEvent,
        update: ProviderEventUpdate,
    ) -> Result<SessionEvent, TurnError> {
        if !matches!(
            draft.event_type.as_str(),
            "session.turn.started"
                | "session.turn.rejected"
                | "session.prompt.assembled"
                | "session.provider.request"
                | "session.provider.attempt"
        ) {
            return Err(TurnStateError::Forbidden.into());
        }
        let (mut tx, job) = self.transaction(scope).await?;
        let turn = draft
            .properties
            .get("turnID")
            .or_else(|| draft.properties.get("turnId"))
            .and_then(Value::as_str);
        if turn != Some(job.turn_id.as_str()) {
            return Err(TurnStateError::Conflict.into());
        }
        match update {
            ProviderEventUpdate::None => {}
            ProviderEventUpdate::RequestStarted {
                estimated_prompt_tokens,
                context_limit,
            } => {
                if draft.event_type != "session.provider.request"
                    || draft.properties.get("status").and_then(Value::as_str) != Some("started")
                {
                    return Err(TurnStateError::Conflict.into());
                }
                query(
                    "UPDATE zuno_enterprise_preview.session SET tokens_estimated_pending_prompt=$4,tokens_context_limit=COALESCE($5,tokens_context_limit)
                     WHERE tenant_id=$1 AND principal_id=$2 AND id=$3",
                ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
                    .bind(i64::try_from(estimated_prompt_tokens).map_err(|_|TurnStateError::InvalidData)?)
                    .bind(context_limit.map(i64::try_from).transpose().map_err(|_|TurnStateError::InvalidData)?)
                    .execute(&mut *tx).await.map_err(sql_error)?;
            }
            ProviderEventUpdate::AttemptStarted => {
                if draft.event_type != "session.provider.attempt"
                    || draft.properties.get("status").and_then(Value::as_str) != Some("started")
                {
                    return Err(TurnStateError::Conflict.into());
                }
                query("DELETE FROM zuno_enterprise_preview.provider_retry_backoff WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3")
                    .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
                    .execute(&mut *tx).await.map_err(sql_error)?;
            }
            ProviderEventUpdate::RequestFinished { request_id } => {
                if draft.event_type != "session.provider.request"
                    || draft.properties.get("requestID").and_then(Value::as_str)
                        != Some(&request_id)
                {
                    return Err(TurnStateError::Conflict.into());
                }
                query("DELETE FROM zuno_enterprise_preview.provider_retry_backoff WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND request_id=$4")
                    .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(request_id)
                    .execute(&mut *tx).await.map_err(sql_error)?;
            }
        }
        let value = event(&mut tx, &job, draft).await?;
        self.commit_transaction(tx).await?;
        Ok(value)
    }

    async fn commit_tool_parts(
        &self,
        scope: &TurnStateScope,
        parts: &[PartRecord],
        kind: ToolPartCommitKind,
        at: i64,
    ) -> Result<(), TurnError> {
        let (mut tx, _) = self.transaction(scope).await?;
        let mut ids = std::collections::BTreeSet::new();
        for part in parts {
            if part.session_id != scope.session_id
                || part.kind != PartKind::Tool
                || !ids.insert(&part.id)
            {
                return Err(TurnStateError::Conflict.into());
            }
            let previous = records::find_part(&mut tx, scope, &part.id)
                .await?
                .ok_or(TurnStateError::NotFound)?;
            let old = previous
                .data
                .get("state")
                .ok_or(TurnStateError::InvalidData)?;
            let new = part.data.get("state").ok_or(TurnStateError::InvalidData)?;
            if previous.session_id != part.session_id
                || previous.message_id != part.message_id
                || previous.data.get("callID") != part.data.get("callID")
                || previous.data.get("tool") != part.data.get("tool")
                || old.get("input") != new.get("input")
                || (kind == ToolPartCommitKind::Dispatched && old.get("dispatchedAtMs").is_some())
                || (matches!(
                    old.get("status").and_then(Value::as_str),
                    Some("completed" | "error")
                ) && (kind != ToolPartCommitKind::Result || previous.data != part.data))
            {
                return Err(TurnStateError::Conflict.into());
            }
            records::put_part(&mut tx, scope, part, at).await?;
        }
        self.commit_transaction(tx).await
    }

    async fn consume_input(
        &self,
        scope: &TurnStateScope,
        mut input: InputMaterialization,
    ) -> Result<(), TurnError> {
        let (mut tx, job) = self.transaction(scope).await?;
        let id = input
            .input_id
            .as_deref()
            .ok_or(TurnStateError::InvalidData)?;
        if id != job.input_id.as_str()
            || input
                .turn_id
                .as_deref()
                .is_some_and(|turn| turn != job.turn_id.as_str())
            || input.message.id != id
            || input.message.role != MessageRole::User
            || input.message.session_id != scope.session_id
            || input
                .parts
                .iter()
                .any(|part| part.session_id != scope.session_id || part.message_id != id)
        {
            return Err(TurnStateError::Conflict.into());
        }
        let row = query(
            "SELECT prompt,state FROM zuno_enterprise_preview.input WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3 AND id=$4 FOR UPDATE",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id).bind(id)
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        let prompt: Value = row.try_get("prompt").map_err(sql_error)?;
        let phase: String = row.try_get("state").map_err(sql_error)?;
        if !matches!(phase.as_str(), "queued" | "promoted")
            || input.parts.len() != 1
            || input.parts[0].kind != PartKind::Text
            || input.parts[0].data.get("text") != prompt.pointer("/prompt/text")
        {
            return Err(TurnStateError::Conflict.into());
        }
        match prompt.get("kind").and_then(Value::as_str) {
            Some("user") => {}
            Some("delegation") => {
                crate::runtime::children::validate_delegation_input(&mut tx, &job, &prompt)
                    .await
                    .map_err(state_error)?
            }
            Some("completion") => {
                crate::runtime::children::validate_completion_input(&mut tx, &job, &prompt)
                    .await
                    .map_err(state_error)?
            }
            _ => return Err(TurnStateError::InvalidData.into()),
        }
        input.message.data.insert("inputSource".to_owned(),json!({
            "kind":prompt["kind"],"completion":prompt.get("completion"),
            "parentSessionID":prompt.get("parentSessionID"),"parentJobID":prompt.get("parentJobID"),
        }));
        for (field, key) in [("agent", "agent"), ("model", "model")] {
            if let Some(expected) = prompt.get(field).filter(|value| !value.is_null())
                && input.message.data.get(key) != Some(expected)
            {
                return Err(TurnStateError::Conflict.into());
            }
        }
        if records::find_message(&mut tx, scope, id).await?.is_some() {
            return Err(TurnStateError::Conflict.into());
        }
        let latest: Option<i64> = query_scalar(
            "SELECT max(time_created) FROM zuno_enterprise_preview.message WHERE tenant_id=$1 AND principal_id=$2 AND session_id=$3",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .fetch_one(&mut *tx).await.map_err(sql_error)?;
        let time = zuno_db::message::created_after(
            database_time(&mut tx).await.map_err(state_error)?,
            latest,
        );
        input.message.time_created = time;
        input
            .message
            .data
            .insert("time".to_owned(), json!({"created":time}));
        records::put_message(&mut tx, scope, &input.message, time).await?;
        for mut part in input.parts {
            if records::find_part(&mut tx, scope, &part.id)
                .await?
                .is_some()
            {
                return Err(TurnStateError::Conflict.into());
            }
            part.time_created = time;
            records::put_part(&mut tx, scope, &part, time).await?;
        }
        query("UPDATE zuno_enterprise_preview.input SET state='consumed' WHERE tenant_id=$1 AND principal_id=$2 AND id=$3")
            .bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(id)
            .execute(&mut *tx).await.map_err(sql_error)?;
        let recorded = query(
            "UPDATE zuno_enterprise_preview.input_execution_receipt SET state='recorded',turn_id=$4,time_updated=$5
             WHERE tenant_id=$1 AND principal_id=$2 AND input_id=$3 AND state='admitted' AND (turn_id IS NULL OR turn_id=$4)",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(id)
            .bind(job.turn_id.as_str()).bind(time).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
        if recorded != 1 {
            return Err(TurnStateError::Conflict.into());
        }

        event(
            &mut tx,
            &job,
            NewSessionEvent::new(
                "session.input.consumed",
                json!({"inputID":id,"state":"consumed"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )?,
        )
        .await?;
        self.commit_transaction(tx).await
    }

    async fn schedule_backoff(
        &self,
        scope: &TurnStateScope,
        checkpoint: ProviderBackoffCheckpoint,
    ) -> Result<(), TurnError> {
        let (mut tx, job) = self.transaction(scope).await?;
        if checkpoint.session_id != scope.session_id
            || checkpoint.turn_id != job.turn_id.as_str()
            || checkpoint.delay_ms <= 0
            || checkpoint.failed_attempt == 0
            || checkpoint.next_attempt <= checkpoint.failed_attempt
            || checkpoint.max_attempts < checkpoint.next_attempt
            || checkpoint.reason.len() > 256
        {
            return Err(TurnStateError::InvalidData.into());
        }
        let at = database_time(&mut tx).await.map_err(state_error)?;
        let retry_at = at
            .checked_add(checkpoint.delay_ms)
            .ok_or(TurnStateError::InvalidData)?;
        query(
            "INSERT INTO zuno_enterprise_preview.provider_retry_backoff(
              tenant_id,principal_id,session_id,request_id,turn_id,failed_attempt,next_attempt,max_attempts,reason,delay_ms,retry_at_ms,scheduled_at_ms)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT(tenant_id,principal_id,session_id) DO UPDATE SET
               request_id=excluded.request_id,turn_id=excluded.turn_id,failed_attempt=excluded.failed_attempt,
               next_attempt=excluded.next_attempt,max_attempts=excluded.max_attempts,reason=excluded.reason,
               delay_ms=excluded.delay_ms,retry_at_ms=excluded.retry_at_ms,scheduled_at_ms=excluded.scheduled_at_ms",
        ).bind(scope.owner.tenant_id.as_str()).bind(scope.owner.principal_id.as_str()).bind(&scope.session_id)
            .bind(checkpoint.request_id).bind(checkpoint.turn_id)
            .bind(i32::try_from(checkpoint.failed_attempt).map_err(|_|TurnStateError::InvalidData)?)
            .bind(i32::try_from(checkpoint.next_attempt).map_err(|_|TurnStateError::InvalidData)?)
            .bind(i32::try_from(checkpoint.max_attempts).map_err(|_|TurnStateError::InvalidData)?)
            .bind(checkpoint.reason).bind(checkpoint.delay_ms).bind(retry_at).bind(at)
            .execute(&mut *tx).await.map_err(sql_error)?;
        self.commit_transaction(tx).await
    }

    async fn begin_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
    ) -> Result<BeginAdvance, AdvanceError> {
        journal::begin(self, scope, request).await
    }

    async fn commit_advance(
        &self,
        scope: &TurnStateScope,
        request: &AdvanceRequest,
        admission: &AdvanceAdmission,
        state: AdvanceState,
    ) -> Result<CheckpointRef, AdvanceError> {
        journal::commit(self, scope, request, admission, state).await
    }
}
