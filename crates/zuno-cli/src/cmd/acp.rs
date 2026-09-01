use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use zuno_engine::interrupt::{HardInterruptReason, HardInterruptRequest, HardInterruptSource};
use zuno_engine::r#loop::{TurnEvent, event_channel};
use zuno_engine::session_command::SessionCommand;
use zuno_engine::status::{SessionControl, SessionRunRegistry};
use zuno_llm::event::{FinishReason, RequestContentBlock};
use zuno_tool::PermissionAsker;

use super::child_turn::{ChildTurnObserver, DetachedTurnObserver};
use super::mcp_runtime::{McpRuntime, RequiredMcpServers};
use super::turn::{
    CatalogModelChoice, ExtensionComposition, SessionChoice, SessionCommandError, TurnHost,
    TurnHostRuntimeDependencies, TurnOptions, TurnPlan, persisted_session_agent,
};

use crate::command::AcpArgs;
use crate::environment::StartupEnvironment;

const ACP_PROTOCOL_VERSION: u64 = 1;
const ACP_SCHEMA_VERSION: &str = "1.21.0";
const ACP_TEXT_RESOURCE_MAX_BYTES: usize = 50 * 1_024;
const ACP_TEXT_RESOURCE_MAX_LINES: usize = 2_000;
const MAX_OPEN_ACP_SESSIONS: usize = 32;

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
    session_slots: Arc<Semaphore>,
    composition_gate: Mutex<()>,
    elicitation_form: AtomicBool,
    native_subagents: AtomicBool,
    permission_grants: Arc<zuno_acp::AcpPermissionGrants>,
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
        session.cancel(HardInterruptReason::UserCancel);
        Ok(())
    }

    async fn request_cancelled(&self, method: &str, params: &Value) {
        if method != "session/prompt" {
            return;
        }
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            return;
        };
        let session = self.state.sessions.lock().await.get(session_id).cloned();
        if let Some(session) = session {
            session.cancel(HardInterruptReason::RequestCancelled);
        }
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
    let mut response = json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": true,
                "audio": false,
                "embeddedContext": true,
            },
            "mcpCapabilities": {
                "stdio": true,
                "http": true,
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
    });
    if supports_native_subagents(params) {
        response["agentCapabilities"]["sessionCapabilities"]["subagents"] = json!({});
    }
    Ok(response)
}

