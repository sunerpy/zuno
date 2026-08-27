use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex;
use zuno_engine::r#loop::{TurnEvent, event_channel};
use zuno_engine::status::{SessionControl, SessionRunRegistry};
use zuno_llm::event::{FinishReason, RequestContentBlock};
use zuno_tool::PermissionAsker;

use super::mcp_runtime::McpRuntime;
use super::turn::{
    CatalogModelChoice, ExtensionComposition, SessionChoice, TurnHost, TurnOptions, TurnPlan,
    persisted_session_agent,
};

use crate::command::AcpArgs;
use crate::environment::StartupEnvironment;

const ACP_PROTOCOL_VERSION: u64 = 1;
const ACP_SCHEMA_VERSION: &str = "1.21.0";

pub(super) fn execute(args: &AcpArgs, environment: &StartupEnvironment) -> Result<(), String> {
    if args.check {
        println!(
            "ACP stdio adapter ready (protocol v{ACP_PROTOCOL_VERSION}; schema v{ACP_SCHEMA_VERSION})"
        );
        return Ok(());
    }

    let agent = ProductionAcpAgent::new(environment.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let transport = runtime
        .block_on(zuno_acp::serve_stdio(agent.clone()))
        .map_err(|error| error.to_string());
    let shutdown = runtime.block_on(agent.shutdown());
    environment.cancel_background_jobs();
    runtime.block_on(environment.wait_background_jobs());
    match (transport, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(shutdown)) => Err(format!(
            "{error}; ACP session shutdown also failed: {shutdown}"
        )),
    }
}

#[derive(Clone)]
struct ProductionAcpAgent {
    state: Arc<AcpState>,
}

struct AcpState {
    environment: StartupEnvironment,
    runs: SessionRunRegistry,
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    composition_gate: Mutex<()>,
    elicitation_form: AtomicBool,
}

#[async_trait]
impl zuno_acp::Agent for ProductionAcpAgent {
    async fn request(
        &self,
        method: &str,
        params: Value,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        match method {
            "initialize" => self.initialize(&params),
            "session/new" => self.new_session(&params, client).await,
            "session/load" => self.open_existing(&params, client, true).await,
            "session/set_mode" => self.set_mode(&params, client).await,
            "session/set_config_option" => self.set_config_option(&params, client).await,
            "session/prompt" => self.prompt(&params, client).await,
            "session/resume" => self.open_existing(&params, client, false).await,
            "session/list" => self.list_sessions(&params),
            "session/close" => self.close_session(&params).await,
            "session/delete" => self.delete_session(&params).await,
            _ => Err(zuno_acp::RpcError::method_not_found(method)),
        }
    }

    async fn notification(
        &self,
        method: &str,
        params: Value,
        _client: zuno_acp::ClientConnection,
    ) -> Result<(), zuno_acp::RpcError> {
        if method != "session/cancel" {
            return Err(zuno_acp::RpcError::method_not_found(method));
        }
        let session_id = required_string(&params, "sessionId")?;
        let session = self.session(&session_id).await?;
        session.cancel();
        Ok(())
    }
}

fn initialize(params: &Value) -> Result<Value, zuno_acp::RpcError> {
    if params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err(zuno_acp::RpcError::invalid_params(
            "protocolVersion must be a number",
        ));
    }
    Ok(json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": false,
            },
            "mcpCapabilities": {
                "http": false,
                "sse": false,
            },
            "sessionCapabilities": {
                "list": {},
                "delete": {},
                "resume": {},
                "close": {},
            },
            "auth": {},
        },
        "authMethods": [],
        "agentInfo": {
            "name": "zuno",
            "title": "Zuno",
            "version": env!("CARGO_PKG_VERSION"),
        },
    }))
}

impl ProductionAcpAgent {
    fn initialize(&self, params: &Value) -> Result<Value, zuno_acp::RpcError> {
        let response = initialize(params)?;
        self.state.elicitation_form.store(
            params
                .pointer("/clientCapabilities/elicitation/form")
                .is_some_and(Value::is_object),
            Ordering::Release,
        );
        Ok(response)
    }

    fn new(environment: StartupEnvironment) -> Self {
        Self {
            state: Arc::new(AcpState {
                environment,
                runs: SessionRunRegistry::new(),
                sessions: Mutex::new(HashMap::new()),
                composition_gate: Mutex::new(()),
                elicitation_form: AtomicBool::new(false),
            }),
        }
    }

