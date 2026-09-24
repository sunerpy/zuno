use super::projection::replay_thread;
use super::*;
use crate::transport::SESSION_BUSY_CODE;
use crate::transport::STEER_REJECTED_CODE;

impl CodexAcpAgent {
    #[must_use]
    pub fn new(requests: AppServerRequestHandle) -> Self {
        Self {
            requests,
            state: Arc::new(BridgeState::default()),
        }
    }

    pub(super) async fn app_request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        static NEXT_REQUEST_ID: AtomicI64 = AtomicI64::new(1);
        let request = JSONRPCRequest {
            id: AppRequestId::Integer(NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)),
            method: method.to_owned(),
            params: Some(params),
            trace: None,
        };
        let request = ClientRequest::try_from(request).map_err(|error| {
            RpcError::internal(format!("invalid App Server request {method}: {error}"))
        })?;
        self.requests
            .request(request)
            .await
            .map_err(|error| RpcError::internal(format!("{method} transport failed: {error}")))?
            .map_err(|error| app_server_rpc_error(method, error))
    }

    pub(super) fn require_session(
        &self,
        params: &Value,
    ) -> Result<(String, SessionRoute), RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let route = lock(&self.state.sessions)
            .get(&session_id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params(format!("unknown session {session_id}")))?;
        Ok((session_id, route))
    }

    pub(super) fn bind_client(&self, session_id: &str, client: &ClientConnection) {
        if let Some(route) = lock(&self.state.sessions).get_mut(session_id) {
            route.client = client.session_scoped();
        }
    }

    pub(super) async fn new_session(
        &self,
        params: &Value,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        let cwd = required_string(params, "cwd")?;
        let meta = zuno_meta(params)?;
        let mut start = Map::new();
        start.insert("cwd".to_owned(), Value::String(cwd.clone()));
        start.insert("ephemeral".to_owned(), Value::Bool(false));
        start.insert("threadSource".to_owned(), Value::String("user".to_owned()));

        copy_optional_string(meta, "model", &mut start, "model")?;
        copy_optional_string(meta, "modelProvider", &mut start, "modelProvider")?;
        copy_optional_string(meta, "permissions", &mut start, "permissions")?;
        copy_optional_string(meta, "serviceTier", &mut start, "serviceTier")?;
        copy_optional_value(meta, "sandbox", &mut start, "sandbox");
        copy_optional_value(meta, "approvalPolicy", &mut start, "approvalPolicy");

        let mut thread_config = mcp_server_config(params.get("mcpServers"))?;
        if let Some(effort) = meta.get("effort").and_then(Value::as_str) {
            validate_effort(effort)?;
            thread_config.insert("model_reasoning_effort".to_owned(), json!(effort));
        }
        if !thread_config.is_empty() {
            start.insert("config".to_owned(), Value::Object(thread_config));
        }

        let response = self
            .app_request("thread/start", Value::Object(start))
            .await?;
        let route = route_from_lifecycle(&response, &cwd, client)?;
        let session_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::internal("thread/start response omitted thread.id"))?
            .to_owned();
        lock(&self.state.sessions).insert(session_id.clone(), route.clone());
        Ok(lifecycle_response(&session_id, &route))
    }

    pub(super) async fn resume_session(
        &self,
        params: &Value,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let cwd = required_string(params, "cwd")?;
        let response = self
            .app_request(
                "thread/resume",
                json!({ "threadId": session_id, "cwd": cwd }),
            )
            .await?;
        let route = route_from_lifecycle(&response, &cwd, client)?;
        lock(&self.state.sessions).insert(session_id.clone(), route.clone());
        replay_thread(&response, &session_id, client).await?;
        Ok(lifecycle_response(&session_id, &route))
    }

    pub(super) async fn fork_session(
        &self,
        params: &Value,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        let source_id = required_string(params, "sessionId")?;
        let cwd = required_string(params, "cwd")?;
        let response = self
            .app_request("thread/fork", json!({ "threadId": source_id, "cwd": cwd }))
            .await?;
        let session_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::internal("thread/fork response omitted thread.id"))?
            .to_owned();
        let route = route_from_lifecycle(&response, &cwd, client)?;
        lock(&self.state.sessions).insert(session_id.clone(), route.clone());
        replay_thread(&response, &session_id, client).await?;
        Ok(lifecycle_response(&session_id, &route))
    }

    pub(super) async fn list_sessions(&self, params: &Value) -> Result<Value, RpcError> {
        let mut request = json!({
            "limit": params.get("limit").and_then(Value::as_u64).unwrap_or(DEFAULT_LIST_LIMIT),
            "archived": false,
        });
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            request["cwd"] = Value::String(cwd.to_owned());
        }
        if let Some(cursor) = params.get("cursor").and_then(Value::as_str) {
            request["cursor"] = Value::String(cursor.to_owned());
        }
        let response = self.app_request("thread/list", request).await?;
        let sessions = response
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(session_list_entry)
            .collect::<Vec<_>>();
        Ok(json!({
            "sessions": sessions,
            "nextCursor": response.get("nextCursor").cloned().unwrap_or(Value::Null),
        }))
    }

    pub(super) async fn prompt(
        &self,
        params: &Value,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        let (session_id, route) = self.require_session(params)?;
        self.bind_client(&session_id, client);
        let input = acp_prompt_to_codex(params.get("prompt"))?;
        let client_user_message_id = message_id(params)?;
        if let Some(active_turn_id) = route.active_turn_id {
            let response = self
                .app_request(
                    "turn/steer",
                    json!({
                        "threadId": session_id,
                        "expectedTurnId": active_turn_id,
                        "input": input,
                        "clientUserMessageId": client_user_message_id,
                    }),
                )
                .await?;
            let turn_id = response
                .get("turnId")
                .and_then(Value::as_str)
                .unwrap_or(&active_turn_id);
            return Err(RpcError::downstream(
                SESSION_BUSY_CODE,
                "prompt was admitted as steering into the active turn",
                Some(json!({
                    "admission": "steered",
                    "sessionId": session_id,
                    "turnId": turn_id,
                    "messageId": client_user_message_id,
                })),
            ));
        }
        let response = self
            .app_request(
                "turn/start",
                json!({
                    "threadId": session_id,
                    "input": input,
                    "clientUserMessageId": client_user_message_id,
                }),
            )
            .await?;
        let turn_id = response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::internal("turn/start response omitted turn.id"))?
            .to_owned();
        {
            let mut sessions = lock(&self.state.sessions);
            if let Some(current) = sessions.get_mut(&session_id) {
                current.active_turn_id = Some(turn_id.clone());
                current.client = client.session_scoped();
            }
        }
        let outcome = self.state.wait_for_turn(turn_id).await;
        match outcome.status.as_str() {
            "completed" => Ok(prompt_response("end_turn", client_user_message_id)),
            "interrupted" => Ok(prompt_response("cancelled", client_user_message_id)),
            "failed" => Err(RpcError::internal(
                outcome
                    .error
                    .unwrap_or_else(|| "Codex turn failed".to_owned()),
            )),
            other => Err(RpcError::internal(format!(
                "Codex turn completed with unsupported status {other}"
            ))),
        }
    }

    pub(super) async fn steer_session(
        &self,
        params: &Value,
        client: &ClientConnection,
    ) -> Result<Value, RpcError> {
        let (session_id, route) = self.require_session(params)?;
        self.bind_client(&session_id, client);
        let active_turn_id = route.active_turn_id.ok_or_else(|| {
            RpcError::downstream(
                STEER_REJECTED_CODE,
                "session has no active turn to steer",
                Some(json!({ "sessionId": session_id, "reason": "noActiveTurn" })),
            )
        })?;
        let expected_turn_id = params
            .get("expectedTurnId")
            .and_then(Value::as_str)
            .unwrap_or(&active_turn_id);
        if expected_turn_id != active_turn_id {
            return Err(RpcError::downstream(
                STEER_REJECTED_CODE,
                "expectedTurnId does not match the active turn",
                Some(json!({
                    "sessionId": session_id,
                    "expectedTurnId": expected_turn_id,
                    "activeTurnId": active_turn_id,
                })),
            ));
        }
        let input = acp_prompt_to_codex(params.get("prompt"))?;
        let response = self
            .app_request(
                "turn/steer",
                json!({
                    "threadId": session_id,
                    "expectedTurnId": active_turn_id,
                    "input": input,
                    "clientUserMessageId": message_id(params)?,
                }),
            )
            .await?;
        Ok(json!({
            "turnId": response.get("turnId").cloned().unwrap_or(Value::String(active_turn_id)),
        }))
    }

    pub(super) async fn cancel_session(&self, params: &Value) -> Result<(), RpcError> {
        let (session_id, route) = self.require_session(params)?;
        let Some(turn_id) = route.active_turn_id else {
            return Ok(());
        };
        let _response = self
            .app_request(
                "turn/interrupt",
                json!({ "threadId": session_id, "turnId": turn_id }),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn set_option(&self, params: &Value) -> Result<Value, RpcError> {
        let (session_id, route) = self.require_session(params)?;
        let config_id = required_string(params, "configId")?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| RpcError::invalid_params("value must be a non-empty string"))?;
        let mut update = json!({ "threadId": session_id });
        match config_id.as_str() {
            "model" => update["model"] = Value::String(value.to_owned()),
            "reasoning_effort" => {
                validate_effort(value)?;
                update["effort"] = Value::String(value.to_owned());
            }
            "permissions" => update["permissions"] = Value::String(value.to_owned()),
            "mode" => {
                let mode = match value {
                    "build" => "default",
                    "plan" => "plan",
                    _ => return Err(RpcError::invalid_params("mode must be build or plan")),
                };
                update["collaborationMode"] = json!({
                    "mode": mode,
                    "settings": {
                        "model": route.model,
                        "reasoning_effort": route.effort,
                        "developer_instructions": null,
                    },
                });
            }
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "unknown config option {config_id}"
                )));
            }
        }
        let _response = self.app_request("thread/settings/update", update).await?;
        let mut sessions = lock(&self.state.sessions);
        let route = sessions
            .get_mut(&session_id)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown session {session_id}")))?;
        match config_id.as_str() {
            "model" => route.model = value.to_owned(),
            "reasoning_effort" => route.effort = Some(value.to_owned()),
            "mode" => route.mode = value.to_owned(),
            "permissions" => {}
            _ => unreachable!(),
        }
        Ok(json!({ "configOptions": config_options(route) }))
    }

    pub(super) async fn set_mode(&self, params: &Value) -> Result<Value, RpcError> {
        let translated = set_mode_as_config_option(params)?;
        let _config_options = self.set_option(&translated).await?;
        Ok(json!({}))
    }

    pub(super) async fn set_model(&self, params: &Value) -> Result<Value, RpcError> {
        let mut translated = params.clone();
        translated["configId"] = Value::String("model".to_owned());
        translated["value"] = Value::String(required_string(params, "modelId")?);
        self.set_option(&translated).await
    }
}