fn supports_native_subagents(params: &Value) -> bool {
    params
        .pointer("/clientCapabilities/subagents")
        .is_some_and(Value::is_object)
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
        self.state
            .native_subagents
            .store(supports_native_subagents(params), Ordering::Release);
        Ok(response)
    }

    fn new(environment: StartupEnvironment) -> Self {
        Self {
            state: Arc::new(AcpState {
                environment,
                runs: SessionRunRegistry::new(),
                sessions: Mutex::new(HashMap::new()),
                session_slots: Arc::new(Semaphore::new(MAX_OPEN_ACP_SESSIONS)),
                composition_gate: Mutex::new(()),
                elicitation_form: AtomicBool::new(false),
                native_subagents: AtomicBool::new(false),
                permission_grants: Arc::new(zuno_acp::AcpPermissionGrants::default()),
            }),
        }
    }

    async fn new_session(
        &self,
        params: &Value,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let cwd = lifecycle_directory(params)?;
        require_empty_additional_directories(params)?;
        let mcp_servers = zuno_acp::parse_mcp_servers(params.get("mcpServers"))
            .map_err(|error| zuno_acp::RpcError::invalid_params(error.to_string()))?;
        let options = TurnOptions {
            directory: Some(cwd),
            model: None,
            agent: None,
            preset: None,
            session: SessionChoice::New,
            title: None,
            effort: None,
            variant: None,
            thinking: false,
            tool_authority: None,
            extension_composition: ExtensionComposition::Active,
        };
        let session_slot = self.reserve_session_slot()?;
        let session = self
            .open_session(options, client.clone(), session_slot, mcp_servers, true)
            .await?;
        let session_id = session.id.clone();
        let response = session.lifecycle_response().await?;
        let mut sessions = self.state.sessions.lock().await;
        let inserted = match sessions.entry(session_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(session.clone());
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        };
        drop(sessions);
        if !inserted {
            session
                .shutdown()
                .await
                .map_err(zuno_acp::RpcError::internal)?;
            return Err(zuno_acp::RpcError::internal(
                "generated duplicate ACP session id",
            ));
        }
        if let Err(error) = session.defer_available_commands(&client).await {
            self.state.sessions.lock().await.remove(&session_id);
            let _shutdown = session.shutdown().await;
            return Err(error);
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
        require_empty_additional_directories(params)?;
        let mcp_servers = zuno_acp::parse_mcp_servers(params.get("mcpServers"))
            .map_err(|error| zuno_acp::RpcError::invalid_params(error.to_string()))?;
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

        let previous = self.state.sessions.lock().await.remove(&session_id);
        if let Some(previous) = previous {
            previous
                .shutdown()
                .await
                .map_err(zuno_acp::RpcError::internal)?;
        };
        let choice = SessionChoice::Existing(session_id.clone());
        let options = TurnOptions {
            directory: Some(cwd),
            model: None,
            agent: persisted_session_agent(&choice),
            preset: None,
            session: choice,
            title: None,
            effort: None,
            variant: None,
            thinking: false,
            tool_authority: None,
            extension_composition: ExtensionComposition::Active,
        };
        let session_slot = self.reserve_session_slot()?;
        let session = self
            .open_dormant_session(options, session_slot, mcp_servers)
            .await?;
        if let Err(error) = session.ensure_active(&self.state, client.clone()).await {
            let _shutdown = session.shutdown().await;
            return Err(error);
        }
        self.state
            .sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::clone(&session));
        if replay {
            if let Err(error) = session
                .replay(&client, self.state.native_subagents.load(Ordering::Acquire))
                .await
            {
                self.state.sessions.lock().await.remove(&session_id);
                let _shutdown = session.shutdown().await;
                return Err(error);
            }
        } else {
            session.mark_replay_satisfied().await?;
        }
        if let Err(error) = session.defer_available_commands(&client).await {
            self.state.sessions.lock().await.remove(&session_id);
            let _shutdown = session.shutdown().await;
            return Err(error);
        }
        match session.lifecycle_response().await {
            Ok(response) => Ok(response),
            Err(error) => {
                self.state.sessions.lock().await.remove(&session_id);
                let _shutdown = session.shutdown().await;
                Err(error)
            }
        }
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
            .prompt(prompt, self.state.as_ref(), client)
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
            "reasoning_effort" => SessionReconfiguration::Reasoning(value),
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
        let session = self.state.sessions.lock().await.get(&session_id).cloned();
        let shutdown = if let Some(session) = session.as_ref() {
            session.shutdown().await
        } else {
            Ok(())
        };
        if let Some(session) = session.as_ref() {
            let mut sessions = self.state.sessions.lock().await;
            if sessions
                .get(&session_id)
                .is_some_and(|current| Arc::ptr_eq(current, session))
            {
                sessions.remove(&session_id);
            }
        }
        drop(session);
        self.state.permission_grants.clear_session(&session_id);
        shutdown.map_err(zuno_acp::RpcError::internal)?;
        Ok(json!({}))
    }

    async fn delete_session(&self, params: &Value) -> Result<Value, zuno_acp::RpcError> {
        let session_id = required_string(params, "sessionId")?;
        let cleanup_derived_experiences = required_bool(params, "cleanupDerivedExperiences")?;
        let session = self.state.sessions.lock().await.get(&session_id).cloned();
        let outcome = match session.as_ref() {
            Some(session) => session.delete_durable(cleanup_derived_experiences).await?,
            None if cleanup_derived_experiences => {
                return Err(zuno_acp::RpcError::invalid_params(format!(
                    "session {session_id} must be open to prepare Memory and Skill revocation candidates"
                )));
            }
            None => {
                let pool = durable_pool()?;
                super::turn::SessionDeleteOutcome {
                    deleted_session_ids: zuno_db::session::Store::new(&pool)
                        .remove(&session_id)
                        .map_err(|error| map_session_lookup(&session_id, error))?,
                    ..super::turn::SessionDeleteOutcome::default()
                }
            }
        };
        if session.is_some() {
            self.close_session(params).await?;
        }
        Ok(json!({
            "deletedSessionIds": outcome.deleted_session_ids,
            "forgottenExperienceIds": outcome.forgotten_experience_ids,
            "memoryRevocationCandidateIds": outcome.memory_revocation_candidate_ids,
            "skillRevocationCandidateIds": outcome.skill_revocation_candidate_ids,
            "rejectedMemoryCandidateIds": outcome.rejected_memory_candidate_ids,
            "rejectedSkillCandidateIds": outcome.rejected_skill_candidate_ids,
        }))
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

    fn reserve_session_slot(&self) -> Result<OwnedSemaphorePermit, zuno_acp::RpcError> {
        Arc::clone(&self.state.session_slots)
            .try_acquire_owned()
            .map_err(|_| session_capacity_error())
    }

    async fn open_session(
        &self,
        options: TurnOptions,
        client: zuno_acp::ClientConnection,
        session_slot: OwnedSemaphorePermit,
        mcp_servers: Vec<zuno_acp::AcpMcpServer>,
        replayed: bool,
    ) -> Result<Arc<AcpSession>, zuno_acp::RpcError> {
        let _composition = self.state.composition_gate.lock().await;
        let plan = TurnPlan::resolve(&options, &self.state.environment)
            .await
            .map_err(zuno_acp::RpcError::internal)?;
        let replay_root = plan
            .worktree()
            .unwrap_or_else(|| plan.directory())
            .to_path_buf();
        let background_notification_directory = plan.directory().to_path_buf();
        let background_notifications = self.state.environment.background_notifications();
        let resources = open_session_resources(
            plan,
            &self.state.environment,
            self.state.runs.clone(),
            AcpSurfaceContext::from_state(self.state.as_ref(), client),
            None,
            &mcp_servers,
        )
        .await
        .map_err(zuno_acp::RpcError::internal)?;
        let id = resources.host.session_id().to_owned();
        let control = resources.host.control();
        Ok(Arc::new(AcpSession {
            id,
            control,
            prompt_active: AtomicBool::new(false),
            replayed: AtomicBool::new(replayed),
            closed: AtomicBool::new(false),
            replay_gate: Mutex::new(()),
            mount_gate: Mutex::new(()),
            replay_root,
            background_notification_directory,
            background_notifications,
            _session_slot: session_slot,
            mcp_servers: Arc::from(mcp_servers),
            dormant: Mutex::new(None),
            resources: Mutex::new(Some(resources)),
        }))
    }

    async fn open_dormant_session(
        &self,
        options: TurnOptions,
        session_slot: OwnedSemaphorePermit,
        mcp_servers: Vec<zuno_acp::AcpMcpServer>,
    ) -> Result<Arc<AcpSession>, zuno_acp::RpcError> {
        let session_id = match &options.session {
            SessionChoice::Existing(session_id) => session_id.clone(),
            _ => {
                return Err(zuno_acp::RpcError::internal(
                    "a dormant ACP session must reference an existing durable session",
                ));
            }
        };
        let _composition = self.state.composition_gate.lock().await;
        let plan = TurnPlan::resolve(&options, &self.state.environment)
            .await
            .map_err(zuno_acp::RpcError::internal)?;
        let replay_root = plan
            .worktree()
            .unwrap_or_else(|| plan.directory())
            .to_path_buf();
        let background_notification_directory = plan.directory().to_path_buf();
        let background_notifications = self.state.environment.background_notifications();
        let configuration = SessionConfiguration::from_plan(&plan, None);
        let available_commands =
            available_commands_for_plan(&plan, self.state.environment.resolved())
                .map_err(zuno_acp::RpcError::internal)?;
        Ok(Arc::new(AcpSession {
            id: session_id.clone(),
            control: self.state.runs.control(session_id),
            prompt_active: AtomicBool::new(false),
            replayed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            replay_gate: Mutex::new(()),
            mount_gate: Mutex::new(()),
            replay_root,
            background_notification_directory,
            background_notifications,
            _session_slot: session_slot,
            mcp_servers: Arc::from(mcp_servers),
            dormant: Mutex::new(Some(DormantSession {
                options,
                configuration,
                available_commands,
            })),
            resources: Mutex::new(None),
        }))
    }

    async fn shutdown(&self) -> Result<(), String> {
        let sessions = std::mem::take(&mut *self.state.sessions.lock().await);
        let mut failures = Vec::new();
        for session in sessions.into_values() {
            self.state.permission_grants.clear_session(&session.id);
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
    replayed: AtomicBool,
    closed: AtomicBool,
    replay_gate: Mutex<()>,
    mount_gate: Mutex<()>,
    replay_root: PathBuf,
    background_notification_directory: PathBuf,
    background_notifications: super::background_notification::BackgroundNotificationRegistry,
    _session_slot: OwnedSemaphorePermit,
    mcp_servers: Arc<[zuno_acp::AcpMcpServer]>,
    dormant: Mutex<Option<DormantSession>>,
    resources: Mutex<Option<SessionResources>>,
}

struct DormantSession {
    options: TurnOptions,
    configuration: SessionConfiguration,
    available_commands: Value,
}

struct SessionResources {
    host: TurnHost,
    mcp: Option<McpRuntime>,
    subagents: Option<super::acp_subagent::AcpSubagentBridge>,
    subagent_flush: Option<super::acp_subagent::AcpSubagentFlush>,
    question_asker: Option<Arc<zuno_acp::AcpQuestionAsker>>,
    permission_asker: Arc<zuno_acp::AcpPermissionAsker>,
    configuration: SessionConfiguration,
}

#[derive(Debug)]
enum SessionReconfiguration {
    Mode(String),
    Agent(String),
    Model(String),
    Reasoning(String),
}

#[derive(Debug, Clone, Copy)]
enum ConfigurationPersistence {
    None,
    Agent,
    Model,
}

struct PreparedReconfiguration {
    options: TurnOptions,
    persistence: ConfigurationPersistence,
}

#[derive(Clone)]
struct AcpSurfaceContext {
    client: zuno_acp::ClientConnection,
    permission_grants: Arc<zuno_acp::AcpPermissionGrants>,
    elicitation_form: bool,
    native_subagents: bool,
}

struct AcpDetachedTurnObserver {
    root_session_id: Arc<OnceLock<String>>,
    client: zuno_acp::ClientConnection,
    projector: Mutex<zuno_acp::AttemptBufferedTurnEventProjector>,
    children: Option<Arc<dyn ChildTurnObserver>>,
}

#[async_trait]
impl DetachedTurnObserver for AcpDetachedTurnObserver {
    async fn event(&self, session_id: &str, event: &TurnEvent) {
        if self
            .root_session_id
            .get()
            .is_some_and(|root| root == session_id)
        {
            let updates = self.projector.lock().await.project(event);
            for update in updates {
                if let Err(error) = self.client.session_update(session_id, update).await {
                    tracing::debug!(
                        session_id,
                        %error,
                        "detached root turn outlived its ACP projection"
                    );
                    break;
                }
            }
        } else if let Some(children) = self.children.as_ref() {
            children.event(session_id, event);
        }
    }

    async fn work_state(&self, session_id: &str, work: &zuno_types::WorkStateProjection) {
        if !self
            .root_session_id
            .get()
            .is_some_and(|root| root == session_id)
        {
            return;
        }
        for update in zuno_acp::durable_work_updates(work) {
            if let Err(error) = self.client.session_update(session_id, update).await {
                tracing::debug!(
                    session_id,
                    %error,
                    "detached root turn outlived its final ACP work-state projection"
                );
                break;
            }
        }
    }
}

impl AcpSurfaceContext {
    fn from_state(state: &AcpState, client: zuno_acp::ClientConnection) -> Self {
        Self {
            client,
            permission_grants: Arc::clone(&state.permission_grants),
            elicitation_form: state.elicitation_form.load(Ordering::Acquire),
            native_subagents: state.native_subagents.load(Ordering::Acquire),
        }
    }
}

fn required_mcp_servers(
    servers: &[zuno_acp::AcpMcpServer],
    workspace: &std::path::Path,
) -> RequiredMcpServers {
    use zuno_config::schema::mcp::{
        LocalKind, McpLocal, McpOauth, McpRemote, McpServerConfig, RemoteKind,
    };
    use zuno_config::schema::ordered::False;

    let workspace = workspace.to_string_lossy().into_owned();
    let entries = servers
        .iter()
        .map(|server| match server {
            zuno_acp::AcpMcpServer::Stdio(server) => {
                let mut command = Vec::with_capacity(1 + server.args().len());
                command.push(server.command().to_string_lossy().into_owned());
                command.extend(server.args().iter().cloned());
                (
                    server.name().to_owned(),
                    McpServerConfig::Local(McpLocal {
                        kind: LocalKind::Local,
                        command,
                        cwd: Some(workspace.clone()),
                        environment: Some(server.environment().clone()),
                        enabled: Some(true),
                        timeout: None,
                    }),
                )
            }
            zuno_acp::AcpMcpServer::Http(server) => (
                server.name().to_owned(),
                McpServerConfig::Remote(McpRemote {
                    kind: RemoteKind::Remote,
                    url: server.url().as_str().to_owned(),
                    enabled: Some(true),
                    headers: Some(server.headers().clone()),
                    oauth: Some(McpOauth::Disabled(False)),
                    timeout: None,
                    streamable_http_only: true,
                }),
            ),
        })
        .collect();
    RequiredMcpServers::new(entries)
}

async fn open_session_resources(
    plan: TurnPlan,
    environment: &StartupEnvironment,
    runs: SessionRunRegistry,
    surface: AcpSurfaceContext,
    build_agent: Option<&str>,
    client_mcp: &[zuno_acp::AcpMcpServer],
) -> Result<SessionResources, String> {
    let AcpSurfaceContext {
        client,
        permission_grants,
        elicitation_form,
        native_subagents,
    } = surface;
    let configuration = SessionConfiguration::from_plan(&plan, build_agent);
    let workspace = plan
        .worktree()
        .unwrap_or_else(|| plan.directory())
        .to_path_buf();
    let required_mcp = required_mcp_servers(client_mcp, plan.directory());
    let mut mcp = McpRuntime::from_config_with_required(plan.config(), &workspace, required_mcp)?;
    let notes = match mcp.as_ref() {
        Some(runtime) => match runtime.connect_required().await {
            Ok(notes) => notes,
            Err(error) => {
                if let Some(runtime) = mcp.take() {
                    runtime.shutdown().await;
                }
                return Err(error);
            }
        },
        None => Vec::new(),
    };
    let session_route = Arc::new(zuno_acp::AcpSessionRoute::new(native_subagents));
    let question_asker = elicitation_form.then(|| {
        Arc::new(zuno_acp::AcpQuestionAsker::with_route(
            client.clone(),
            Arc::clone(&session_route),
        ))
    });
    let question = question_asker
        .as_ref()
        .map(|asker| Arc::clone(asker) as Arc<dyn zuno_tools::question::QuestionAsker>);
    let permission_asker = Arc::new(zuno_acp::AcpPermissionAsker::with_grants_and_route(
        client.clone(),
        "Approve Zuno tool call",
        permission_grants,
        Arc::clone(&session_route),
    ));
    let approval: Arc<dyn PermissionAsker> =
        Arc::clone(&permission_asker) as Arc<dyn PermissionAsker>;
    let (child_observer, mut subagents) = if native_subagents {
        let (observer, bridge) = super::acp_subagent::AcpSubagentBridge::start(
            client.clone(),
            workspace.clone(),
            configuration.context_size,
        )?;
        (Some(observer), Some(bridge))
    } else {
        (None, None)
    };
    let detached_root = Arc::new(OnceLock::new());
    let detached_observer: Arc<dyn DetachedTurnObserver> = Arc::new(AcpDetachedTurnObserver {
        root_session_id: Arc::clone(&detached_root),
        client: client.clone(),
        projector: Mutex::new(
            zuno_acp::AttemptBufferedTurnEventProjector::with_context_size(
                configuration.context_size,
            ),
        ),
        children: child_observer.as_ref().map(Arc::clone),
    });
    let host = TurnHost::open_with_runtime_mcp_and_observers(
        plan,
        environment,
        TurnHostRuntimeDependencies {
            approval,
            question,
            runs,
            mcp: mcp.as_ref().map(McpRuntime::catalog),
            child_observer,
            detached_observer: Some(detached_observer),
        },
    )
    .await;
    let mut host = match host {
        Ok(host) => host,
        Err(error) => {
            if let Some(mcp) = mcp.take() {
                mcp.shutdown().await;
            }
            let bridge = shutdown_subagent_bridge(&mut subagents).await;
            return Err(bridge.map_or(error.clone(), |bridge| {
                format!("{error}; ACP subagent projector shutdown also failed: {bridge}")
            }));
        }
    };
    if let Some(bridge) = subagents.as_ref()
        && let Err(error) = bridge.bind_attachment_store(host.attachment_store())
    {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        let bridge = shutdown_subagent_bridge(&mut subagents).await;
        let shutdown = shutdown
            .err()
            .map(|shutdown| format!("; candidate ACP host shutdown failed: {shutdown}"))
            .unwrap_or_default();
        let bridge = bridge
            .map(|bridge| format!("; ACP subagent projector shutdown failed: {bridge}"))
            .unwrap_or_default();
        return Err(format!("{error}{shutdown}{bridge}"));
    }
    if let Err(error) = session_route.bind_root(host.session_id()) {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        let bridge = shutdown_subagent_bridge(&mut subagents).await;
        let host = shutdown
            .err()
            .map(|shutdown| format!("; candidate ACP host shutdown failed: {shutdown}"))
            .unwrap_or_default();
        let bridge = bridge
            .map(|bridge| format!("; ACP subagent projector shutdown failed: {bridge}"))
            .unwrap_or_default();
        return Err(format!("{error}{host}{bridge}"));
    }
    if detached_root.set(host.session_id().to_owned()).is_err() {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        let bridge = shutdown_subagent_bridge(&mut subagents).await;
        let shutdown = shutdown
            .err()
            .map(|error| format!("; candidate ACP host shutdown failed: {error}"))
            .unwrap_or_default();
        let bridge = bridge
            .map(|error| format!("; ACP subagent projector shutdown failed: {error}"))
            .unwrap_or_default();
        return Err(format!(
            "ACP detached-turn observer root was already bound{shutdown}{bridge}"
        ));
    }
    if let Err(error) = host.activate_extension_composition() {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        let bridge = shutdown_subagent_bridge(&mut subagents).await;
        return Err(match shutdown {
            Ok(()) if bridge.is_none() => error,
            Ok(()) => format!(
                "{error}; ACP subagent projector shutdown also failed: {}",
                bridge.expect("checked")
            ),
            Err(shutdown) => {
                let bridge = bridge
                    .map(|bridge| format!("; ACP subagent projector shutdown failed: {bridge}"))
                    .unwrap_or_default();
                format!("{error}; candidate ACP host shutdown also failed: {shutdown}{bridge}")
            }
        });
    }
    host.push_notes(notes);
    if let Err(error) = host.materialize_session() {
        let shutdown = host.shutdown().await;
        if let Some(mcp) = mcp.take() {
            mcp.shutdown().await;
        }
        let bridge = shutdown_subagent_bridge(&mut subagents).await;
        return Err(match shutdown {
            Ok(()) if bridge.is_none() => error,
            Ok(()) => format!(
                "{error}; ACP subagent projector shutdown also failed: {}",
                bridge.expect("checked")
            ),
            Err(shutdown) => {
                let bridge = bridge
                    .map(|bridge| format!("; ACP subagent projector shutdown failed: {bridge}"))
                    .unwrap_or_default();
                format!("{error}; materialization cleanup also failed: {shutdown}{bridge}")
            }
        });
    }
    let goals = host.goal_store();
    let human_requests = goals.human_requests();
    permission_asker.attach_durable(human_requests.clone(), Arc::clone(&goals));
    if let Some(question_asker) = &question_asker {
        question_asker.attach_durable(human_requests, goals);
    }
    host.activate_background_notifications(&tokio::runtime::Handle::current());
    let subagent_flush = subagents
        .as_ref()
        .map(super::acp_subagent::AcpSubagentBridge::flush_handle);
    Ok(SessionResources {
        host,
        mcp,
        subagents,
        subagent_flush,
        question_asker,
        permission_asker,
        configuration,
    })
}

async fn shutdown_session_resources(mut resources: SessionResources) -> Result<(), String> {
    let host = resources.host.shutdown().await;
    let subagents = shutdown_subagent_bridge(&mut resources.subagents).await;
    if let Some(mcp) = resources.mcp.take() {
        mcp.shutdown().await;
    }
    match (host, subagents) {
        (Ok(()), None) => Ok(()),
        (Err(host), None) => Err(host),
        (Ok(()), Some(subagents)) => Err(subagents),
        (Err(host), Some(subagents)) => Err(format!(
            "{host}; ACP subagent projector shutdown failed: {subagents}"
        )),
    }
}

async fn shutdown_subagent_bridge(
    bridge: &mut Option<super::acp_subagent::AcpSubagentBridge>,
) -> Option<String> {
    match bridge.take() {
        Some(bridge) => bridge.shutdown().await.err(),
        None => None,
    }
}

fn persist_dormant_configuration(
    session_id: &str,
    plan: &TurnPlan,
    persistence: ConfigurationPersistence,
) -> Result<(), zuno_acp::RpcError> {
    if matches!(persistence, ConfigurationPersistence::None) {
        return Ok(());
    }
    let pool = durable_pool()?;
    let store = zuno_db::session::Store::new(&pool);
    let message_id = match persistence {
        ConfigurationPersistence::None => unreachable!("handled above"),
        ConfigurationPersistence::Agent => format!("msg_agent_{}", uuid::Uuid::new_v4().simple()),
        ConfigurationPersistence::Model => format!("msg_model_{}", uuid::Uuid::new_v4().simple()),
    };
    let now = zuno_db::message::now_millis();
    match persistence {
        ConfigurationPersistence::None => Ok(()),
        ConfigurationPersistence::Agent => {
            store.switch_agent_at(session_id, &message_id, plan.agent_name(), now)
        }
        ConfigurationPersistence::Model => {
            let qualified = plan.qualified_model();
            let (provider_id, model_id) = qualified.split_once('/').ok_or_else(|| {
                zuno_acp::RpcError::internal(format!(
                    "resolved model `{qualified}` is not provider-qualified"
                ))
            })?;
            store.switch_model_at(
                session_id,
                &message_id,
                &zuno_db::session::model_reference(provider_id, model_id),
                now,
            )
        }
    }
    .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))
}