    async fn new_session(
        &self,
        params: &Value,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let cwd = lifecycle_directory(params)?;
        require_empty_roots_and_client_mcp(params)?;
        let options = TurnOptions {
            directory: Some(cwd),
            model: None,
            agent: None,
            preset: None,
            session: SessionChoice::New,
            title: None,
            effort: None,
            tool_authority: None,
            extension_composition: ExtensionComposition::Active,
        };
        let session = self.open_session(options, client).await?;
        let session_id = session.id.clone();
        let response = session.lifecycle_response().await?;
        let previous = self
            .state
            .sessions
            .lock()
            .await
            .insert(session_id.clone(), session.clone());
        if previous.is_some() {
            session
                .shutdown()
                .await
                .map_err(zuno_acp::RpcError::internal)?;
            return Err(zuno_acp::RpcError::internal(
                "generated duplicate ACP session id",
            ));
        }
        Ok(with_session_id(response, &session_id))
    }

    async fn open_existing(
        &self,
        params: &Value,
        client: zuno_acp::ClientConnection,
        replay: bool,
    ) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let cwd = lifecycle_directory(params)?;
        require_empty_roots_and_client_mcp(params)?;
        let pool = durable_pool()?;
        let stored = zuno_db::session::Store::new(&pool)
            .get(&session_id)
            .map_err(|error| map_session_lookup(&session_id, error))?;
        if stored.directory != cwd.to_string_lossy().as_ref() {
            return Err(zuno_acp::RpcError::invalid_params(format!(
                "session {session_id} belongs to {}, not {}",
                stored.directory,
                cwd.display()
            )));
        }

