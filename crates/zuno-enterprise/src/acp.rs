//! Remote ACP bridge. The authenticated public API owns work, approvals and
//! execution; disconnect only retires the local observer.
mod activity;
mod client;
mod session;

use crate::{Error, config::AcpBridgeConfig, invalid};
use async_trait::async_trait;
use client::{Api, ApiError};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use zuno_acp::{Agent, ClientConnection, RpcError};
use zuno_application::api::{InputVersionView, JobView, SubmitTurn};
use zuno_application::{SessionPage, SessionSummary};
use zuno_engine::interrupt::InterruptSignal;
use zuno_types::activity::{ACTIVITY_PROTOCOL_VERSION, Counter, FramePage};
use zuno_types::identity::{JobId, RequestId, SessionId};

fn invalid_rpc() -> RpcError {
    RpcError::invalid_params("Invalid enterprise ACP request")
}
fn rpc(error: ApiError) -> RpcError {
    RpcError::internal(error.to_string()).with_data(json!({"zuno":{"reason":match error{
        ApiError::Forbidden=>"authorization",ApiError::NotFound=>"not_found",
        ApiError::Conflict=>"conflict",ApiError::Capacity=>"quota_exceeded",ApiError::Invalid=>"invalid_response",ApiError::Unavailable=>"unavailable",ApiError::Unconfirmed=>"unconfirmed",
    }}}))
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid_rpc)
}