impl AcpSession {
    async fn delete_durable(
        &self,
        cleanup_derived_experiences: bool,
    ) -> Result<super::turn::SessionDeleteOutcome, zuno_acp::RpcError> {
        let _mount = self.mount_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        if self.prompt_active.load(Ordering::Acquire) {
            return Err(zuno_acp::RpcError::invalid_params(format!(
                "session {} has an active prompt and cannot be deleted",
                self.id
            )));
        }
        let resources = self.resources.lock().await;
        match resources.as_ref() {
            Some(resources) => resources
                .host
                .delete_session(&self.id, cleanup_derived_experiences)
                .map_err(zuno_acp::RpcError::internal),
            None if cleanup_derived_experiences => {
                Err(zuno_acp::RpcError::invalid_params(format!(
                    "session {} is dormant; load it before choosing derived-learning cleanup",
                    self.id
                )))
            }
            None => {
                let pool = durable_pool()?;
                Ok(super::turn::SessionDeleteOutcome {
                    deleted_session_ids: zuno_db::session::Store::new(&pool)
                        .remove(&self.id)
                        .map_err(|error| map_session_lookup(&self.id, error))?,
                    ..super::turn::SessionDeleteOutcome::default()
                })
            }
        }
    }

    async fn lifecycle_response(&self) -> Result<Value, zuno_acp::RpcError> {
        Ok(self.current_configuration().await?.lifecycle_response())
    }

    fn closed_error(&self) -> zuno_acp::RpcError {
        zuno_acp::RpcError::invalid_params(format!("session {} is closed", self.id))
    }