        let existing = self.state.sessions.lock().await.get(&session_id).cloned();
        let session = if let Some(session) = existing {
            session
        } else {
            let choice = SessionChoice::Existing(session_id.clone());
            let options = TurnOptions {
                directory: Some(cwd),
                model: None,
                agent: persisted_session_agent(&choice),
                preset: None,
                session: choice,
                title: None,
                effort: None,
                tool_authority: None,
                extension_composition: ExtensionComposition::Active,
            };
            let session = self.open_session(options, client.clone()).await?;
            self.state
                .sessions
                .lock()
                .await
                .insert(session_id.clone(), Arc::clone(&session));
            session
        };
        if replay {
            session.replay(&client).await?;
        }
        session.lifecycle_response().await
    }

    fn list_sessions(&self, params: &Value) -> Result<Value, zuno_acp::RpcError> {
        if optional_string(params, "cursor")?.is_some() {
            return Err(zuno_acp::RpcError::invalid_params(
                "session/list cursor is not valid without a nextCursor from Zuno",
            ));
        }
        let cwd = optional_string(params, "cwd")?;
        if let Some(cwd) = cwd.as_deref()
            && !PathBuf::from(cwd).is_absolute()
        {
            return Err(zuno_acp::RpcError::invalid_params(
                "cwd must be an absolute path",
            ));
        }
        let pool = durable_pool()?;
        let store = zuno_db::session::Store::new(&pool);
        let mut query = match cwd {
            Some(cwd) => zuno_db::session::ListQuery::directory(cwd),
            None => zuno_db::session::ListQuery::global(),
        }
        .active_only();
        query.roots = true;
        let sessions = store
            .list(&query)
            .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
            .into_iter()
            .map(session_info)
            .collect::<Vec<_>>();
        Ok(json!({ "sessions": sessions }))
    }

    async fn prompt(
        &self,
        params: &Value,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let prompt = parse_prompt(params)?;
        self.session(&session_id)
            .await?
            .prompt(prompt, client)
            .await
    }

    async fn set_mode(
        &self,
        params: &Value,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let mode_id = required_string(params, "modeId")?;
        self.session(&session_id)
            .await?
            .reconfigure(
                SessionReconfiguration::Mode(mode_id),
                self.state.as_ref(),
                client,
            )
            .await?;
        Ok(json!({}))
    }

    async fn set_config_option(
        &self,
        params: &Value,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let config_id = required_string(params, "configId")?;
        let value = required_string(params, "value")?;
        let change = match config_id.as_str() {
            "agent" => SessionReconfiguration::Agent(value),
            "model" => SessionReconfiguration::Model(value),
            other => {
                return Err(zuno_acp::RpcError::invalid_params(format!(
                    "unknown ACP session config option {other}"
                )));
            }
        };
        let configuration = self
            .session(&session_id)
            .await?
            .reconfigure(change, self.state.as_ref(), client)
            .await?;
        Ok(json!({ "configOptions": configuration.config_options() }))
    }

    async fn close_session(&self, params: &Value) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let session = self.state.sessions.lock().await.remove(&session_id);
        if let Some(session) = session {
            session
                .shutdown()
                .await
                .map_err(zuno_acp::RpcError::internal)?;
        }
        Ok(json!({}))
    }

    async fn delete_session(&self, params: &Value) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        self.close_session(params).await?;
        let pool = durable_pool()?;
        zuno_db::session::Store::new(&pool)
            .remove(&session_id)
            .map_err(|error| map_session_lookup(&session_id, error))?;
        Ok(json!({}))
    }

    async fn session(&self, session_id: &str) -> Result<Arc<AcpSession>, zuno_acp::RpcError> {
        self.state
            .sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                zuno_acp::RpcError::invalid_params(format!(
                    "session {session_id} is not open in this ACP connection"
                ))
            })
    }

    async fn open_session(
        &self,
        options: TurnOptions,
        client: zuno_acp::ClientConnection,
    ) -> Result<Arc<AcpSession>, zuno_acp::RpcError> {
        let _composition = self.state.composition_gate.lock().await;
        let plan = TurnPlan::resolve(&options, &self.state.environment)
            .await
            .map_err(zuno_acp::RpcError::internal)?;
        let resources = open_session_resources(
            plan,
            &self.state.environment,
            self.state.runs.clone(),
            client,
            self.state.elicitation_form.load(Ordering::Acquire),
            None,
        )
        .await
        .map_err(zuno_acp::RpcError::internal)?;
        let id = resources.host.session_id().to_owned();
        let control = resources.host.control();
        Ok(Arc::new(AcpSession {
            id,
            control,
            prompt_active: AtomicBool::new(false),
            resources: Mutex::new(Some(resources)),
        }))
    }

    async fn shutdown(&self) -> Result<(), String> {
        let sessions = std::mem::take(&mut *self.state.sessions.lock().await);
        let mut failures = Vec::new();
        for session in sessions.into_values() {
            if let Err(error) = session.shutdown().await {
                failures.push(format!("{}: {error}", session.id));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

struct AcpSession {
    id: String,
    control: SessionControl,
    prompt_active: AtomicBool,
    resources: Mutex<Option<SessionResources>>,
}

struct SessionResources {
    host: TurnHost,
    mcp: Option<McpRuntime>,
    configuration: SessionConfiguration,
}

#[derive(Debug)]
enum SessionReconfiguration {
    Mode(String),
    Agent(String),
    Model(String),
}

#[derive(Debug, Clone, Copy)]
enum ConfigurationPersistence {
    Agent,
    Model,
}

struct PreparedReconfiguration {
    options: TurnOptions,
    persistence: ConfigurationPersistence,
}

async fn open_session_resources(
    plan: TurnPlan,
    environment: &StartupEnvironment,
    runs: SessionRunRegistry,
    client: zuno_acp::ClientConnection,
    elicitation_form: bool,
    build_agent: Option<&str>,
) -> Result<SessionResources, String> {
    let configuration = SessionConfiguration::from_plan(&plan, build_agent);
    let workspace = plan
        .worktree()
        .unwrap_or_else(|| plan.directory())
        .to_path_buf();
    let mut mcp = McpRuntime::from_config(plan.config(), &workspace);
    let notes = match mcp.as_ref() {
        Some(mcp) => mcp.connect().await,
        None => Vec::new(),
    };
    let question = elicitation_form.then(|| {
        Arc::new(zuno_acp::AcpQuestionAsker::new(client.clone()))
            as Arc<dyn zuno_tools::question::QuestionAsker>
    });
    let approval: Arc<dyn PermissionAsker> = Arc::new(zuno_acp::AcpPermissionAsker::new(
        client,
        "Approve Zuno tool call",
    ));
    let host = TurnHost::open_with_runtime_and_mcp(
        plan,
        environment,
        approval,
        question,
        runs,
        mcp.as_ref().map(McpRuntime::catalog),
    )
    .await;
    let mut host = match host {
        Ok(host) => host,
        Err(error) => {
            if let Some(mcp) = mcp.take() {
                mcp.shutdown().await;
            }
            return Err(error);
        }
    };
    if let Err(error) = host.activate_extension_composition() {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        return Err(match shutdown {
            Ok(()) => error,
            Err(shutdown) => {
                format!("{error}; candidate ACP host shutdown also failed: {shutdown}")
            }
        });
    }
    host.push_notes(notes);
    if let Err(error) = host.materialize_session() {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        return Err(match shutdown {
            Ok(()) => error,
            Err(shutdown) => {
                format!("{error}; materialization cleanup also failed: {shutdown}")
            }
        });
    }
    Ok(SessionResources {
        host,
        mcp,
        configuration,
    })
}

async fn shutdown_session_resources(mut resources: SessionResources) -> Result<(), String> {
    let host = resources.host.shutdown().await;
    if let Some(mcp) = resources.mcp.take() {
        mcp.shutdown().await;
    }
    host
}

async fn restore_session_after_failure(
    slot: &mut Option<SessionResources>,
    session_id: &str,
    options: TurnOptions,
    state: &AcpState,
    client: zuno_acp::ClientConnection,
    build_agent: &str,
    cause: String,
) -> zuno_acp::RpcError {
    let rollback = async {
        let plan = TurnPlan::resolve(&options, &state.environment).await?;
        let resources = open_session_resources(
            plan,
            &state.environment,
            state.runs.clone(),
            client,
            state.elicitation_form.load(Ordering::Acquire),
            Some(build_agent),
        )
        .await?;
        if resources.host.session_id() == session_id {
            return Ok(resources);
        }
        let actual = resources.host.session_id().to_owned();
        let cleanup = shutdown_session_resources(resources).await;
        Err(match cleanup {
            Ok(()) => format!(
                "rollback produced ACP session {actual}, expected {session_id}"
            ),
            Err(cleanup) => format!(
                "rollback produced ACP session {actual}, expected {session_id}; rollback cleanup failed: {cleanup}"
            ),
        })
    }
    .await;
    match rollback {
        Ok(resources) => {
            *slot = Some(resources);
            zuno_acp::RpcError::internal(cause)
        }
        Err(rollback) => zuno_acp::RpcError::internal(format!(
            "{cause}; rollback failed and session {session_id} is closed: {rollback}"
        )),
    }
}

impl AcpSession {
    async fn lifecycle_response(&self) -> Result<Value, zuno_acp::RpcError> {
        let resources = self.resources.lock().await;
        let resources = resources.as_ref().ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
        })?;
        Ok(resources.configuration.lifecycle_response())
    }

    async fn reconfigure(
        &self,
        change: SessionReconfiguration,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
    ) -> Result<SessionConfiguration, zuno_acp::RpcError> {
        if self.prompt_active.load(Ordering::Acquire) {
            return Err(zuno_acp::RpcError::invalid_params(
                "session configuration cannot change while a prompt is active",
            ));
        }
        let _composition = state.composition_gate.lock().await;
        let mut slot = self.resources.lock().await;
        let current = slot.take().ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
        })?;
        let prepared = match current
            .configuration
            .prepare_reconfiguration(&current.host, change)
        {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                let configuration = current.configuration.clone();
                *slot = Some(current);
                return Ok(configuration);
            }
            Err(error) => {
                *slot = Some(current);
                return Err(error);
            }
        };
        let rollback_options = rollback_options(&current.host);
        let build_agent = current.configuration.build_agent.clone();
        let plan = match TurnPlan::resolve(&prepared.options, &state.environment).await {
            Ok(plan) => plan,
            Err(error) => {
                *slot = Some(current);
                return Err(zuno_acp::RpcError::invalid_params(error));
            }
        };
        if let Err(error) = shutdown_session_resources(current).await {
            return Err(zuno_acp::RpcError::internal(format!(
                "could not stop the previous ACP session host: {error}; session {} is closed",
                self.id
            )));
        }
        let candidate = match open_session_resources(
            plan,
            &state.environment,
            state.runs.clone(),
            client.clone(),
            state.elicitation_form.load(Ordering::Acquire),
            Some(&build_agent),
        )
        .await
        {
            Ok(candidate) => candidate,
            Err(error) => {
                return Err(restore_session_after_failure(
                    &mut slot,
                    &self.id,
                    rollback_options,
                    state,
                    client,
                    &build_agent,
                    format!("ACP session reconfiguration failed: {error}"),
                )
                .await);
            }
        };
        if candidate.host.session_id() != self.id {
            let actual = candidate.host.session_id().to_owned();
            let cleanup = shutdown_session_resources(candidate).await;
            let cause = match cleanup {
                Ok(()) => format!(
                    "ACP session reconfiguration produced {actual}, expected {}",
                    self.id
                ),
                Err(cleanup) => format!(
                    "ACP session reconfiguration produced {actual}, expected {}; candidate cleanup failed: {cleanup}",
                    self.id
                ),
            };
            return Err(restore_session_after_failure(
                &mut slot,
                &self.id,
                rollback_options,
                state,
                client,
                &build_agent,
                cause,
            )
            .await);
        }
        let persistence = match prepared.persistence {
            ConfigurationPersistence::Agent => candidate.host.persist_active_agent(),
            ConfigurationPersistence::Model => candidate.host.persist_active_model(),
        };
        if let Err(error) = persistence {
            let cleanup = shutdown_session_resources(candidate).await;
            let cause = match cleanup {
                Ok(()) => format!("could not persist ACP session configuration: {error}"),
                Err(cleanup) => format!(
                    "could not persist ACP session configuration: {error}; candidate cleanup failed: {cleanup}"
                ),
            };
            return Err(restore_session_after_failure(
                &mut slot,
                &self.id,
                rollback_options,
                state,
                client,
                &build_agent,
                cause,
            )
            .await);
        }
        let configuration = candidate.configuration.clone();
        *slot = Some(candidate);
        Ok(configuration)
    }

    async fn prompt(
        &self,
        prompt: AcpPrompt,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let _active = ActivePrompt::begin(&self.prompt_active)?;
        let context_size = {
            let resources = self.resources.lock().await;
            resources
                .as_ref()
                .ok_or_else(|| {
                    zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
                })?
                .configuration
                .context_size
        };
        let (events, receiver) = event_channel();
        let drive = async {
            let mut resources = self.resources.lock().await;
            let resources = resources.as_mut().ok_or_else(|| {
                zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
            })?;
            let outcome = resources
                .host
                .drive_content(&prompt.text, &prompt.content, events.clone())
                .await;
            drop(events);
            outcome.map_err(zuno_acp::RpcError::internal)
        };
        let projection = project_turn(&self.id, context_size, receiver, client.clone());
        let (driven, projected) = tokio::join!(drive, projection);
        let projected = projected?;
        self.project_plan(&client).await?;
        match projected {
            ProjectedTurn::Completed(stop_reason) => {
                driven?;
                Ok(json!({ "stopReason": stop_reason }))
            }
            ProjectedTurn::Interrupted => Ok(json!({ "stopReason": "cancelled" })),
            ProjectedTurn::Failed(message) => {
                let error = driven.err().map_or(message, |error| error.message);
                Err(zuno_acp::RpcError::internal(error))
            }
            ProjectedTurn::Missing => match driven {
                Ok(()) => Err(zuno_acp::RpcError::internal(
                    "turn ended without a terminal durable event",
                )),
                Err(error) => Err(error),
            },
        }
    }

    async fn project_plan(
        &self,
        client: &zuno_acp::ClientConnection,
    ) -> Result<(), zuno_acp::RpcError> {
        let update = {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| {
                zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
            })?;
            let work = resources
                .host
                .work_state()
                .map_err(zuno_acp::RpcError::internal)?;
            zuno_acp::durable_plan_update(&work)
        };
        if let Some(update) = update {
            client.session_update(&self.id, update).await?;
        }
        Ok(())
    }

    fn cancel(&self) {
        if self.prompt_active.load(Ordering::Acquire) {
            let _disposition = self.control.abort();
        }
    }

    async fn replay(&self, client: &zuno_acp::ClientConnection) -> Result<(), zuno_acp::RpcError> {
        let (history, work_state, context_size, cumulative_cost) = {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| {
                zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
            })?;
            let history = resources
                .host
                .resumed_history()
                .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
            let work_state = resources
                .host
                .work_state()
                .map_err(zuno_acp::RpcError::internal)?;
            let usage = resources.host.session_usage();
            let context_size = usage
                .context_limit
                .and_then(|limit| u64::try_from(limit).ok())
                .filter(|limit| *limit > 0)
                .unwrap_or(resources.configuration.context_size);
            (history, work_state, context_size, usage.cost)
        };
        for update in zuno_acp::durable_updates(&history) {
            client.session_update(&self.id, update).await?;
        }
        if let Some(update) = zuno_acp::durable_plan_update(&work_state) {
            client.session_update(&self.id, update).await?;
        }
        if let Some(update) =
            zuno_acp::durable_usage_update(&history, context_size, cumulative_cost)
        {
            client.session_update(&self.id, update).await?;
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), String> {
        self.control.abort();
        let Some(mut resources) = self.resources.lock().await.take() else {
            return Ok(());
        };
        let host = resources.host.shutdown().await;
        if let Some(mcp) = resources.mcp.take() {
            mcp.shutdown().await;
        }
        host
    }
}

