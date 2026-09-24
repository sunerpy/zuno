use super::*;

mod history;

#[cfg(test)]
pub(super) use history::history_updates;
pub(super) use history::notification_updates;
pub(super) use history::replay_thread;

impl BridgeState {
    pub(super) async fn wait_for_turn(&self, turn_id: String) -> TurnOutcome {
        let receiver = {
            let mut turns = lock(&self.turns);
            if let Some(outcome) = turns.completed.remove(&turn_id) {
                return outcome;
            }
            let (sender, receiver) = oneshot::channel();
            turns.waiters.insert(turn_id, sender);
            receiver
        };
        receiver.await.unwrap_or(TurnOutcome {
            status: "failed".to_owned(),
            error: Some("Codex App Server closed before the turn settled".to_owned()),
        })
    }

    fn settle_turn(&self, turn_id: String, outcome: TurnOutcome) {
        let mut turns = lock(&self.turns);
        if let Some(waiter) = turns.waiters.remove(&turn_id) {
            let _ = waiter.send(outcome);
        } else {
            turns.completed.insert(turn_id, outcome);
        }
    }

    /// Resolve when the next turn of `thread_id` settles (commands such as
    /// `/compact` start a turn whose id the App Server does not return).
    pub(super) fn wait_for_thread_turn(
        &self,
        thread_id: &str,
    ) -> impl std::future::Future<Output = TurnOutcome> + Send + 'static {
        let (sender, receiver) = oneshot::channel();
        lock(&self.turns)
            .thread_waiters
            .insert(thread_id.to_owned(), sender);
        async move {
            receiver.await.unwrap_or(TurnOutcome {
                status: "failed".to_owned(),
                error: Some("Codex App Server closed before the turn settled".to_owned()),
            })
        }
    }

    fn settle_thread_turn(&self, thread_id: &str, outcome: TurnOutcome) {
        if let Some(waiter) = lock(&self.turns).thread_waiters.remove(thread_id) {
            let _ = waiter.send(outcome);
        }
    }

    fn route(&self, thread_id: &str) -> Option<SessionRoute> {
        lock(&self.sessions).get(thread_id).cloned()
    }

    fn record_turn_started(&self, thread_id: &str, turn_id: &str) {
        if let Some(route) = lock(&self.sessions).get_mut(thread_id) {
            route.active_turn_id = Some(turn_id.to_owned());
        }
    }

    fn record_turn_settled(&self, thread_id: &str, turn_id: &str) {
        if let Some(route) = lock(&self.sessions).get_mut(thread_id)
            && route.active_turn_id.as_deref() == Some(turn_id)
        {
            route.active_turn_id = None;
        }
    }

    fn apply_settings(&self, params: &Value) {
        let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
            return;
        };
        let Some(settings) = params.get("threadSettings") else {
            return;
        };
        let mut sessions = lock(&self.sessions);
        let Some(route) = sessions.get_mut(thread_id) else {
            return;
        };
        if let Some(model) = settings.get("model").and_then(Value::as_str) {
            route.model = model.to_owned();
        }
        if let Some(provider) = settings.get("modelProvider").and_then(Value::as_str) {
            route.model_provider = provider.to_owned();
        }
        route.effort = settings
            .get("effort")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(mode) = settings
            .pointer("/collaborationMode/mode")
            .and_then(Value::as_str)
        {
            route.collaboration_mode = mode.to_owned();
        }
        if let Some(policy) = settings.get("approvalPolicy") {
            route.approval_policy = approval_policy_id(Some(policy));
        }
        if let Some(reviewer) = settings.get("approvalsReviewer").and_then(Value::as_str) {
            route.approvals_reviewer = reviewer.to_owned();
        }
        if let Some(sandbox) = settings
            .pointer("/sandboxPolicy/type")
            .and_then(Value::as_str)
        {
            route.sandbox_type = sandbox.to_owned();
        }
    }
}

/// The answer the ACP client gave to a bridged App Server request, ready to be
/// forwarded with [`settle_server_request`].
pub(super) type BridgedServerRequest = (AppRequestId, Result<Value, RpcError>);