    async fn restore_after_reconfiguration_failure(
        &self,
        slot: &mut Option<SessionResources>,
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
                AcpSurfaceContext::from_state(state, client),
                Some(build_agent),
                &self.mcp_servers,
            )
            .await?;
            if resources.host.session_id() == self.id {
                return Ok(resources);
            }
            let actual = resources.host.session_id().to_owned();
            let cleanup = shutdown_session_resources(resources).await;
            Err(match cleanup {
                Ok(()) => format!(
                    "rollback produced ACP session {actual}, expected {}",
                    self.id
                ),
                Err(cleanup) => format!(
                    "rollback produced ACP session {actual}, expected {}; rollback cleanup failed: {cleanup}",
                    self.id
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
                "{cause}; rollback failed and session {} is closed: {rollback}",
                self.id
            )),
        }
    }

    async fn current_configuration(&self) -> Result<SessionConfiguration, zuno_acp::RpcError> {
        let _mount = self.mount_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        let resources = self.resources.lock().await;
        if let Some(resources) = resources.as_ref() {
            return Ok(resources.configuration.clone());
        }
        drop(resources);
        self.dormant
            .lock()
            .await
            .as_ref()
            .map(|dormant| dormant.configuration.clone())
            .ok_or_else(|| self.closed_error())
    }

    async fn defer_available_commands(
        &self,
        client: &zuno_acp::ClientConnection,
    ) -> Result<(), zuno_acp::RpcError> {
        let update = {
            let _mount = self.mount_gate.lock().await;
            if self.closed.load(Ordering::Acquire) {
                return Err(self.closed_error());
            }
            let resources = self.resources.lock().await;
            if let Some(resources) = resources.as_ref() {
                available_commands_update(resources.host.commands(), resources.host.slash_skills())
            } else {
                drop(resources);
                self.dormant
                    .lock()
                    .await
                    .as_ref()
                    .map(|dormant| dormant.available_commands.clone())
                    .ok_or_else(|| self.closed_error())?
            }
        };
        client.session_update_after_response(&self.id, update)
    }

    async fn reconfigure(
        &self,
        change: SessionReconfiguration,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
    ) -> Result<SessionConfiguration, zuno_acp::RpcError> {
        self.reconfigure_inner(change, state, client, false).await
    }

    async fn reconfigure_from_prompt(
        &self,
        change: SessionReconfiguration,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
    ) -> Result<SessionConfiguration, zuno_acp::RpcError> {
        self.reconfigure_inner(change, state, client, true).await
    }

    async fn reconfigure_inner(
        &self,
        change: SessionReconfiguration,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
        prompt_owns_session: bool,
    ) -> Result<SessionConfiguration, zuno_acp::RpcError> {
        if !prompt_owns_session && self.prompt_active.load(Ordering::Acquire) {
            return Err(zuno_acp::RpcError::invalid_params(
                "session configuration cannot change while a prompt is active",
            ));
        }
        let mount = self.mount_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        let mut dormant = self.dormant.lock().await;
        if let Some(current) = dormant.as_mut() {
            let configuration = self.reconfigure_dormant(current, change, state).await?;
            drop(dormant);
            drop(mount);
            self.defer_available_commands(&client).await?;
            return Ok(configuration);
        }
        drop(dormant);

        let _composition = state.composition_gate.lock().await;
        let mut slot = self.resources.lock().await;
        let current = slot.take().ok_or_else(|| self.closed_error())?;
        let prepared = match current.configuration.prepare_reconfiguration(
            live_options(&current.host),
            current.host.effort_override(),
            change,
        ) {
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
            AcpSurfaceContext::from_state(state, client.clone()),
            Some(&build_agent),
            &self.mcp_servers,
        )
        .await
        {
            Ok(candidate) => candidate,
            Err(error) => {
                return Err(self
                    .restore_after_reconfiguration_failure(
                        &mut slot,
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
            return Err(self
                .restore_after_reconfiguration_failure(
                    &mut slot,
                    rollback_options,
                    state,
                    client,
                    &build_agent,
                    cause,
                )
                .await);
        }
        let persistence = match prepared.persistence {
            ConfigurationPersistence::None => Ok(()),
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
            return Err(self
                .restore_after_reconfiguration_failure(
                    &mut slot,
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
        drop(slot);
        drop(_composition);
        drop(mount);
        self.defer_available_commands(&client).await?;
        Ok(configuration)
    }

    async fn reconfigure_dormant(
        &self,
        current: &mut DormantSession,
        change: SessionReconfiguration,
        state: &AcpState,
    ) -> Result<SessionConfiguration, zuno_acp::RpcError> {
        let Some(prepared) = current.configuration.prepare_reconfiguration(
            current.options.clone(),
            current.options.effort,
            change,
        )?
        else {
            return Ok(current.configuration.clone());
        };
        let _composition = state.composition_gate.lock().await;
        let plan = TurnPlan::resolve(&prepared.options, &state.environment)
            .await
            .map_err(zuno_acp::RpcError::invalid_params)?;
        let configuration =
            SessionConfiguration::from_plan(&plan, Some(&current.configuration.build_agent));
        let available_commands = available_commands_for_plan(&plan, state.environment.resolved())
            .map_err(zuno_acp::RpcError::internal)?;
        persist_dormant_configuration(&self.id, &plan, prepared.persistence)?;
        current.options = prepared.options;
        current.configuration = configuration.clone();
        current.available_commands = available_commands;
        Ok(configuration)
    }

    async fn prompt(
        &self,
        mut prompt: AcpPrompt,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let native_session = prompt.slash_text().and_then(resolve_session_slash_prompt);
        let _active = ActivePrompt::begin(&self.prompt_active)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        if let Some(SlashInvocation::Session { command, arguments }) = native_session
            && command.is_mode_control()
        {
            validate_session_command_arguments(command, &arguments)?;
            return self.execute_mode_command(command, state, client).await;
        }
        let activated = self.ensure_active(state, client.clone()).await?;
        if activated {
            self.defer_available_commands(&client).await?;
        }
        {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| self.closed_error())?;
            prompt.admit_images(resources.host.attachment_store().as_ref())?;
        }
        self.recover_pending_human_requests(&client).await?;
        let (context_size, invocation) = {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| self.closed_error())?;
            let invocation = prompt.slash_text().and_then(|text| {
                resolve_slash_prompt(
                    text,
                    resources.host.commands(),
                    &resources.host.slash_skills(),
                )
            });
            (resources.configuration.context_size, invocation)
        };
        let skill_selection_only = matches!(
            &invocation,
            Some(SlashInvocation::Skill { arguments, .. }) if arguments.is_empty()
        );
        if let Some(SlashInvocation::Session { command, arguments }) = &invocation
            && !command.accepts_arguments()
            && !arguments.is_empty()
        {
            return Err(zuno_acp::RpcError::invalid_params(format!(
                "/{} does not accept arguments",
                command.name()
            )));
        }
        let (events, receiver) = event_channel();
        let drive = async {
            let mut resources = self.resources.lock().await;
            let resources = resources.as_mut().ok_or_else(|| self.closed_error())?;
            let outcome = match &invocation {
                Some(SlashInvocation::Session { command, arguments }) => match command {
                    SessionCommand::Compact
                    | SessionCommand::Goal
                    | SessionCommand::Learn
                    | SessionCommand::Reflect => resources
                        .host
                        .execute_session_command(*command, arguments, events.clone())
                        .await
                        .map_err(session_command_rpc_error),
                    SessionCommand::Plan
                    | SessionCommand::StartPlan
                    | SessionCommand::StartWork => Err(zuno_acp::RpcError::internal(format!(
                        "/{} mode control was not handled before host execution",
                        command.name()
                    ))),
                },
                Some(SlashInvocation::Command { name, arguments }) => resources
                    .host
                    .drive_command(name, arguments, events.clone())
                    .await
                    .map_err(zuno_acp::RpcError::internal),
                Some(SlashInvocation::Skill {
                    name,
                    source,
                    arguments,
                }) => resources
                    .host
                    .drive_skill(name, source, arguments, events.clone())
                    .await
                    .map_err(zuno_acp::RpcError::internal),
                None => resources
                    .host
                    .drive_content(&prompt.text, &prompt.content, events.clone())
                    .await
                    .map_err(zuno_acp::RpcError::internal),
            };
            drop(events);
            outcome
        };
        let projection = project_turn(&self.id, context_size, receiver, client.clone());
        let (driven, projected) = tokio::join!(drive, projection);
        let projected = projected?;
        self.flush_subagents().await?;
        self.project_work_state(&client).await?;
        let mut driven = driven;
        let mut projected = projected;
        loop {
            match projected {
                ProjectedTurn::Completed(stop_reason) => {
                    driven?;
                    if let Some(next) = self.drive_goal_continuation(&client).await? {
                        (driven, projected) = next;
                        continue;
                    }
                    return Ok(json!({ "stopReason": stop_reason }));
                }
                ProjectedTurn::WaitingForHuman(request_id) => {
                    driven?;
                    if !self.answer_pending_human_request(&request_id).await? {
                        return Ok(json!({ "stopReason": "end_turn" }));
                    }
                    let Some(next) = self.drive_next_durable_input(&client).await? else {
                        return Err(zuno_acp::RpcError::internal(format!(
                            "answered human request `{request_id}` did not admit durable input"
                        )));
                    };
                    (driven, projected) = next;
                }
                ProjectedTurn::Interrupted => {
                    return Ok(json!({ "stopReason": "cancelled" }));
                }
                ProjectedTurn::Failed(message) => {
                    return match driven {
                        Err(error) => Err(error),
                        Ok(()) => Err(zuno_acp::RpcError::internal(message)),
                    };
                }
                ProjectedTurn::Missing => {
                    return match driven {
                        Ok(()) if skill_selection_only => Ok(json!({ "stopReason": "end_turn" })),
                        Ok(()) => Err(zuno_acp::RpcError::internal(
                            "turn ended without a terminal durable event",
                        )),
                        Err(error) => Err(error),
                    };
                }
            }
        }
    }

    async fn answer_pending_human_request(
        &self,
        request_id: &str,
    ) -> Result<bool, zuno_acp::RpcError> {
        let (request, question, permission) = {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| self.closed_error())?;
            let request = resources
                .host
                .goal_store()
                .human_requests()
                .get(request_id)
                .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
                .ok_or_else(|| {
                    zuno_acp::RpcError::internal(format!(
                        "human request `{request_id}` disappeared before presentation"
                    ))
                })?;
            (
                request,
                resources.question_asker.as_ref().map(Arc::clone),
                Arc::clone(&resources.permission_asker),
            )
        };
        match request.kind {
            zuno_db::human_request::HumanRequestKind::Input => question
                .ok_or_else(|| {
                    zuno_acp::RpcError::internal(
                        "ACP client does not advertise form elicitation for pending Goal input",
                    )
                })?
                .answer_pending(request_id)
                .await
                .map_err(|error| zuno_acp::RpcError::internal(error.to_string())),
            zuno_db::human_request::HumanRequestKind::Permission => permission
                .answer_pending(request_id)
                .await
                .map_err(|error| zuno_acp::RpcError::internal(error.to_string())),
        }
    }

    async fn recover_pending_human_requests(
        &self,
        client: &zuno_acp::ClientConnection,
    ) -> Result<(), zuno_acp::RpcError> {
        loop {
            let pending_request_id = {
                let resources = self.resources.lock().await;
                let resources = resources.as_ref().ok_or_else(|| self.closed_error())?;
                resources
                    .host
                    .goal_store()
                    .human_requests()
                    .pending(Some(resources.host.session_id()))
                    .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
                    .into_iter()
                    .next()
                    .map(|request| request.id)
            };
            if let Some(request_id) = pending_request_id
                && !self.answer_pending_human_request(&request_id).await?
            {
                continue;
            }

            let Some((driven, projected)) = self.drive_next_durable_input(client).await? else {
                return Ok(());
            };
            match projected {
                ProjectedTurn::Completed(_) => driven?,
                ProjectedTurn::WaitingForHuman(request_id) => {
                    driven?;
                    if !self.answer_pending_human_request(&request_id).await? {
                        return Ok(());
                    }
                }
                ProjectedTurn::Interrupted => return Ok(()),
                ProjectedTurn::Failed(message) => {
                    let error = driven.err().map_or(message, |error| error.message);
                    return Err(zuno_acp::RpcError::internal(error));
                }
                ProjectedTurn::Missing => {
                    driven?;
                    return Err(zuno_acp::RpcError::internal(
                        "recovered durable input ended without a terminal event",
                    ));
                }
            }
        }
    }

    async fn drive_next_durable_input(
        &self,
        client: &zuno_acp::ClientConnection,
    ) -> Result<Option<(Result<(), zuno_acp::RpcError>, ProjectedTurn)>, zuno_acp::RpcError> {
        let (input_id, text, context_size) = {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| self.closed_error())?;
            let inbox = resources.host.session_inbox();
            let Some(input) = inbox
                .pending(resources.host.session_id())
                .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
                .into_iter()
                .next()
            else {
                return Ok(None);
            };
            let text = durable_input_text(&input)?;
            let promoted = inbox
                .promote_id(resources.host.session_id(), &input.id)
                .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
                .ok_or_else(|| {
                    zuno_acp::RpcError::internal(format!(
                        "durable input `{}` changed before ACP promotion",
                        input.id
                    ))
                })?;
            (promoted.id, text, resources.configuration.context_size)
        };
        let (events, receiver) = event_channel();
        let drive = async {
            let mut resources = self.resources.lock().await;
            let resources = resources.as_mut().ok_or_else(|| self.closed_error())?;
            let outcome = resources
                .host
                .drive_promoted(&text, &input_id, events.clone())
                .await;
            drop(events);
            outcome.map_err(zuno_acp::RpcError::internal)
        };
        let projection = project_turn(&self.id, context_size, receiver, client.clone());
        let (driven, projected) = tokio::join!(drive, projection);
        Ok(Some((driven, projected?)))
    }

    async fn drive_goal_continuation(
        &self,
        client: &zuno_acp::ClientConnection,
    ) -> Result<Option<(Result<(), zuno_acp::RpcError>, ProjectedTurn)>, zuno_acp::RpcError> {
        loop {
            let context_size = {
                let resources = self.resources.lock().await;
                resources
                    .as_ref()
                    .ok_or_else(|| self.closed_error())?
                    .configuration
                    .context_size
            };
            let (events, receiver) = event_channel();
            let drive = async {
                let mut resources = self.resources.lock().await;
                let resources = resources.as_mut().ok_or_else(|| self.closed_error())?;
                let continued = resources
                    .host
                    .continue_goal_if_idle(zuno_goal::QueuedUserInput::Absent, events.clone())
                    .await
                    .map_err(zuno_acp::RpcError::internal)?;
                drop(events);
                Ok::<bool, zuno_acp::RpcError>(continued)
            };
            let projection = project_turn(&self.id, context_size, receiver, client.clone());
            let (continued, projected) = tokio::join!(drive, projection);
            let projected = projected?;
            match (continued, projected) {
                (Ok(false), ProjectedTurn::Missing) => return Ok(None),
                (Ok(true), ProjectedTurn::Missing) => continue,
                (continued, projected) => {
                    return Ok(Some((continued.map(|_| ()), projected)));
                }
            }
        }
    }

    async fn execute_mode_command(
        &self,
        command: SessionCommand,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
    ) -> Result<Value, zuno_acp::RpcError> {
        let current = self.current_configuration().await?;
        let target = match command {
            SessionCommand::Plan if current.mode == "plan" => "build",
            SessionCommand::Plan | SessionCommand::StartPlan => "plan",
            SessionCommand::StartWork => "build",
            SessionCommand::Compact
            | SessionCommand::Goal
            | SessionCommand::Learn
            | SessionCommand::Reflect => {
                return Err(zuno_acp::RpcError::internal(format!(
                    "/{} is not a mode control",
                    command.name()
                )));
            }
        };
        if target == "build" && current.mode == "plan" && !self.has_durable_plan()? {
            return Err(zuno_acp::RpcError::invalid_params(
                "no durable plan is ready; run /start-plan and let the plan Agent create one",
            ));
        }
        let configuration = self
            .reconfigure_from_prompt(
                SessionReconfiguration::Mode(target.to_owned()),
                state,
                client.clone(),
            )
            .await?;
        client
            .session_update(
                &self.id,
                json!({
                    "sessionUpdate": "current_mode_update",
                    "currentModeId": configuration.mode,
                }),
            )
            .await?;
        client
            .session_update(
                &self.id,
                json!({
                    "sessionUpdate": "config_option_update",
                    "configOptions": configuration.config_options(),
                }),
            )
            .await?;
        Ok(json!({ "stopReason": "end_turn" }))
    }

    fn has_durable_plan(&self) -> Result<bool, zuno_acp::RpcError> {
        let store = zuno_tools::WorkStateStore::new(Arc::new(durable_pool()?));
        store
            .plan(&self.id)
            .map(|plan| plan.is_some())
            .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))
    }

    async fn ensure_active(
        &self,
        state: &AcpState,
        client: zuno_acp::ClientConnection,
    ) -> Result<bool, zuno_acp::RpcError> {
        let _mount = self.mount_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        if self.resources.lock().await.is_some() {
            return Ok(false);
        }
        let dormant = self
            .dormant
            .lock()
            .await
            .take()
            .ok_or_else(|| self.closed_error())?;
        let _composition = state.composition_gate.lock().await;
        let plan = match TurnPlan::resolve(&dormant.options, &state.environment).await {
            Ok(plan) => plan,
            Err(error) => {
                *self.dormant.lock().await = Some(dormant);
                return Err(zuno_acp::RpcError::internal(error));
            }
        };
        let resources = match open_session_resources(
            plan,
            &state.environment,
            state.runs.clone(),
            AcpSurfaceContext::from_state(state, client),
            Some(&dormant.configuration.build_agent),
            &self.mcp_servers,
        )
        .await
        {
            Ok(resources) => resources,
            Err(error) => {
                *self.dormant.lock().await = Some(dormant);
                return Err(zuno_acp::RpcError::internal(format!(
                    "could not activate ACP session {}: {error}",
                    self.id
                )));
            }
        };
        if resources.host.session_id() != self.id {
            let actual = resources.host.session_id().to_owned();
            let cleanup = shutdown_session_resources(resources).await;
            *self.dormant.lock().await = Some(dormant);
            let cleanup = cleanup
                .err()
                .map(|error| format!("; candidate cleanup failed: {error}"))
                .unwrap_or_default();
            return Err(zuno_acp::RpcError::internal(format!(
                "activated ACP session {actual}, expected {}{cleanup}",
                self.id
            )));
        }
        *self.resources.lock().await = Some(resources);
        Ok(true)
    }

    async fn project_work_state(
        &self,
        client: &zuno_acp::ClientConnection,
    ) -> Result<(), zuno_acp::RpcError> {
        let updates = {
            let resources = self.resources.lock().await;
            let resources = resources.as_ref().ok_or_else(|| self.closed_error())?;
            let work = resources
                .host
                .work_state()
                .map_err(zuno_acp::RpcError::internal)?;
            zuno_acp::durable_work_updates(&work)
        };
        for update in updates {
            client.session_update(&self.id, update).await?;
        }
        Ok(())
    }

    async fn flush_subagents(&self) -> Result<(), zuno_acp::RpcError> {
        let flush = self
            .resources
            .lock()
            .await
            .as_ref()
            .and_then(|resources| resources.subagent_flush.clone());
        if let Some(flush) = flush {
            flush.flush().await.map_err(zuno_acp::RpcError::internal)?;
        }
        Ok(())
    }

    fn cancel(&self, reason: HardInterruptReason) {
        if self.prompt_active.load(Ordering::Acquire) {
            let _disposition = self
                .control
                .abort(HardInterruptRequest::new(HardInterruptSource::Acp, reason));
        }
    }

    async fn replay(
        &self,
        client: &zuno_acp::ClientConnection,
        native_subagents: bool,
    ) -> Result<(), zuno_acp::RpcError> {
        let _replay = self.replay_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        if self.replayed.load(Ordering::Acquire) {
            return Ok(());
        }
        let configured_context_size = self.current_configuration().await?.context_size;
        let pool = Arc::new(durable_pool()?);
        let stored = zuno_db::session::Store::new(pool.as_ref())
            .get(&self.id)
            .map_err(|error| map_session_lookup(&self.id, error))?;
        let connection = pool
            .open_connection()
            .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
        let mut history = zuno_engine::r#loop::hydrate_retained_history_tail(
            &connection,
            &self.id,
            zuno_acp::REPLAY_MESSAGE_CAP,
            zuno_acp::REPLAY_TRANSCRIPT_BYTE_CAP,
        )
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
        let attachments = self
            .resources
            .lock()
            .await
            .as_ref()
            .map(|resources| resources.host.attachment_store())
            .ok_or_else(|| {
                zuno_acp::RpcError::internal(
                    "active ACP session has no attachment service for durable replay",
                )
            })?;
        hydrate_replay_attachments(&mut history.messages, attachments.as_ref())
            .map_err(zuno_acp::RpcError::internal)?;
        let work_state = replay_work_state(Arc::clone(&pool), &self.id)?;
        let context_size = stored
            .usage
            .context_limit
            .and_then(|limit| u64::try_from(limit).ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(configured_context_size);
        let replay = zuno_acp::durable_updates(
            &history.messages,
            &zuno_acp::ReplayPolicy::for_workspace(&self.replay_root),
            history.omitted,
        );
        for update in replay.updates {
            if self.closed.load(Ordering::Acquire) {
                return Err(self.closed_error());
            }
            client.session_update(&self.id, update).await?;
        }
        for update in zuno_acp::durable_work_updates(&work_state) {
            if self.closed.load(Ordering::Acquire) {
                return Err(self.closed_error());
            }
            client.session_update(&self.id, update).await?;
        }
        if let Some(update) =
            zuno_acp::durable_usage_update(&history.messages, context_size, stored.usage.cost)
        {
            if self.closed.load(Ordering::Acquire) {
                return Err(self.closed_error());
            }
            client.session_update(&self.id, update).await?;
        }
        if native_subagents {
            replay_child_sessions(
                client,
                Arc::clone(&pool),
                &self.id,
                &self.replay_root,
                configured_context_size,
                Arc::clone(&attachments),
            )
            .await?;
        }
        self.replayed.store(true, Ordering::Release);
        Ok(())
    }

    async fn mark_replay_satisfied(&self) -> Result<(), zuno_acp::RpcError> {
        let _replay = self.replay_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        self.replayed.store(true, Ordering::Release);
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), String> {
        self.closed.store(true, Ordering::Release);
        if self.prompt_active.load(Ordering::Acquire) {
            let _disposition = self.control.abort(HardInterruptRequest::new(
                HardInterruptSource::Lifecycle,
                HardInterruptReason::SessionClose,
            ));
        } else {
            let _aborted = self.control.abort_active(HardInterruptRequest::new(
                HardInterruptSource::Lifecycle,
                HardInterruptReason::SessionClose,
            ));
        }
        let _replay = self.replay_gate.lock().await;
        let _mount = self.mount_gate.lock().await;
        let notification_task = self
            .background_notifications
            .unregister(&self.background_notification_directory, &self.id);
        let _disposition = self.control.abort(HardInterruptRequest::new(
            HardInterruptSource::Lifecycle,
            HardInterruptReason::SessionClose,
        ));
        self.control.wait_until_idle().await;
        let notification_error = match notification_task {
            Some(task) => task
                .await
                .err()
                .map(|error| format!("background notification watcher failed: {error}")),
            None => None,
        };
        self.control.wait_until_idle().await;
        self.dormant.lock().await.take();
        let resources = self.resources.lock().await.take();
        let resources_result = if let Some(resources) = resources {
            let (background_jobs, session_id) = resources.host.background_job_scope();
            background_jobs.cancel_for_parent(&session_id);
            background_jobs.wait_for_parent(&session_id).await;
            shutdown_session_resources(resources).await
        } else {
            Ok(())
        };
        let _cleared = self.control.clear_pending_abort();
        match (resources_result, notification_error) {
            (Ok(()), None) => Ok(()),
            (Err(error), None) => Err(error),
            (Ok(()), Some(notification)) => Err(notification),
            (Err(error), Some(notification)) => Err(format!("{error}; {notification}")),
        }
    }
}