fn live_options(host: &TurnHost) -> TurnOptions {
    host_options(host, host.model_override().map(str::to_owned))
}

fn rollback_options(host: &TurnHost) -> TurnOptions {
    host_options(host, Some(host.qualified_model()))
}

fn host_options(host: &TurnHost, model: Option<String>) -> TurnOptions {
    TurnOptions {
        directory: Some(PathBuf::from(host.session_directory())),
        model,
        agent: Some(host.agent_name().to_owned()),
        preset: host.preset_name().map(str::to_owned),
        session: host.rebuild_session_choice(),
        title: None,
        effort: host.effort_override(),
        tool_authority: None,
        extension_composition: ExtensionComposition::Active,
    }
}

#[derive(Clone)]
struct SessionConfiguration {
    mode: &'static str,
    context_size: u64,
    active_agent: String,
    build_agent: String,
    agents: Vec<AgentChoice>,
    model: String,
    models: Vec<CatalogModelChoice>,
}

#[derive(Clone)]
struct AgentChoice {
    name: String,
    description: Option<String>,
}

impl SessionConfiguration {
    fn from_plan(plan: &TurnPlan, preserved_build_agent: Option<&str>) -> Self {
        let active_agent = plan.agent_name().to_owned();
        let mode = if active_agent == "plan" {
            "plan"
        } else {
            "build"
        };
        let agents = plan
            .agents()
            .iter()
            .filter(|agent| agent.hidden != Some(true) && agent.name != "plan")
            .map(|agent| AgentChoice {
                name: agent.name.clone(),
                description: agent.description.clone(),
            })
            .collect::<Vec<_>>();
        let build_agent = if mode == "build" {
            active_agent.clone()
        } else {
            preserved_build_agent
                .filter(|name| agents.iter().any(|agent| agent.name == *name))
                .map(str::to_owned)
                .or_else(|| {
                    ["build", "orchestrator"]
                        .into_iter()
                        .find(|name| agents.iter().any(|agent| agent.name == *name))
                        .map(str::to_owned)
                })
                .or_else(|| agents.first().map(|agent| agent.name.clone()))
                .unwrap_or_else(|| active_agent.clone())
        };
        Self {
            mode,
            context_size: plan.context_window(),
            active_agent,
            build_agent,
            agents,
            model: plan.qualified_model(),
            models: plan.catalog_models(),
        }
    }