pub(super) async fn handle_app_server_event(
    state: &Arc<BridgeState>,
    event: AppServerEvent,
    bridged: &mut JoinSet<BridgedServerRequest>,
) -> Result<(), AcpBridgeError> {
    match event {
        AppServerEvent::ServerNotification(notification) => {
            let value = serde_json::to_value(notification.as_ref())?;
            project_notification(state, &value).await;
        }
        AppServerEvent::ServerRequest(request) => {
            let request_id = request.id().clone();
            let value = serde_json::to_value(request.as_ref())?;
            let state = Arc::clone(state);
            // Bridging an approval or a tool question waits for the ACP client's
            // answer, which the transport read loop delivers; that loop is polled
            // by the same event loop that called us, so the wait must not happen
            // inline or the answer is never read and the turn hangs.
            bridged.spawn(async move {
                let result = bridge_server_request(&state, &value).await;
                (request_id, result)
            });
        }
        AppServerEvent::Lagged { skipped } => {
            tracing::warn!(skipped, "ACP App Server event stream lagged");
        }
        AppServerEvent::Disconnected { message } => {
            tracing::warn!(%message, "ACP App Server disconnected");
            return Err(AcpBridgeError::AppServerClosed);
        }
    }
    Ok(())
}

/// Forward a bridged answer to the App Server request that asked for it.
pub(super) async fn settle_server_request(
    app_server: &AppServerClient,
    (request_id, result): BridgedServerRequest,
) -> Result<(), AcpBridgeError> {
    match result {
        Ok(result) => {
            app_server
                .resolve_server_request(request_id, result)
                .await?
        }
        Err(error) => {
            app_server
                .reject_server_request(
                    request_id,
                    JSONRPCErrorError {
                        code: error.code,
                        message: error.message,
                        data: error.data,
                    },
                )
                .await?;
        }
    }
    Ok(())
}

async fn project_notification(state: &BridgeState, notification: &Value) {
    let Some(method) = notification.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = notification.get("params").unwrap_or(&Value::Null);
    let thread_id = params.get("threadId").and_then(Value::as_str);
    let turn_id = params
        .get("turnId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/turn/id").and_then(Value::as_str));

    match method {
        "turn/started" => {
            if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
                state.record_turn_started(thread_id, turn_id);
            }
        }
        "turn/completed" => {
            let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
                return;
            };
            let outcome = TurnOutcome {
                status: params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_owned(),
                error: params
                    .pointer("/turn/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            state.record_turn_settled(thread_id, turn_id);
            state.settle_thread_turn(thread_id, outcome.clone());
            state.settle_turn(turn_id.to_owned(), outcome);
        }
        "thread/settings/updated" => state.apply_settings(params),
        // A turn the App Server accepted but core refused before it started (for
        // example `review/start` in a directory that is not a git repository)
        // produces only this notification: settle the waiters so `session/prompt`
        // fails instead of hanging.
        "error" if params.get("willRetry").and_then(Value::as_bool) != Some(true) => {
            if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
                let outcome = TurnOutcome {
                    status: "failed".to_owned(),
                    error: params
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                };
                state.record_turn_settled(thread_id, turn_id);
                state.settle_thread_turn(thread_id, outcome.clone());
                state.settle_turn(turn_id.to_owned(), outcome);
            }
        }
        _ => {}
    }

    let Some(thread_id) = thread_id else {
        return;
    };
    let Some(route) = state.route(thread_id) else {
        return;
    };
    for update in notification_updates(method, params) {
        if let Err(error) = route.client.session_update(thread_id, update).await {
            tracing::warn!(%error, thread_id, method, "failed to project App Server event to ACP");
            break;
        }
    }
}