fn session_command_rpc_error(error: SessionCommandError) -> zuno_acp::RpcError {
    if error.is_invalid_arguments() {
        zuno_acp::RpcError::invalid_params(error.to_string())
    } else {
        zuno_acp::RpcError::internal(error.to_string())
    }
}

fn replay_work_state(
    pool: Arc<zuno_db::pool::Pool>,
    session_id: &str,
) -> Result<zuno_types::WorkStateProjection, zuno_acp::RpcError> {
    let session = zuno_db::session::Store::new(pool.as_ref())
        .get(session_id)
        .map_err(|error| map_session_lookup(session_id, error))?;
    let snapshot = zuno_tools::work_state::WorkStateStore::new(Arc::clone(&pool))
        .snapshot(session_id)
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    let plan = snapshot.plan.map(|plan| zuno_types::PlanProjection {
        id: plan.id,
        goal_id: plan.goal_id,
        revision: plan.revision,
        title: plan.title,
        steps: plan
            .steps
            .into_iter()
            .map(|step| zuno_types::PlanStepProjection {
                id: step.id,
                title: step.title,
                status: step.status.as_str().to_owned(),
            })
            .collect(),
        span: zuno_types::ExecutionSpan::default(),
        time_created: plan.time_created,
        time_updated: plan.time_updated,
    });
    let todos = snapshot
        .items
        .into_iter()
        .map(|item| zuno_types::TodoProjection {
            id: item.id,
            goal_id: item.goal_id,
            plan_step_id: item.plan_step_id,
            parent_id: item.parent_id,
            subject: item.subject,
            description: item.description,
            active_form: item.active_form,
            status: item.status.as_str().to_owned(),
            priority: item.priority.as_str().to_owned(),
            dependencies: item.dependencies,
            owner: item.owner,
            revision: item.revision,
            span: zuno_types::ExecutionSpan::default(),
            time_created: item.time_created,
            time_updated: item.time_updated,
        })
        .collect();
    let learning = zuno_learning::LearningProjectionService::new(pool)
        .snapshot(session_id, &session.project_id)
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    Ok(zuno_types::WorkStateProjection {
        plan,
        todos,
        learning,
        ..zuno_types::WorkStateProjection::default()
    })
}