    fn prepare_reconfiguration(
        &self,
        host: &TurnHost,
        change: SessionReconfiguration,
    ) -> Result<Option<PreparedReconfiguration>, zuno_acp::RpcError> {
        let mut options = live_options(host);
        let persistence = match change {
            SessionReconfiguration::Mode(mode) => {
                if mode == self.mode {
                    return Ok(None);
                }
                options.agent = Some(match mode.as_str() {
                    "plan" => "plan".to_owned(),
                    "build" => self.build_agent.clone(),
                    other => {
                        return Err(zuno_acp::RpcError::invalid_params(format!(
                            "unknown ACP session mode {other}; expected build or plan"
                        )));
                    }
                });
                ConfigurationPersistence::Agent
            }
            SessionReconfiguration::Agent(agent) => {
                if !self.agents.iter().any(|choice| choice.name == agent) {
                    let available = self
                        .agents
                        .iter()
                        .map(|choice| choice.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(zuno_acp::RpcError::invalid_params(format!(
                        "unknown ACP Agent {agent}; available Agents: {available}"
                    )));
                }
                if self.mode == "build" && self.active_agent == agent {
                    return Ok(None);
                }
                options.agent = Some(agent);
                ConfigurationPersistence::Agent
            }
            SessionReconfiguration::Model(model) => {
                if !self.models.iter().any(|choice| choice.id == model) {
                    let available = self
                        .models
                        .iter()
                        .map(|choice| choice.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(zuno_acp::RpcError::invalid_params(format!(
                        "unknown ACP model {model}; available models: {available}"
                    )));
                }
                if self.model == model {
                    return Ok(None);
                }
                options.model = Some(model);
                ConfigurationPersistence::Model
            }
        };
        Ok(Some(PreparedReconfiguration {
            options,
            persistence,
        }))
    }

    fn lifecycle_response(&self) -> Value {
        json!({
            "modes": {
                "currentModeId": self.mode,
                "availableModes": [
                    {
                        "id": "build",
                        "name": "Build",
                        "description": "Use the active implementation Agent and its full authorized tool set."
                    },
                    {
                        "id": "plan",
                        "name": "Plan",
                        "description": "Use Zuno's read-only planning Agent."
                    }
                ]
            },
            "configOptions": self.config_options()
        })
    }

    fn config_options(&self) -> Vec<Value> {
        vec![
            json!({
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": self.model,
                "options": self.models.iter().map(|model| json!({
                    "value": model.id,
                    "name": model.name,
                    "description": format!("{} provider", model.provider),
                })).collect::<Vec<_>>(),
            }),
            json!({
                "id": "agent",
                "name": "Agent",
                "category": "_agent",
                "type": "select",
                "currentValue": self.build_agent,
                "options": self.agents.iter().map(|agent| json!({
                    "value": agent.name,
                    "name": agent.name,
                    "description": agent.description,
                })).collect::<Vec<_>>(),
            }),
        ]
    }
}

struct ActivePrompt<'a> {
    active: &'a AtomicBool,
}

impl<'a> ActivePrompt<'a> {
    fn begin(active: &'a AtomicBool) -> Result<Self, zuno_acp::RpcError> {
        active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                zuno_acp::RpcError::invalid_params(
                    "a prompt is already active for this ACP session",
                )
            })?;
        Ok(Self { active })
    }
}