async fn bridge_server_request(state: &BridgeState, request: &Value) -> Result<Value, RpcError> {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::internal("App Server request omitted method"))?;
    let params = request.get("params").unwrap_or(&Value::Null);
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::internal("App Server request omitted threadId"))?;
    let route = state.route(thread_id).ok_or_else(|| {
        RpcError::internal(format!("no ACP client is bound to thread {thread_id}"))
    })?;
    match method {
        "item/commandExecution/requestApproval" => {
            let item_id = params
                .get("itemId")
                .and_then(Value::as_str)
                .unwrap_or("command");
            let title = params
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| params.get("reason").and_then(Value::as_str))
                .unwrap_or("Run command");
            let selected = request_permission(
                &route.client,
                thread_id,
                item_id,
                title,
                "execute",
                params.clone(),
            )
            .await?;
            let decision = match selected.as_str() {
                "allow_once" => "accept",
                "allow_always" => "acceptForSession",
                "cancel" => "cancel",
                _ => "decline",
            };
            Ok(json!({ "decision": decision }))
        }
        "item/fileChange/requestApproval" => {
            let item_id = params
                .get("itemId")
                .and_then(Value::as_str)
                .unwrap_or("file-change");
            let title = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Apply file changes");
            let selected = request_permission(
                &route.client,
                thread_id,
                item_id,
                title,
                "edit",
                params.clone(),
            )
            .await?;
            let decision = match selected.as_str() {
                "allow_once" => "accept",
                "allow_always" => "acceptForSession",
                "cancel" => "cancel",
                _ => "decline",
            };
            Ok(json!({ "decision": decision }))
        }
        "item/permissions/requestApproval" => {
            let item_id = params
                .get("itemId")
                .and_then(Value::as_str)
                .unwrap_or("permissions");
            let title = params
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("Grant additional permissions");
            let selected = request_permission(
                &route.client,
                thread_id,
                item_id,
                title,
                "other",
                params.clone(),
            )
            .await?;
            let granted = matches!(selected.as_str(), "allow_once" | "allow_always");
            Ok(json!({
                "permissions": if granted {
                    params.get("permissions").cloned().unwrap_or_else(|| json!({}))
                } else {
                    json!({})
                },
                "scope": if selected == "allow_always" { "session" } else { "turn" },
                "strictAutoReview": false,
            }))
        }
        "item/tool/requestUserInput" => {
            if state.client_supports_form_elicitation() {
                bridge_tool_user_input_form(&route.client, thread_id, params).await
            } else {
                bridge_tool_user_input(&route.client, thread_id, params).await
            }
        }
        _ => Err(RpcError::downstream(
            -32601,
            format!("ACP cannot satisfy App Server request {method}"),
            None,
        )),
    }
}

/// Collect tool questions through one ACP `elicitation/create` form (ACP 1.7):
/// multiple-choice questions become `enum` properties and free-text questions
/// become `string` properties, so clients that advertise
/// `clientCapabilities.elicitation.form` can answer both.
async fn bridge_tool_user_input_form(
    client: &ClientConnection,
    session_id: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let request = elicitation_form_request(session_id, params)?;
    let response = client.request("elicitation/create", request).await?;
    answers_from_elicitation(params, &response)
}