async fn replay_child_sessions(
    client: &zuno_acp::ClientConnection,
    pool: Arc<zuno_db::pool::Pool>,
    root_session_id: &str,
    replay_root: &std::path::Path,
    default_context_size: u64,
    attachments: Arc<zuno_attachment::AttachmentStore>,
) -> Result<(), zuno_acp::RpcError> {
    let store = zuno_db::session::Store::new(pool.as_ref());
    let mut pending = VecDeque::from([root_session_id.to_owned()]);
    let mut seen = BTreeSet::from([root_session_id.to_owned()]);
    let mut children = Vec::new();
    while let Some(parent_session_id) = pending.pop_front() {
        for child in store
            .children(&parent_session_id)
            .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?
        {
            if !seen.insert(child.id.clone()) {
                continue;
            }
            pending.push_back(child.id.clone());
            children.push((parent_session_id.clone(), child));
        }
    }
    if children.is_empty() {
        return Ok(());
    }

    let connection = pool
        .open_connection()
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
    for (parent_session_id, child) in &children {
        client
            .session_update(
                parent_session_id,
                json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagentSessionId": child.id,
                    "name": child.agent.as_deref().unwrap_or("subagent"),
                    "task": child.title,
                    "capabilities": {
                        "cancel": false,
                        "close": false,
                    },
                }),
            )
            .await?;
        let mut history = zuno_engine::r#loop::hydrate_retained_history_tail(
            &connection,
            &child.id,
            zuno_acp::REPLAY_MESSAGE_CAP,
            zuno_acp::REPLAY_TRANSCRIPT_BYTE_CAP,
        )
        .map_err(|error| zuno_acp::RpcError::internal(error.to_string()))?;
        hydrate_replay_attachments(&mut history.messages, attachments.as_ref())
            .map_err(zuno_acp::RpcError::internal)?;
        let replay = zuno_acp::durable_updates(
            &history.messages,
            &zuno_acp::ReplayPolicy::for_workspace(replay_root),
            history.omitted,
        );
        for update in replay.updates {
            client.session_update(&child.id, update).await?;
        }
        let work_state = replay_work_state(Arc::clone(&pool), &child.id)?;
        for update in zuno_acp::durable_work_updates(&work_state) {
            client.session_update(&child.id, update).await?;
        }
        let context_size = child
            .usage
            .context_limit
            .and_then(|limit| u64::try_from(limit).ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(default_context_size);
        if let Some(update) =
            zuno_acp::durable_usage_update(&history.messages, context_size, child.usage.cost)
        {
            client.session_update(&child.id, update).await?;
        }
    }
    for (parent_session_id, child) in children.iter().rev() {
        client
            .session_update(
                parent_session_id,
                json!({
                    "sessionUpdate": "subagent_state_update",
                    "subagentSessionId": child.id,
                    "state": "disconnected",
                }),
            )
            .await?;
    }
    Ok(())
}

pub(super) fn hydrate_replay_attachments(
    messages: &mut [zuno_db::message::MessageWithParts],
    store: &zuno_attachment::AttachmentStore,
) -> Result<(), String> {
    for message in messages {
        for part in &mut message.parts {
            if part.kind != zuno_db::message::PartKind::File {
                continue;
            }
            let Some(reference) = part.data.get("attachment").cloned() else {
                continue;
            };
            let reference = serde_json::from_value::<zuno_attachment::ImageAttachmentRef>(
                reference,
            )
            .map_err(|error| {
                format!(
                    "durable image part `{}` has an invalid attachment reference: {error}",
                    part.id
                )
            })?;
            let bytes = store.read(&reference).map_err(|error| {
                format!(
                    "durable image part `{}` cannot resolve attachment {}: {error}",
                    part.id, reference.id
                )
            })?;
            part.data
                .insert("mime".to_owned(), json!(reference.media_type));
            part.data.insert(
                "data".to_owned(),
                json!(base64::engine::general_purpose::STANDARD.encode(bytes)),
            );
        }
    }
    Ok(())
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
        variant: None,
        thinking: false,
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
    effective_effort: Option<zuno_llm::effort::ReasoningEffort>,
    effort_override: Option<zuno_llm::effort::ReasoningEffort>,
    reasoning_efforts: HashMap<String, Vec<zuno_llm::effort::ReasoningEffort>>,
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
        let model = plan.qualified_model();
        let reasoning_efforts = plan
            .catalog_models()
            .iter()
            .map(|choice| (choice.id.clone(), plan.model_reasoning_efforts(&choice.id)))
            .collect();
        Self {
            mode,
            context_size: plan.context_window(),
            active_agent,
            build_agent,
            agents,
            model,
            models: plan.catalog_models(),
            effective_effort: plan.effort(),
            effort_override: plan.effort_override(),
            reasoning_efforts,
        }
    }

    fn prepare_reconfiguration(
        &self,
        mut options: TurnOptions,
        current_effort_override: Option<zuno_llm::effort::ReasoningEffort>,
        change: SessionReconfiguration,
    ) -> Result<Option<PreparedReconfiguration>, zuno_acp::RpcError> {
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
                if options.effort.is_some_and(|effort| {
                    !self
                        .reasoning_efforts
                        .get(&model)
                        .is_some_and(|efforts| efforts.contains(&effort))
                }) {
                    options.effort = None;
                }
                options.model = Some(model);
                ConfigurationPersistence::Model
            }
            SessionReconfiguration::Reasoning(value) => {
                let effort = if value == "default" {
                    None
                } else {
                    let effort = value.parse().map_err(|error| {
                        zuno_acp::RpcError::invalid_params(format!(
                            "invalid ACP thought level {value}: {error}"
                        ))
                    })?;
                    let available = self
                        .reasoning_efforts
                        .get(&self.model)
                        .cloned()
                        .unwrap_or_default();
                    if !available.contains(&effort) {
                        return Err(zuno_acp::RpcError::invalid_params(format!(
                            "thought level {value} is not available for {}; expected one of: {}",
                            self.model,
                            available
                                .iter()
                                .map(|effort| effort.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                    Some(effort)
                };
                if current_effort_override == effort {
                    return Ok(None);
                }
                options.effort = effort;
                ConfigurationPersistence::None
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
        let mut options = vec![
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
        ];
        let efforts = self
            .reasoning_efforts
            .get(&self.model)
            .cloned()
            .unwrap_or_default();
        if !efforts.is_empty() {
            let default_description = self.effective_effort.map_or_else(
                || "Use the selected Agent and model defaults.".to_owned(),
                |effort| {
                    format!(
                        "Use the selected Agent and model defaults (currently {}).",
                        reasoning_effort_name(effort)
                    )
                },
            );
            let mut effort_options = vec![json!({
                "value": "default",
                "name": "Configured default",
                "description": default_description,
            })];
            effort_options.extend(efforts.iter().map(|effort| {
                json!({
                    "value": effort.as_str(),
                    "name": reasoning_effort_name(*effort),
                })
            }));
            let current = self
                .effort_override
                .filter(|effort| efforts.contains(effort))
                .map_or("default", zuno_llm::effort::ReasoningEffort::as_str);
            options.push(json!({
                "id": "reasoning_effort",
                "name": "Reasoning",
                "description": "Choose the reasoning effort supported by the active model.",
                "category": "thought_level",
                "type": "select",
                "currentValue": current,
                "options": effort_options,
            }));
        }
        options
    }

    #[cfg(test)]
    fn for_model(
        &self,
        model: &str,
        effective_effort: Option<zuno_llm::effort::ReasoningEffort>,
    ) -> Self {
        Self {
            model: model.to_owned(),
            effective_effort,
            effort_override: None,
            ..self.clone()
        }
    }
}

const fn reasoning_effort_name(effort: zuno_llm::effort::ReasoningEffort) -> &'static str {
    match effort {
        zuno_llm::effort::ReasoningEffort::Off => "Off",
        zuno_llm::effort::ReasoningEffort::Low => "Low",
        zuno_llm::effort::ReasoningEffort::Medium => "Medium",
        zuno_llm::effort::ReasoningEffort::High => "High",
        zuno_llm::effort::ReasoningEffort::Xhigh => "Extra High",
        zuno_llm::effort::ReasoningEffort::Max => "Maximum",
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

#[derive(Debug)]
struct AcpPrompt {
    text: String,
    content: Vec<RequestContentBlock>,
}

impl AcpPrompt {
    fn slash_text(&self) -> Option<&str> {
        match self.content.as_slice() {
            [RequestContentBlock::Text { text }] => Some(text),
            _ => None,
        }
    }

    fn admit_images(
        &mut self,
        store: &zuno_attachment::AttachmentStore,
    ) -> Result<(), zuno_acp::RpcError> {
        for block in &mut self.content {
            match block {
                RequestContentBlock::Image {
                    filename,
                    media_type,
                    data,
                } => {
                    let reference = store
                        .admit_base64_typed(data, Some(media_type), filename.clone())
                        .map_err(|error| {
                            zuno_acp::RpcError::invalid_params(format!(
                                "image admission failed: {error}"
                            ))
                        })?;
                    *block = RequestContentBlock::ImageAttachment { reference };
                }
                RequestContentBlock::ImageAttachment { reference } => {
                    store.read(reference).map_err(|error| {
                        zuno_acp::RpcError::invalid_params(format!(
                            "durable image attachment is invalid: {error}"
                        ))
                    })?;
                }
                _ => {}
            }
        }
        self.text = render_acp_prompt(&self.content);
        Ok(())
    }
}

fn parse_prompt(params: &Value) -> Result<AcpPrompt, zuno_acp::RpcError> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| zuno_acp::RpcError::invalid_params("prompt must be an array"))?;
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
            "image" => parse_image_block(block)?,
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
            "resource" => parse_embedded_resource(block)?,
            other => {
                return Err(zuno_acp::RpcError::invalid_params(format!(
                    "prompt block type {other} is not advertised by this ACP adapter"
                )));
            }
        };
        if resolved
            .provider_text()
            .is_some_and(|text| !text.is_empty())
            || matches!(resolved, RequestContentBlock::Image { .. })
        {
            content.push(resolved);
        }
    }
    if content.is_empty() {
        return Err(zuno_acp::RpcError::invalid_params(
            "prompt must contain text, an image, or a resource",
        ));
    }
    Ok(AcpPrompt {
        text: render_acp_prompt(&content),
        content,
    })
}

fn render_acp_prompt(content: &[RequestContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| {
            if let Some(text) = block.provider_text().filter(|text| !text.is_empty()) {
                return Some(text.into_owned());
            }
            match block {
                RequestContentBlock::Image {
                    filename,
                    media_type,
                    ..
                } => Some(filename.as_ref().map_or_else(
                    || format!("Attached image ({media_type})"),
                    |filename| format!("Attached image: {filename} ({media_type})"),
                )),
                RequestContentBlock::ImageAttachment { reference } => {
                    Some(reference.filename.as_ref().map_or_else(
                        || format!("Attached image ({})", reference.media_type),
                        |filename| format!("Attached image: {filename} ({})", reference.media_type),
                    ))
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn parse_image_block(block: &Value) -> Result<RequestContentBlock, zuno_acp::RpcError> {
    let media_type = required_non_empty_string(block, "mimeType", "image")?;
    let data = required_non_empty_string(block, "data", "image")?;
    let uri = optional_string(block, "uri")?;
    validated_image(uri.as_deref(), &media_type, data)
}

fn parse_embedded_resource(block: &Value) -> Result<RequestContentBlock, zuno_acp::RpcError> {
    let resource = block
        .get("resource")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(
                "resource prompt blocks must contain a resource object",
            )
        })?;
    let resource = Value::Object(resource.clone());
    let uri = required_non_empty_string(&resource, "uri", "embedded resource")?;
    let media_type = optional_string(&resource, "mimeType")?;
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        validate_text_resource(&uri, text)?;
        let mut rendered = format!("Embedded resource `{uri}`");
        if let Some(media_type) = media_type.as_deref().filter(|value| !value.is_empty()) {
            rendered.push_str(" (");
            rendered.push_str(media_type);
            rendered.push(')');
        }
        rendered.push_str(":\n--- BEGIN EMBEDDED RESOURCE ---\n");
        rendered.push_str(text);
        rendered.push_str("\n--- END EMBEDDED RESOURCE ---");
        return Ok(RequestContentBlock::Text { text: rendered });
    }
    if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
        let media_type = media_type
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                zuno_acp::RpcError::invalid_params(
                    "binary embedded resources must contain a non-empty mimeType",
                )
            })?;
        if !media_type.starts_with("image/") {
            return Err(zuno_acp::RpcError::invalid_params(format!(
                "binary embedded resource {uri} uses unsupported MIME type {media_type}; only images are accepted"
            )));
        }
        return validated_image(Some(&uri), &media_type, blob.to_owned());
    }
    Err(zuno_acp::RpcError::invalid_params(
        "embedded resources must contain either string text or base64 blob content",
    ))
}