impl Drop for ActivePrompt<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

struct AcpPrompt {
    text: String,
    content: Vec<RequestContentBlock>,
}

fn parse_prompt(params: &Value) -> Result<AcpPrompt, zuno_acp::RpcError> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| zuno_acp::RpcError::invalid_params("prompt must be an array"))?;
    let mut rendered = Vec::new();
    let mut content = Vec::new();
    for block in blocks {
        let kind = block.get("type").and_then(Value::as_str).ok_or_else(|| {
            zuno_acp::RpcError::invalid_params("each prompt block must have a string type")
        })?;
        let resolved = match kind {
            "text" => RequestContentBlock::Text {
                text: block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        zuno_acp::RpcError::invalid_params(
                            "text prompt blocks must contain a string text field",
                        )
                    })?
                    .to_owned(),
            },
            "resource_link" => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        zuno_acp::RpcError::invalid_params(
                            "resource_link blocks must contain a non-empty name",
                        )
                    })?;
                let uri = block
                    .get("uri")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        zuno_acp::RpcError::invalid_params(
                            "resource_link blocks must contain a non-empty uri",
                        )
                    })?;
                RequestContentBlock::ResourceLink {
                    name: name.to_owned(),
                    uri: uri.to_owned(),
                    title: optional_string(block, "title")?,
                    description: optional_string(block, "description")?,
                    media_type: optional_string(block, "mimeType")?,
                    size: optional_u64(block, "size")?,
                }
            }
            other => {
                return Err(zuno_acp::RpcError::invalid_params(format!(
                    "prompt block type {other} is not advertised by this ACP adapter"
                )));
            }
        };
        let text = resolved.provider_text().map(|text| text.into_owned());
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            rendered.push(text);
            content.push(resolved);
        }
    }
    if content.is_empty() {
        return Err(zuno_acp::RpcError::invalid_params(
            "prompt must contain text or a resource link",
        ));
    }
    Ok(AcpPrompt {
        text: rendered.join("\n\n"),
        content,
    })
}