pub(super) fn elicitation_form_request(
    session_id: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::internal("tool input request omitted questions"))?;
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut message = Vec::new();
    for question in questions {
        let id = required_string(question, "id")?;
        if question.get("isSecret").and_then(Value::as_bool) == Some(true) {
            // "Form mode MUST NOT be used to request secrets or credentials."
            return Err(RpcError::invalid_request(
                "ACP form elicitation cannot collect secret tool input",
            ));
        }
        let header = question
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or(id.as_str())
            .to_owned();
        let prompt = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut property = json!({ "type": "string", "title": header, "description": prompt });
        if let Some(options) = question
            .get("options")
            .and_then(Value::as_array)
            .filter(|options| !options.is_empty())
        {
            // The App Server tool always allows a free-form "Other" answer
            // (`isOther`), which a closed `enum` would silently drop. Form
            // schemas are limited to primitives and enums, so an open question
            // stays a `string` and lists its suggestions in the description.
            let labels = options
                .iter()
                .filter_map(|option| option.get("label").and_then(Value::as_str))
                .collect::<Vec<_>>();
            if question.get("isOther").and_then(Value::as_bool) == Some(true) {
                let suggestions = options
                    .iter()
                    .filter_map(|option| {
                        let label = option.get("label").and_then(Value::as_str)?;
                        match option.get("description").and_then(Value::as_str) {
                            Some(description) if !description.is_empty() => {
                                Some(format!("{label} ({description})"))
                            }
                            _ => Some(label.to_owned()),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let description = if prompt.is_empty() {
                    format!("Options: {suggestions}. Or type another answer.")
                } else {
                    format!("{prompt}\nOptions: {suggestions}. Or type another answer.")
                };
                property["description"] = Value::String(description);
            } else {
                property["enum"] = json!(labels);
            }
        }
        properties.insert(id.clone(), property);
        required.push(Value::String(id));
        if !prompt.is_empty() {
            message.push(format!("{header}: {prompt}"));
        }
    }
    if properties.is_empty() {
        return Err(RpcError::internal(
            "tool input request contained no questions",
        ));
    }
    let mut request = json!({
        "sessionId": session_id,
        "mode": "form",
        "message": if message.is_empty() {
            "The agent needs more information to continue.".to_owned()
        } else {
            message.join("\n")
        },
        "requestedSchema": {
            "type": "object",
            "properties": Value::Object(properties),
            "required": required,
        },
    });
    if let Some(item_id) = params.get("itemId").filter(|id| !id.is_null()) {
        request["toolCallId"] = item_id.clone();
    }
    Ok(request)
}

pub(super) fn answers_from_elicitation(
    params: &Value,
    response: &Value,
) -> Result<Value, RpcError> {
    match response.get("action").and_then(Value::as_str) {
        Some("accept") => {}
        Some("decline") => return Err(RpcError::cancelled("tool input was declined")),
        Some("cancel") | None => return Err(RpcError::cancelled("tool input was cancelled")),
        Some(other) => {
            return Err(RpcError::internal(format!(
                "unsupported elicitation action {other}"
            )));
        }
    }
    let content = response
        .get("content")
        .and_then(Value::as_object)
        .ok_or_else(|| RpcError::cancelled("tool input was accepted without answers"))?;
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::internal("tool input request omitted questions"))?;
    let mut answers = Map::new();
    for question in questions {
        let id = required_string(question, "id")?;
        let answer = match content.get(&id) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            Some(Value::Bool(flag)) => flag.to_string(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            _ => {
                return Err(RpcError::cancelled(format!(
                    "tool input question {id} was left unanswered"
                )));
            }
        };
        answers.insert(id, json!({ "answers": [answer] }));
    }
    Ok(json!({ "answers": answers }))
}

/// Fallback for clients without form elicitation: multiple choice through
/// `session/request_permission`; free-text questions cannot be bridged.
async fn bridge_tool_user_input(
    client: &ClientConnection,
    session_id: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let questions = params
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::internal("tool input request omitted questions"))?;
    let mut answers = Map::new();
    for question in questions {
        let id = required_string(question, "id")?;
        if question.get("isSecret").and_then(Value::as_bool) == Some(true) {
            return Err(RpcError::invalid_request(
                "ACP v1 cannot safely collect secret free-form tool input",
            ));
        }
        let options = question
            .get("options")
            .and_then(Value::as_array)
            .filter(|options| !options.is_empty())
            .ok_or_else(|| {
                RpcError::invalid_request("ACP v1 can bridge only multiple-choice tool input")
            })?;
        let acp_options = options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                json!({
                    "optionId": format!("answer-{index}"),
                    "name": option.get("label").and_then(Value::as_str).unwrap_or("Option"),
                    "kind": "allow_once",
                })
            })
            .chain(std::iter::once(json!({
                "optionId": "cancel",
                "name": "Cancel",
                "kind": "reject_once",
            })))
            .collect::<Vec<_>>();
        let response = client
            .request_permission(json!({
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": params.get("itemId").cloned().unwrap_or_else(|| json!("question")),
                    "title": question.get("header").cloned().unwrap_or_else(|| json!("Question")),
                    "kind": "other",
                    "status": "pending",
                    "rawInput": question,
                },
                "options": acp_options,
            }))
            .await?;
        let selected = response
            .pointer("/outcome/optionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::cancelled("tool input was not answered"))?;
        let index = selected
            .strip_prefix("answer-")
            .and_then(|index| index.parse::<usize>().ok())
            .ok_or_else(|| RpcError::cancelled("tool input was cancelled"))?;
        let label = options
            .get(index)
            .and_then(|option| option.get("label"))
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::internal("selected tool input option disappeared"))?;
        answers.insert(id, json!({ "answers": [label] }));
    }
    Ok(json!({ "answers": answers }))
}

async fn request_permission(
    client: &ClientConnection,
    session_id: &str,
    tool_call_id: &str,
    title: &str,
    kind: &str,
    raw_input: Value,
) -> Result<String, RpcError> {
    let response = client
        .request_permission(json!({
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": tool_call_id,
                "title": title,
                "kind": kind,
                "status": "pending",
                "rawInput": raw_input,
            },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always" },
                { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
                { "optionId": "cancel", "name": "Reject and stop", "kind": "reject_once" },
            ],
        }))
        .await?;
    if response.pointer("/outcome/outcome").and_then(Value::as_str) != Some("selected") {
        return Ok("reject_once".to_owned());
    }
    Ok(response
        .pointer("/outcome/optionId")
        .and_then(Value::as_str)
        .unwrap_or("reject_once")
        .to_owned())
}