fn required_non_empty_string(
    value: &Value,
    field: &str,
    label: &str,
) -> Result<String, zuno_acp::RpcError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            zuno_acp::RpcError::invalid_params(format!(
                "{label} blocks must contain a non-empty {field}"
            ))
        })
}

fn validate_text_resource(uri: &str, text: &str) -> Result<(), zuno_acp::RpcError> {
    if text.len() > ACP_TEXT_RESOURCE_MAX_BYTES {
        return Err(zuno_acp::RpcError::invalid_params(format!(
            "embedded text resource {uri} exceeds the {ACP_TEXT_RESOURCE_MAX_BYTES}-byte limit"
        )));
    }
    let lines = text.lines().count();
    if lines > ACP_TEXT_RESOURCE_MAX_LINES {
        return Err(zuno_acp::RpcError::invalid_params(format!(
            "embedded text resource {uri} has {lines} lines, exceeding the {ACP_TEXT_RESOURCE_MAX_LINES}-line limit"
        )));
    }
    Ok(())
}

fn validated_image(
    uri: Option<&str>,
    media_type: &str,
    data: String,
) -> Result<RequestContentBlock, zuno_acp::RpcError> {
    if !media_type.starts_with("image/") {
        return Err(zuno_acp::RpcError::invalid_params(format!(
            "image block MIME type must start with image/, got {media_type}"
        )));
    }
    Ok(RequestContentBlock::Image {
        filename: uri.and_then(filename_from_uri),
        media_type: media_type.to_owned(),
        data,
    })
}

fn filename_from_uri(uri: &str) -> Option<String> {
    url::Url::parse(uri)
        .ok()
        .and_then(|uri| {
            uri.path_segments()
                .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
                .map(str::to_owned)
        })
        .or_else(|| {
            PathBuf::from(uri)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlashInvocation {
    Session {
        command: SessionCommand,
        arguments: String,
    },
    Command {
        name: String,
        arguments: String,
    },
    Skill {
        name: String,
        source: String,
        arguments: String,
    },
}

fn resolve_slash_prompt<'a>(
    text: &str,
    commands: impl IntoIterator<Item = &'a zuno_catalog::command::Info>,
    skills: &[zuno_catalog::skill::Skill],
) -> Option<SlashInvocation> {
    if let Some(invocation) = resolve_session_slash_prompt(text) {
        return Some(invocation);
    }
    let invocation = text.strip_prefix('/')?;
    let name_end = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());
    let name = &invocation[..name_end];
    if name.is_empty() {
        return None;
    }
    let arguments = invocation[name_end..].trim_start().to_owned();
    if commands
        .into_iter()
        .any(|command| executable_command(command) && command.name == name)
    {
        return Some(SlashInvocation::Command {
            name: name.to_owned(),
            arguments,
        });
    }
    skills
        .iter()
        .find(|skill| skill.name == name)
        .map(|skill| SlashInvocation::Skill {
            name: skill.name.clone(),
            source: skill.location.clone(),
            arguments,
        })
}

fn resolve_session_slash_prompt(text: &str) -> Option<SlashInvocation> {
    let invocation = text.strip_prefix('/')?;
    let name_end = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());
    let name = &invocation[..name_end];
    let command = SessionCommand::from_name(name)?;
    Some(SlashInvocation::Session {
        command,
        arguments: invocation[name_end..].trim_start().to_owned(),
    })
}

fn validate_session_command_arguments(
    command: SessionCommand,
    arguments: &str,
) -> Result<(), zuno_acp::RpcError> {
    if command.accepts_arguments() || arguments.is_empty() {
        return Ok(());
    }
    Err(zuno_acp::RpcError::invalid_params(format!(
        "/{} does not accept arguments",
        command.name()
    )))
}

fn available_commands_for_plan(plan: &TurnPlan, env: &zuno_paths::Env) -> Result<Value, String> {
    let commands = plan.command_registry(env, None)?;
    let skills = plan
        .skills()
        .slash_invokable(commands.list().map(|command| command.name.as_str()))
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    Ok(available_commands_update(commands.list(), skills))
}

fn executable_command(command: &zuno_catalog::command::Info) -> bool {
    command.subtask != Some(true)
        && matches!(command.template, zuno_catalog::command::Template::Text(_))
}

fn available_commands_update<'a>(
    commands: impl IntoIterator<Item = &'a zuno_catalog::command::Info>,
    skills: impl IntoIterator<Item = zuno_catalog::skill::Skill>,
) -> Value {
    let mut available = SessionCommand::ALL
        .into_iter()
        .map(|command| {
            let mut advertised = json!({
                "name": command.name(),
                "description": command.description(),
            });
            if let Some(hint) = command.input_hint() {
                advertised["input"] = json!({ "hint": hint });
            }
            advertised
        })
        .collect::<Vec<_>>();
    available.extend(
        commands
            .into_iter()
            .filter(|command| {
                executable_command(command) && SessionCommand::from_name(&command.name).is_none()
            })
            .map(|command| {
                let mut advertised = json!({
                    "name": command.name,
                    "description": command.description.clone().unwrap_or_else(|| {
                        format!("Run the /{} Zuno command.", command.name)
                    }),
                });
                if !command.hints.is_empty() {
                    advertised["input"] = json!({ "hint": command.hints.join(" ") });
                }
                advertised
            }),
    );
    available.extend(
        skills
            .into_iter()
            .filter(|skill| SessionCommand::from_name(&skill.name).is_none())
            .map(|skill| {
                json!({
                    "name": skill.name,
                    "description": skill.description.unwrap_or_else(|| {
                        "Load this Skill for the current session.".to_owned()
                    }),
                    "input": { "hint": "arguments" },
                })
            }),
    );
    json!({
        "sessionUpdate": "available_commands_update",
        "availableCommands": available,
    })
}

enum ProjectedTurn {
    Completed(&'static str),
    WaitingForHuman(String),
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
    let mut projector =
        zuno_acp::AttemptBufferedTurnEventProjector::with_context_size(context_size);
    let mut finish_reason = None;
    while let Some(event) = receiver.recv().await {
        for update in projector.project(&event) {
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
            TurnEvent::SessionCommandCompleted { .. } => {
                return Ok(ProjectedTurn::Completed("end_turn"));
            }
            TurnEvent::TurnWaitingForHuman { request_id, .. } => {
                return Ok(ProjectedTurn::WaitingForHuman(request_id));
            }
            TurnEvent::TurnInterrupted { .. } => return Ok(ProjectedTurn::Interrupted),
            TurnEvent::SessionCommandFailed { message, .. } => {
                return Ok(ProjectedTurn::Failed(message));
            }
            TurnEvent::TurnFailed { message, .. } => {
                return Ok(ProjectedTurn::Failed(message));
            }
            _ => {}
        }
    }
    for update in projector.finish() {
        client.session_update(session_id, update).await?;
    }
    Ok(ProjectedTurn::Missing)
}

fn durable_input_text(input: &zuno_db::inbox::SessionInput) -> Result<String, zuno_acp::RpcError> {
    if super::background_notification::is_async_notification(&input.prompt)
        || input.prompt.get("kind").and_then(Value::as_str) == Some("humanRequestAnswer")
    {
        return input
            .prompt
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                zuno_acp::RpcError::internal(format!(
                    "durable input `{}` has no string `text`",
                    input.id
                ))
            });
    }
    Err(zuno_acp::RpcError::internal(format!(
        "durable input `{}` has an unsupported ACP prompt shape",
        input.id
    )))
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