enum ProjectedTurn {
    Completed(&'static str),
    Interrupted,
    Failed(String),
    Missing,
}

async fn project_turn(
    session_id: &str,
    context_size: u64,
    mut receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
    client: zuno_acp::ClientConnection,
) -> Result<ProjectedTurn, zuno_acp::RpcError> {
    let mut projector = zuno_acp::TurnEventProjector::with_context_size(context_size);
    let mut finish_reason = None;
    while let Some(event) = receiver.recv().await {
        if let Some(update) = projector.project(&event) {
            client.session_update(session_id, update).await?;
        }
        match event {
            TurnEvent::StepCompleted {
                finish_reason: reason,
                ..
            } => finish_reason = reason,
            TurnEvent::TurnCompleted { .. } => {
                return Ok(ProjectedTurn::Completed(match finish_reason {
                    Some(FinishReason::Length) => "max_tokens",
                    Some(FinishReason::ContentFilter) => "refusal",
                    _ => "end_turn",
                }));
            }
            TurnEvent::TurnInterrupted { .. } => return Ok(ProjectedTurn::Interrupted),
            TurnEvent::TurnFailed { message, .. } => {
                return Ok(ProjectedTurn::Failed(message));
            }
            _ => {}
        }
    }
    Ok(ProjectedTurn::Missing)
}

fn lifecycle_directory(params: &Value) -> Result<PathBuf, zuno_acp::RpcError> {
    let cwd = PathBuf::from(required_string(params, "cwd")?);
    if !cwd.is_absolute() {
        return Err(zuno_acp::RpcError::invalid_params(
            "cwd must be an absolute path",
        ));
    }
    Ok(cwd)
}

fn require_empty_roots_and_client_mcp(params: &Value) -> Result<(), zuno_acp::RpcError> {
    for (field, label) in [
        ("additionalDirectories", "additional workspace directories"),
        ("mcpServers", "client-provided MCP servers"),
    ] {
        let Some(value) = params.get(field) else {
            if field == "mcpServers" {
                return Err(zuno_acp::RpcError::invalid_params(
                    "mcpServers must be an array",
                ));
            }
            continue;
        };
        let values = value.as_array().ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(format!("{field} must be an array"))
        })?;
        if !values.is_empty() {
            return Err(zuno_acp::RpcError::invalid_params(format!(
                "{label} are not advertised by this ACP adapter; configure them in Zuno"
            )));
        }
    }
    Ok(())
}

