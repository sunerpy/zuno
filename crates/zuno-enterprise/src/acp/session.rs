use super::*;
use zuno_application::{
    control::{CancelJob, CancellationReceipt},
    runtime::JobPhase,
};
use zuno_types::activity::{CommittedEvent, CommittedFrame, HistoryPage, LiveFrame};

impl Bridge {
    pub(super) async fn load(
        &self,
        params: &Value,
        client: ClientConnection,
        replay: bool,
    ) -> Result<Value, RpcError> {
        self.cwd(params)?;
        let id = SessionId::new(required(params, "sessionId")?).map_err(|_| invalid_rpc())?;
        let summary: SessionSummary = self.api.get(&format!("sessions/{id}")).await.map_err(rpc)?;
        if summary.id != id {
            return Err(invalid_rpc());
        }
        let session = self.attach(summary, Counter(0)).await?;
        let _guard = session
            .observer
            .try_lock()
            .map_err(|_| RpcError::session_busy("A session operation is still active"))?;
        if session.current.lock().await.is_some() {
            return Err(RpcError::session_busy("A prompt observer is still active"));
        }
        let page: HistoryPage = self
            .api
            .get(&format!("sessions/{id}/history?limit=100"))
            .await
            .map_err(rpc)?;
        validate_history(&page, &id)?;
        let mut cursor = session.cursor.lock().await;
        if replay {
            client
                .notify(
                    "_zuno/history",
                    serde_json::to_value(&page).map_err(|_| invalid_rpc())?,
                )
                .await?;
            for item in &page.items {
                activity::publish(
                    &client,
                    &id,
                    &CommittedFrame {
                        version: page.version,
                        session_id: id.clone(),
                        sequence: item.revision,
                        event: CommittedEvent::Upsert {
                            position: item.position,
                            record: Box::new(item.record.clone()),
                        },
                    },
                    true,
                )
                .await?;
            }
        }
        *cursor = page.through;
        Ok(
            json!({"sessionId":id,"_meta":{"zuno":{"history":{"through":page.through,"before":page.before},
            "historyMethod":"_zuno/history","jobMethod":"_zuno/job","observeMethod":"_zuno/observe","workspaceId":self.options.workspace_id}}}),
        )
    }
    pub(super) async fn history(&self, params: &Value) -> Result<Value, RpcError> {
        let session = self.session(params).await?;
        let before: Counter =
            serde_json::from_value(params["before"].clone()).map_err(|_| invalid_rpc())?;
        let through: Counter =
            serde_json::from_value(params["through"].clone()).map_err(|_| invalid_rpc())?;
        let page: HistoryPage = self
            .api
            .get(&format!(
                "sessions/{}/history?limit=100&before={}&through={}",
                session.id, before.0, through.0
            ))
            .await
            .map_err(rpc)?;
        validate_history(&page, &session.id)?;
        if page.through != through || page.items.iter().any(|i| i.position >= before) {
            return Err(invalid_rpc());
        }
        serde_json::to_value(page).map_err(|_| invalid_rpc())
    }
    pub(super) async fn job(&self, params: &Value) -> Result<Value, RpcError> {
        let session = self.session(params).await?;
        let id = JobId::new(required(params, "jobId")?).map_err(|_| invalid_rpc())?;
        let job: JobView = self.api.get(&format!("jobs/{id}")).await.map_err(rpc)?;
        if job.id != id || job.session_id != session.id {
            return Err(invalid_rpc());
        }
        serde_json::to_value(job).map_err(|_| invalid_rpc())
    }
    pub(super) async fn resolve_request(&self, params: &Value) -> Result<Value, RpcError> {
        let session = self.session(params).await?;
        let id = RequestId::new(required(params, "requestId")?).map_err(|_| invalid_rpc())?;
        let job: JobView = self
            .api
            .get(&format!("sessions/{}/requests/{id}", session.id))
            .await
            .map_err(rpc)?;
        if job.session_id != session.id {
            return Err(invalid_rpc());
        }
        serde_json::to_value(job).map_err(|_| invalid_rpc())
    }
    pub(super) async fn prompt(
        &self,
        request: &zuno_acp::RequestId,
        params: &Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        let session = self.session(params).await?;
        let text = prompt_text(params)?;
        let _guard = session
            .observer
            .try_lock()
            .map_err(|_| RpcError::session_busy("A session operation is still active"))?;
        let durable = match params.pointer("/_meta/zuno/requestId") {
            Some(value) => RequestId::new(value.as_str().ok_or_else(invalid_rpc)?)
                .map_err(|_| invalid_rpc())?,
            None => self.durable(request, "prompt"),
        };
        let prompt = Arc::new(Prompt {
            request: request.clone(),
            durable,
            cancelled: InterruptSignal::new(),
            job: Mutex::new(None),
        });
        {
            let mut current = session.current.lock().await;
            if current.is_some() {
                return Err(RpcError::session_busy(
                    "This ACP connection already observes a prompt for this session",
                )
                .with_data(json!({"zuno":{"accepted":false}})));
            }
            *current = Some(prompt.clone());
        }
        if client.is_request_cancelled() {
            prompt.cancelled.fire();
        }
        let result = self.observe(&session, &prompt, text, client).await;
        let mut current = session.current.lock().await;
        if current.as_ref().is_some_and(|p| Arc::ptr_eq(p, &prompt)) {
            *current = None;
        }
        result
    }
    pub(super) async fn observe_existing(
        &self,
        request: &zuno_acp::RequestId,
        params: &Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        let session = self.session(params).await?;
        let _guard = session
            .observer
            .try_lock()
            .map_err(|_| RpcError::session_busy("A session operation is still active"))?;
        let id = JobId::new(required(params, "jobId")?).map_err(|_| invalid_rpc())?;
        let job: JobView = self.api.get(&format!("jobs/{id}")).await.map_err(rpc)?;
        if job.id != id || job.session_id != session.id {
            return Err(invalid_rpc());
        }
        let prompt = Arc::new(Prompt {
            request: request.clone(),
            durable: self.durable(request, "observe"),
            cancelled: InterruptSignal::new(),
            job: Mutex::new(Some(job.clone())),
        });
        *session.current.lock().await = Some(prompt.clone());
        if client.is_request_cancelled() {
            prompt.cancelled.fire();
        }
        let result = self.poll_job(&session, &prompt, &job, client).await;
        let mut current = session.current.lock().await;
        if current.as_ref().is_some_and(|p| Arc::ptr_eq(p, &prompt)) {
            *current = None;
        }
        result
    }
    async fn observe(
        &self,
        session: &Session,
        prompt: &Arc<Prompt>,
        text: String,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        let api = self.api.clone();
        let id = session.id.clone();
        let task_prompt = prompt.clone();
        let permit = self
            .admissions
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::session_busy("ACP input admission capacity reached"))?;
        // Admission outlives the request observer. An explicit cancellation
        // arriving while POST is in flight is applied to the admitted Job.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _permit = permit;
            let result = admit(&api, &id, &task_prompt, text).await;
            if let Ok(job) = &result {
                *task_prompt.job.lock().await = Some(job.clone());
                if task_prompt.cancelled.is_set() {
                    let _ = cancel(&api, &task_prompt, job).await;
                }
            }
            let _ = sender.send(result);
        });
        let admitted = receiver
            .await
            .map_err(|_| RpcError::internal("Admission observer closed"))?
            .map_err(|error| {
                let uncertain = matches!(error, ApiError::Unconfirmed);
                let mut failure = rpc(error);
                let data = failure.data.get_or_insert_with(|| json!({"zuno":{}}));
                data["zuno"]["sessionId"] = json!(session.id);
                data["zuno"]["requestId"] = json!(prompt.durable);
                data["zuno"]["resolutionMethod"] = json!("_zuno/request");
                data["zuno"]["admission"] =
                    json!(if uncertain { "unconfirmed" } else { "rejected" });
                failure
            })?;
        client
            .notify(
                "_zuno/admission",
                json!({"sessionId":session.id,"requestId":prompt.durable,"job":admitted}),
            )
            .await?;
        self.poll_job(session, prompt, &admitted, client).await
    }
    async fn poll_job(
        &self,
        session: &Session,
        prompt: &Prompt,
        admitted: &JobView,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        let mut live_seen: Option<(String, Counter)> = None;
        loop {
            self.frames(session, &client).await?;
            let job: JobView = self
                .api
                .get(&format!("jobs/{}", admitted.id))
                .await
                .map_err(rpc)?;
            if job.id != admitted.id
                || job.session_id != session.id
                || job.turn_id != admitted.turn_id
            {
                return Err(invalid_rpc());
            }
            *prompt.job.lock().await = Some(job.clone());
            if prompt.cancelled.is_set() && !job.stop_requested {
                self.cancel(prompt).await?;
            }
            match job.phase {
                JobPhase::Completed
                | JobPhase::Cancelled
                | JobPhase::Failed
                | JobPhase::Paused
                | JobPhase::Uncertain => {
                    // Settlement and final public activity commit atomically.
                    self.frames(session, &client).await?;
                    return match job.phase {
                        JobPhase::Completed => Ok(
                            json!({"stopReason":"end_turn","_meta":{"zuno":{"jobId":job.id,"turnId":job.turn_id}}}),
                        ),
                        JobPhase::Cancelled => Ok(
                            json!({"stopReason":"cancelled","_meta":{"zuno":{"jobId":job.id,"pendingOperations":job.pending_operations}}}),
                        ),
                        _ => Err(RpcError::internal("Enterprise Job requires inspection")
                            .with_data(json!({"zuno":{"job":job}}))),
                    };
                }
                _ => {}
            }
            let live: Option<LiveFrame> = self
                .api
                .get(&format!("sessions/{}/live", session.id))
                .await
                .map_err(rpc)?;
            if let Some(live) = live {
                if live.version != ACTIVITY_PROTOCOL_VERSION || live.session_id != session.id {
                    return Err(invalid_rpc());
                }
                if live.after_committed <= *session.cursor.lock().await
                    && live_seen.as_ref() != Some(&(live.generation.clone(), live.sequence))
                {
                    client
                        .notify(
                            "_zuno/live",
                            serde_json::to_value(&live).map_err(|_| invalid_rpc())?,
                        )
                        .await?;
                    live_seen = Some((live.generation, live.sequence));
                }
            } else if live_seen.take().is_some() {
                client
                    .notify("_zuno/live", json!({"sessionId":session.id,"reset":true}))
                    .await?;
            }
            tokio::time::sleep(Duration::from_millis(self.options.poll_millis)).await;
        }
    }
    async fn frames(&self, session: &Session, client: &ClientConnection) -> Result<(), RpcError> {
        let mut cursor = session.cursor.lock().await;
        loop {
            let page: FramePage = self
                .api
                .get(&format!(
                    "sessions/{}/frames?after={}&limit=100",
                    session.id, cursor.0
                ))
                .await
                .map_err(rpc)?;
            if page.version != ACTIVITY_PROTOCOL_VERSION
                || page.session_id != session.id
                || page.through < *cursor
                || page.frames.last().map(|f| f.sequence).unwrap_or(*cursor) != page.through
                || page.more && page.frames.is_empty()
            {
                return Err(invalid_rpc());
            }
            for frame in &page.frames {
                if frame.session_id != session.id
                    || frame.version != ACTIVITY_PROTOCOL_VERSION
                    || frame.sequence.0 != cursor.0 + 1
                {
                    return Err(invalid_rpc());
                }
                activity::publish(client, &session.id, frame, false).await?;
                *cursor = frame.sequence;
            }
            if !page.more {
                return Ok(());
            }
        }
    }
    pub(super) async fn cancel(&self, prompt: &Prompt) -> Result<(), RpcError> {
        if let Some(job) = prompt.job.lock().await.clone() {
            cancel(&self.api, prompt, &job).await.map_err(rpc)?;
        }
        Ok(())
    }
}
fn prompt_text(params: &Value) -> Result<String, RpcError> {
    let blocks = params["prompt"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or_else(invalid_rpc)?;
    let mut text = String::new();
    for block in blocks {
        if block["type"] != "text" {
            return Err(RpcError::invalid_params(
                "This bridge accepts text prompts; attachments stay in the enterprise API",
            ));
        }
        let value = block["text"].as_str().ok_or_else(invalid_rpc)?;
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(value);
        if text.len() > zuno_application::MAX_INPUT_BYTES {
            return Err(invalid_rpc());
        }
    }
    if text.trim().is_empty() {
        return Err(invalid_rpc());
    }
    Ok(text)
}
async fn admit(
    api: &Api,
    session: &SessionId,
    prompt: &Prompt,
    text: String,
) -> Result<JobView, ApiError> {
    if prompt.cancelled.is_set() {
        return Err(ApiError::Conflict);
    }
    let version: InputVersionView = api
        .get(&format!("sessions/{session}/input-version"))
        .await?;
    if prompt.cancelled.is_set() {
        return Err(ApiError::Conflict);
    }
    let request = SubmitTurn {
        request_id: prompt.durable.clone(),
        expected_input_version: version.version,
        text,
    };
    let job: JobView = match api
        .post(&format!("sessions/{session}/turns"), &request)
        .await
    {
        Ok(job) => job,
        Err(ApiError::Unavailable | ApiError::Invalid) => api
            .get(&format!("sessions/{session}/requests/{}", prompt.durable))
            .await
            .map_err(|_| ApiError::Unconfirmed)?,
        Err(error) => return Err(error),
    };
    if job.session_id != *session {
        return Err(ApiError::Invalid);
    }
    Ok(job)
}
pub(super) async fn cancel(api: &Api, prompt: &Prompt, job: &JobView) -> Result<(), ApiError> {
    let request = RequestId::new(format!(
        "acp_cancel_{}",
        zuno_orchestration::sha256_text(prompt.durable.as_str())
    ))
    .expect("digest");
    let receipt: CancellationReceipt = api
        .post(
            &format!("jobs/{}/cancel", job.id),
            &CancelJob {
                request_id: request.clone(),
                expected_turn_id: job.turn_id.clone(),
                reason: "Cancelled by the ACP user".to_owned(),
            },
        )
        .await?;
    if receipt.request_id != request || receipt.job_id != job.id || receipt.turn_id != job.turn_id {
        return Err(ApiError::Invalid);
    }
    Ok(())
}
fn validate_history(page: &HistoryPage, session: &SessionId) -> Result<(), RpcError> {
    if page.version != ACTIVITY_PROTOCOL_VERSION
        || page.session_id != *session
        || page
            .items
            .iter()
            .any(|i| i.position > i.revision || i.revision > page.through)
        || page
            .items
            .windows(2)
            .any(|w| w[0].position >= w[1].position)
        || page
            .before
            .is_some_and(|before| page.items.first().is_none_or(|i| i.position != before))
    {
        return Err(invalid_rpc());
    }
    Ok(())
}