struct Session {
    id: SessionId,
    cursor: Mutex<Counter>,
    current: Mutex<Option<Arc<Prompt>>>,
    observer: Mutex<()>,
}
struct Prompt {
    request: zuno_acp::RequestId,
    durable: RequestId,
    cancelled: InterruptSignal,
    job: Mutex<Option<JobView>>,
}
struct Bridge {
    api: Api,
    options: AcpBridgeConfig,
    connection_id: String,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    admissions: Arc<tokio::sync::Semaphore>,
}
pub async fn run(options: AcpBridgeConfig, shutdown: InterruptSignal) -> Result<(), Error> {
    if !(1..=128).contains(&options.max_sessions)
        || !(100..=5000).contains(&options.poll_millis)
        || !options.local_directory.is_absolute()
    {
        return Err(invalid(
            "ACP bridge requires a logical absolute cwd, 1-128 sessions and 100-5000 ms polling",
        ));
    }
    let api = Api::new(&options.api).await?;
    let workspaces: Vec<zuno_application::api::WorkspaceView> = api
        .get("workspaces")
        .await
        .map_err(|_| invalid("ACP workspace catalog unavailable"))?;
    if !workspaces.iter().any(|w| w.id == options.workspace_id) {
        return Err(invalid("ACP workspace is not available to this user"));
    }
    let bridge = Bridge {
        api,
        admissions: Arc::new(tokio::sync::Semaphore::new(options.max_sessions as usize)),
        options,
        connection_id: uuid::Uuid::new_v4().to_string(),
        sessions: Mutex::new(HashMap::new()),
    };
    tokio::select! {
        result=zuno_acp::serve_stdio(bridge)=>result.map_err(|_|invalid("ACP transport stopped")),
        _=shutdown.notified()=>Ok(()),
    }
}
#[async_trait]
impl Agent for Bridge {
    async fn request(
        &self,
        method: &str,
        request: &zuno_acp::RequestId,
        params: Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        match method {
            "initialize" => {
                if params["protocolVersion"].as_u64().is_none() {
                    return Err(invalid_rpc());
                }
                Ok(
                    json!({"protocolVersion":1,"agentInfo":{"name":"zuno-enterprise","version":env!("CARGO_PKG_VERSION")},
                    "agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},
                    "mcpCapabilities":{"http":false,"sse":false},"sessionCapabilities":{"list":{},"resume":{},"close":{}}},
                    "authMethods":[],"_meta":{"zuno":{"enterprise":true,"actor":self.api.actor(),
                    "activity":{"version":1,"method":"_zuno/activity"},"live":{"version":1,"method":"_zuno/live"},
                    "approvalAuthority":"enterprise_api","executionLocation":"enterprise"}}}),
                )
            }
            "session/new" => self.new_session(request, &params).await,
            "session/load" | "session/resume" => {
                self.load(&params, client, method == "session/load").await
            }
            "session/list" => self.list(&params).await,
            "_zuno/history" => self.history(&params).await,
            "_zuno/job" => self.job(&params).await,
            "_zuno/request" => self.resolve_request(&params).await,
            "_zuno/observe" => self.observe_existing(request, &params, client).await,
            "session/prompt" => self.prompt(request, &params, client).await,
            "session/close" => {
                let session = self.session(&params).await?;
                let _guard = session
                    .observer
                    .try_lock()
                    .map_err(|_| RpcError::session_busy("A session operation is still active"))?;
                if session.current.lock().await.is_some() {
                    return Err(RpcError::session_busy("A prompt observer is still active"));
                }
                self.sessions.lock().await.remove(&session.id);
                Ok(json!({}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
    async fn notification(
        &self,
        method: &str,
        params: Value,
        _client: ClientConnection,
    ) -> Result<(), RpcError> {
        if method != "session/cancel" {
            return Err(RpcError::method_not_found(method));
        }
        let session = self.session(&params).await?;
        if let Some(prompt) = session.current.lock().await.clone() {
            prompt.cancelled.fire();
            self.cancel(&prompt).await?;
        }
        Ok(())
    }
    async fn request_cancelled(&self, method: &str, request: &zuno_acp::RequestId, params: &Value) {
        if !matches!(method, "session/prompt" | "_zuno/observe") {
            return;
        }
        if let Ok(session) = self.session(params).await {
            let active = session
                .current
                .lock()
                .await
                .clone()
                .filter(|p| p.request == *request);
            if let Some(prompt) = active {
                prompt.cancelled.fire();
                let api = self.api.clone();
                let pending = prompt.clone();
                tokio::spawn(async move {
                    let job = pending.job.lock().await.clone();
                    if let Some(job) = job {
                        let _ = session::cancel(&api, &pending, &job).await;
                    }
                });
                let mut current = session.current.lock().await;
                if current.as_ref().is_some_and(|p| Arc::ptr_eq(p, &prompt)) {
                    *current = None;
                }
            }
        }
    }
    async fn request_disconnected(
        &self,
        method: &str,
        request: &zuno_acp::RequestId,
        params: &Value,
    ) {
        if !matches!(method, "session/prompt" | "_zuno/observe") {
            return;
        }
        if let Ok(session) = self.session(params).await {
            let mut current = session.current.lock().await;
            if current.as_ref().is_some_and(|p| p.request == *request) {
                *current = None;
            }
        }
    }
}

impl Bridge {
    fn durable(&self, request: &zuno_acp::RequestId, purpose: &str) -> RequestId {
        RequestId::new(format!(
            "acp_{}",
            zuno_orchestration::sha256_json(&json!([
                self.connection_id,
                request.canonical_key(),
                purpose
            ]))
        ))
        .expect("digest ID")
    }
    fn cwd(&self, params: &Value) -> Result<(), RpcError> {
        if params
            .get("cwd")
            .is_some_and(|cwd| cwd.as_str() != self.options.local_directory.to_str())
            || params
                .get("mcpServers")
                .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
        {
            return Err(RpcError::invalid_params(
                "This bridge uses its configured logical cwd and remote tool declarations",
            ));
        }
        Ok(())
    }
    async fn session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let id = SessionId::new(required(params, "sessionId")?).map_err(|_| invalid_rpc())?;
        self.sessions
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("Session is not open on this connection"))
    }
    async fn attach(
        &self,
        summary: SessionSummary,
        cursor: Counter,
    ) -> Result<Arc<Session>, RpcError> {
        if summary.workspace_id.as_ref() != Some(&self.options.workspace_id) {
            return Err(RpcError::invalid_params(
                "Session belongs to a different workspace",
            ));
        }
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(&summary.id) {
            return Ok(session.clone());
        }
        if sessions.len() >= self.options.max_sessions as usize {
            return Err(RpcError::session_busy("ACP session capacity reached"));
        }
        let session = Arc::new(Session {
            id: summary.id,
            cursor: Mutex::new(cursor),
            current: Mutex::new(None),
            observer: Mutex::new(()),
        });
        sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }
    async fn new_session(
        &self,
        request: &zuno_acp::RequestId,
        params: &Value,
    ) -> Result<Value, RpcError> {
        self.cwd(params)?;
        if self.sessions.lock().await.len() >= self.options.max_sessions as usize {
            return Err(RpcError::session_busy("ACP session capacity reached"));
        }
        let summary: SessionSummary = self
            .api
            .post(
                "sessions",
                &zuno_application::CreateSession {
                    request_id: self.durable(request, "new"),
                    workspace_id: self.options.workspace_id.clone(),
                    title: "ACP session".to_owned(),
                },
            )
            .await
            .map_err(rpc)?;
        let session = self.attach(summary, Counter(0)).await?;
        Ok(
            json!({"sessionId":session.id,"_meta":{"zuno":{"workspaceId":self.options.workspace_id}}}),
        )
    }
    async fn list(&self, params: &Value) -> Result<Value, RpcError> {
        self.cwd(params)?;
        let mut path = "sessions?limit=100".to_owned();
        if let Some(cursor) = params.get("cursor") {
            let cursor: zuno_application::SessionCursor =
                serde_json::from_str(cursor.as_str().ok_or_else(invalid_rpc)?)
                    .map_err(|_| invalid_rpc())?;
            path.push_str(&format!(
                "&beforeUpdatedAt={}&beforeSessionId={}",
                cursor.updated_at, cursor.session_id
            ));
        }
        let page: SessionPage = self.api.get(&path).await.map_err(rpc)?;
        let sessions = page
            .items
            .into_iter()
            .filter(|s| s.workspace_id.as_ref() == Some(&self.options.workspace_id))
            .map(|s| {
                json!({"sessionId":s.id,"cwd":self.options.local_directory,"title":s.title,
                "_meta":{"zuno":{"updatedAt":s.updated_at}}})
            })
            .collect::<Vec<_>>();
        Ok(
            json!({"sessions":sessions,"nextCursor":page.next.map(|c|serde_json::to_string(&c).expect("cursor"))}),
        )
    }
}