fn require_empty_additional_directories(params: &Value) -> Result<(), zuno_acp::RpcError> {
    let Some(value) = params.get("additionalDirectories") else {
        return Ok(());
    };
    let values = value.as_array().ok_or_else(|| {
        zuno_acp::RpcError::invalid_params("additionalDirectories must be an array")
    })?;
    if !values.is_empty() {
        return Err(zuno_acp::RpcError::invalid_params(
            "additional workspace directories are not advertised by this ACP adapter",
        ));
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

fn session_capacity_error() -> zuno_acp::RpcError {
    zuno_acp::RpcError::invalid_params(format!(
        "this ACP connection already has {MAX_OPEN_ACP_SESSIONS} open sessions; close an inactive \
         session before opening another"
    ))
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

fn required_bool(params: &Value, field: &str) -> Result<bool, zuno_acp::RpcError> {
    params
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| zuno_acp::RpcError::invalid_params(format!("missing boolean `{field}`")))
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

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_catalog::command::{Info, Source, Template};
    use zuno_llm::effort::ReasoningEffort;

    fn configuration() -> SessionConfiguration {
        SessionConfiguration {
            mode: "build",
            context_size: 100_000,
            active_agent: "build".to_owned(),
            build_agent: "build".to_owned(),
            agents: vec![AgentChoice {
                name: "build".to_owned(),
                description: Some("Build".to_owned()),
            }],
            model: "test/reasoning".to_owned(),
            models: vec![
                CatalogModelChoice {
                    id: "test/reasoning".to_owned(),
                    name: "Reasoning".to_owned(),
                    provider: "Test".to_owned(),
                },
                CatalogModelChoice {
                    id: "test/plain".to_owned(),
                    name: "Plain".to_owned(),
                    provider: "Test".to_owned(),
                },
            ],
            effective_effort: Some(ReasoningEffort::Xhigh),
            effort_override: Some(ReasoningEffort::Xhigh),
            reasoning_efforts: HashMap::from([
                (
                    "test/reasoning".to_owned(),
                    vec![
                        ReasoningEffort::Low,
                        ReasoningEffort::High,
                        ReasoningEffort::Xhigh,
                        ReasoningEffort::Max,
                    ],
                ),
                ("test/plain".to_owned(), Vec::new()),
            ]),
        }
    }

    fn command(
        name: &str,
        description: Option<&str>,
        template: Template,
        subtask: Option<bool>,
    ) -> Info {
        Info {
            name: name.to_owned(),
            description: description.map(str::to_owned),
            agent: None,
            model: None,
            source: Source::Command,
            template,
            subtask,
            hints: vec!["question".to_owned()],
        }
    }

    #[test]
    fn initialize_advertises_only_the_rich_prompt_handlers_it_implements() {
        let response = initialize(&json!({"protocolVersion": 1})).expect("initialize");
        let capabilities = &response["agentCapabilities"]["promptCapabilities"];
        assert_eq!(capabilities["image"], true);
        assert_eq!(capabilities["embeddedContext"], true);
        assert_eq!(capabilities["audio"], false);
    }

    #[test]
    fn initialize_advertises_native_subagents_only_after_explicit_negotiation() {
        let ordinary = initialize(&json!({"protocolVersion": 1})).expect("initialize");
        assert!(
            ordinary["agentCapabilities"]["sessionCapabilities"]
                .get("subagents")
                .is_none()
        );

        let negotiated = initialize(&json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "subagents": {}
            }
        }))
        .expect("initialize");
        assert!(negotiated["agentCapabilities"]["sessionCapabilities"]["subagents"].is_object());
    }

    #[test]
    fn thought_level_selector_uses_the_current_models_dynamic_efforts() {
        let configuration = configuration();
        let options = configuration.config_options();
        let thought = options
            .iter()
            .find(|option| option["id"] == "reasoning_effort")
            .expect("thought level option");
        assert_eq!(thought["category"], "thought_level");
        assert_eq!(thought["currentValue"], "xhigh");
        assert_eq!(
            thought["options"]
                .as_array()
                .expect("thought options")
                .iter()
                .filter_map(|option| option["value"].as_str())
                .collect::<Vec<_>>(),
            vec!["default", "low", "high", "xhigh", "max"]
        );

        let plain = configuration.for_model("test/plain", None);
        assert!(
            plain
                .config_options()
                .iter()
                .all(|option| option["id"] != "reasoning_effort"),
            "a non-reasoning model must remove the stale selector"
        );
    }

    #[test]
    fn available_commands_exclude_unhandled_sources_and_include_slash_skills() {
        let commands = [
            command(
                "compact",
                Some("User-defined compact prompt"),
                Template::Text("Do not run this prompt".to_owned()),
                None,
            ),
            command(
                "review",
                Some("Review the change"),
                Template::Text("Review $ARGUMENTS".to_owned()),
                None,
            ),
            command(
                "remote",
                Some("Remote prompt"),
                Template::Mcp(zuno_catalog::command::McpTemplate {
                    client: "server".to_owned(),
                    prompt: "remote".to_owned(),
                    arguments: Vec::new(),
                }),
                None,
            ),
            command(
                "delegated",
                Some("Delegated prompt"),
                Template::Text("Delegate".to_owned()),
                Some(true),
            ),
        ];
        let skills = vec![
            zuno_catalog::skill::Skill::embedded(
                "compact",
                Some("User-defined compact Skill".to_owned()),
                "builtin://compact",
                "Do not load this Skill.",
            ),
            zuno_catalog::skill::Skill::embedded(
                "codegraph",
                Some("Navigate code structurally".to_owned()),
                "builtin://codegraph",
                "Use CodeGraph.",
            ),
        ];

        let update = available_commands_update(commands.iter(), skills);
        let advertised = update["availableCommands"]
            .as_array()
            .expect("available commands");
        assert_eq!(
            advertised
                .iter()
                .filter_map(|command| command["name"].as_str())
                .collect::<Vec<_>>(),
            vec![
                "compact",
                "goal",
                "learn",
                "plan",
                "reflect",
                "start-plan",
                "start-work",
                "review",
                "codegraph"
            ]
        );
        assert_eq!(
            advertised[0]["description"],
            "Summarize older context and keep the recent turn tail"
        );
        assert!(advertised[0].get("input").is_none());
        assert_eq!(advertised[1]["input"]["hint"], "objective | action [value]");
        assert_eq!(
            advertised[2]["input"]["hint"],
            "remember|issue|solved|forget|promote|feedback ..."
        );
        assert_eq!(advertised[4]["input"]["hint"], "turn | session");
        assert_eq!(advertised[7]["input"]["hint"], "question");
    }

    #[test]
    fn slash_prompt_resolution_prefers_commands_and_preserves_arguments() {
        let commands = [
            command(
                "compact",
                Some("Shadow compact"),
                Template::Text("Do not run".to_owned()),
                None,
            ),
            command(
                "review",
                Some("Review"),
                Template::Text("Review $ARGUMENTS".to_owned()),
                None,
            ),
        ];
        let skills = vec![
            zuno_catalog::skill::Skill::embedded(
                "compact",
                Some("Shadow compact Skill".to_owned()),
                "builtin://compact",
                "Do not load.",
            ),
            zuno_catalog::skill::Skill::embedded(
                "codegraph",
                Some("Navigate code".to_owned()),
                "builtin://codegraph",
                "Use CodeGraph.",
            ),
        ];

        assert_eq!(
            resolve_slash_prompt("/review src/lib.rs", commands.iter(), &skills),
            Some(SlashInvocation::Command {
                name: "review".to_owned(),
                arguments: "src/lib.rs".to_owned(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/codegraph trace calls", commands.iter(), &skills),
            Some(SlashInvocation::Skill {
                name: "codegraph".to_owned(),
                source: "builtin://codegraph".to_owned(),
                arguments: "trace calls".to_owned(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/compact", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::Compact,
                arguments: String::new(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/compact unexpected", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::Compact,
                arguments: "unexpected".to_owned(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/goal create ship ACP commands", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::Goal,
                arguments: "create ship ACP commands".to_owned(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/goal ship ACP commands", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::Goal,
                arguments: "ship ACP commands".to_owned(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/plan", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::Plan,
                arguments: String::new(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/start-plan", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::StartPlan,
                arguments: String::new(),
            })
        );
        assert_eq!(
            resolve_slash_prompt("/start-work", commands.iter(), &skills),
            Some(SlashInvocation::Session {
                command: SessionCommand::StartWork,
                arguments: String::new(),
            })
        );
    }

    #[test]
    fn prompt_parser_accepts_images_and_embedded_text_resources() {
        let parsed = parse_prompt(&json!({
            "prompt": [
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "data": "iVBORw0KGgo=",
                    "uri": "file:///tmp/screenshot.png"
                },
                {
                    "type": "resource",
                    "resource": {
                        "uri": "file:///workspace/src/lib.rs",
                        "mimeType": "text/rust",
                        "text": "fn main() {}"
                    }
                },
                {"type": "text", "text": "Review this selection"}
            ]
        }))
        .expect("rich ACP prompt");
        assert!(matches!(
            &parsed.content[0],
            RequestContentBlock::Image {
                filename: Some(filename),
                media_type,
                ..
            } if filename == "screenshot.png" && media_type == "image/png"
        ));
        assert!(matches!(
            &parsed.content[1],
            RequestContentBlock::Text { text }
                if text.contains("file:///workspace/src/lib.rs")
                    && text.contains("fn main() {}")
        ));
        assert_eq!(
            parsed.content[2],
            RequestContentBlock::Text {
                text: "Review this selection".to_owned()
            }
        );
    }

    #[test]
    fn prompt_parser_defers_embedded_image_limits_to_admission() {
        let parsed = parse_prompt(&json!({
            "prompt": [{
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/pixel.png",
                    "mimeType": "image/png",
                    "blob": "iVBORw0KGgo="
                }
            }]
        }))
        .expect("embedded image resource");
        assert!(matches!(
            &parsed.content[0],
            RequestContentBlock::Image {
                filename: Some(filename),
                media_type,
                ..
            } if filename == "pixel.png" && media_type == "image/png"
        ));

        let mut oversized = parse_prompt(&json!({
            "prompt": [{
                "type": "image",
                "mimeType": "image/png",
                "data": "AAAA"
            }]
        }))
        .expect("the parser only validates the ACP shape");
        let root = tempfile::tempdir().expect("attachment root");
        let store = zuno_attachment::AttachmentStore::new(
            root.path(),
            "database",
            zuno_attachment::ImageAdmissionPolicy {
                max_source_bytes: 1,
                ..zuno_attachment::ImageAdmissionPolicy::default()
            },
        )
        .expect("attachment store");
        let error = oversized
            .admit_images(&store)
            .expect_err("admission must enforce the configured byte limit");
        assert!(
            error.message.contains("admission limit"),
            "unexpected admission error: {}",
            error.message
        );
    }
}