fn durable_pool() -> Result<zuno_db::pool::Pool, zuno_acp::RpcError> {
    let pool = zuno_db::pool::Pool::open_default()
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    let mut connection = pool
        .open_connection()
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    zuno_db::migration::apply(&mut connection)
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    drop(connection);
    Ok(pool)
}

fn map_session_lookup(session_id: &str, error: zuno_error::DbError) -> zuno_acp::RpcError {
    match error {
        zuno_error::DbError::NotFound { .. } => {
            zuno_acp::RpcError::invalid_params(format!("unknown session {session_id}"))
        }
        error => zuno_acp::RpcError::internal(error.to_string()),
    }
}

fn session_info(session: zuno_db::session::Session) -> Value {
    let title = (!zuno_db::session::is_default_title(&session.title)).then_some(session.title);
    json!({
        "sessionId": session.id,
        "cwd": session.directory,
        "title": title,
        "updatedAt": timestamp(session.time_updated),
    })
}

fn timestamp(milliseconds: i64) -> Option<String> {
    let nanos = i128::from(milliseconds).checked_mul(1_000_000)?;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn with_session_id(mut response: Value, session_id: &str) -> Value {
    response["sessionId"] = Value::String(session_id.to_owned());
    response
}

fn required_string(params: &Value, field: &str) -> Result<String, zuno_acp::RpcError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(format!("{field} must be a non-empty string"))
        })
}

fn optional_u64(params: &Value, field: &str) -> Result<Option<u64>, zuno_acp::RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(format!("{field} must be a non-negative integer"))
        }),
    }
}

fn optional_string(params: &Value, field: &str) -> Result<Option<String>, zuno_acp::RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(zuno_acp::RpcError::invalid_params(format!(
            "{field} must be a string"
        ))),
    }
}
