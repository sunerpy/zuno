//! The one composition root that turns a resolved environment into an executable
//! turn, for every surface that has one.
//!
//! Todo 104 joined the tool registry to `run` and left the interactive surface
//! inert; `tui.rs` said so in its own module docs. The tempting fix is to copy the
//! forty lines of `run::execute` that open the database, resolve the session,
//! assemble the tools and build the resolver — and that copy is the defect, not the
//! fix. `tool_runtime` already records why there is exactly one assembly site: a
//! second one is how two surfaces come to disagree about which tools exist or which
//! permission governs them. The same argument applies one level up, to everything
//! `run_turn` needs, so it all lives here and both surfaces call it.
//!
//! # Resolution and execution are separate steps because their failures are
//!
//! [`TurnPlan::resolve`] reads configuration, credentials, the model catalog and
//! the agent set. [`TurnHost::open`] then opens the database and assembles the
//! tools. Both can fail, and an interactive host must learn about it **before** it
//! enters the alternate screen — an error printed into a raw-mode terminal that is
//! about to be torn down is an error nobody reads. Splitting them also means the
//! plan is plain `Send` data, so a host is free to open the host on whichever thread
//! will drive it.
//!
//! # What is deliberately not here
//!
//! Rendering. [`TurnHost::drive`] takes an
//! [`oc_engine::r#loop::TurnEventSender`] and says nothing about what the other end
//! does with it: `run` prints, the TUI folds the events into its component tree.
//! That is the whole reason one driver can serve both.

use std::path::PathBuf;
use std::sync::Arc;

use oc_auth::Credential;
use oc_engine::dispatch::ToolRegistryDispatcher;
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::{
    AgentModelResolver, ResolvedAgent, ResolvedModel as EngineModel, RunTurnRequest, TurnContext,
    TurnEventSender, run_turn,
};
use oc_llm::cache::{DynamicContext, McpToolStatus};
use oc_llm::catalog::{Catalog, CatalogSource, ResolveInput};
use oc_llm::registry::{ApiSurface, ProviderRegistry, Spec};
use oc_provider_compatible::{ReqwestTransport, Transport, factory};
use oc_tool::PermissionAsker;
use serde_json::json;
use uuid::Uuid;

use crate::environment::StartupEnvironment;

const COMPATIBLE_PROVIDER: &str = "openai-compatible";

/// The agent every surface falls back to.
pub(crate) const DEFAULT_AGENT: &str = "build";

const DEFAULT_MAX_STEPS: u32 = 100;
const OPENCODE_ENABLE_EXPERIMENTAL_MODELS: &str = "OPENCODE_ENABLE_EXPERIMENTAL_MODELS";

/// Which session a surface wants to talk in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum SessionChoice {
    /// Create one.
    #[default]
    New,
    /// Reuse the most recent active session in the working directory.
    Continue,
    /// Reuse this exact session.
    Existing(String),
}

impl SessionChoice {
    /// The choice `--session` and `--continue` describe together.
    ///
    /// `--session` wins over `--continue` only because they are mutually exclusive
    /// at the argument layer; nothing here relies on that, so a surface that has not
    /// validated its flags still gets a defined answer rather than a silent one.
    pub(crate) fn resolve(session: Option<&str>, r#continue: bool) -> Self {
        match (session, r#continue) {
            (Some(id), _) => Self::Existing(id.to_owned()),
            (None, true) => Self::Continue,
            (None, false) => Self::New,
        }
    }
}

/// What a surface asks for, before anything has been resolved.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnOptions {
    /// The working directory, defaulting to the process's.
    pub(crate) directory: Option<PathBuf>,
    /// `provider/model`, defaulting to the agent's and then to the catalog's first.
    pub(crate) model: Option<String>,
    /// The agent name, defaulting to [`DEFAULT_AGENT`].
    pub(crate) agent: Option<String>,
    /// Which session to talk in.
    pub(crate) session: SessionChoice,
    /// The title a newly created session gets.
    pub(crate) title: Option<String>,
}

/// Everything resolved from configuration, with no handle open yet.
pub(crate) struct TurnPlan {
    directory: PathBuf,
    project: oc_paths::project::ResolvedProject,
    config: oc_config::schema::Config,
    agent: oc_catalog::agent::Agent,
    provider_id: String,
    model_id: String,
    credential: Option<String>,
    resolver: Resolver,
    session: SessionChoice,
    title: Option<String>,
}

impl TurnPlan {
    /// Resolve configuration, credentials, the catalog and the agent.
    ///
    /// # Errors
    ///
    /// Returns a message when configuration or credentials cannot be read, when the
    /// named agent or model does not exist, or when the model's transport is one this
    /// runtime has no provider for.
    pub(crate) async fn resolve(
        options: &TurnOptions,
        environment: &StartupEnvironment,
    ) -> Result<Self, String> {
        let directory = match options.directory.clone() {
            Some(directory) => directory,
            None => std::env::current_dir().map_err(to_string)?,
        };
        let env = environment.resolved();
        let project = oc_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.clone());
        let layout = oc_paths::Layout::resolve(env);
        let config =
            oc_config::discovery::discover_with(&oc_config::discovery::DiscoveryOptions::new(
                &directory,
                worktree.as_deref(),
                env.clone(),
            ))
            .map_err(to_string)?;
        let credentials = oc_auth::AuthStore::resolve(&layout, env)
            .all()
            .map_err(to_string)?
            .entries;
        let document = CatalogSource::resolve(env, &layout)
            .load()
            .await
            .map_err(to_string)?;
        let input = ResolveInput::new()
            .with_config(&config)
            .with_credentials(credentials.clone())
            .with_env(
                env.iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            )
            .with_experimental_models(env.flag(OPENCODE_ENABLE_EXPERIMENTAL_MODELS));
        let catalog = Catalog::resolve(&document, &input);

        let agents =
            oc_catalog::agent::load(&directory, worktree.as_deref(), env).map_err(to_string)?;
        let agent_name = options.agent.as_deref().unwrap_or(DEFAULT_AGENT);
        let agent = agents
            .into_iter()
            .find(|entry| entry.name == agent_name)
            .ok_or_else(|| format!("Agent not found: {agent_name}"))?;
        let requested_model = options.model.as_deref().or(agent.model.as_deref());
        let (provider_id, model_id, catalog_model) = select_model(&catalog, requested_model)?;
        if !supports_compatible_transport(&catalog_model.api.npm) {
            return Err(format!(
                "model {provider_id}/{model_id} uses transport {}, but this runtime currently supports OpenAI-compatible transports",
                catalog_model.api.npm
            ));
        }
        let resolver = Resolver {
            requested_agent: agent.name.clone(),
            system_prompt: agent.prompt.clone().unwrap_or_default(),
            max_steps: agent
                .steps
                .map_or(DEFAULT_MAX_STEPS, std::num::NonZeroU32::get),
            requested_provider: provider_id.clone(),
            requested_model: model_id.clone(),
            wire_model: catalog_model.api.id.clone(),
            spec: model_spec(catalog_model),
        };
        Ok(Self {
            directory,
            project,
            config,
            agent,
            credential: credentials.get(&provider_id).map(credential_value),
            provider_id,
            model_id,
            resolver,
            session: options.session.clone(),
            title: options.title.clone(),
        })
    }
}

/// An open database, an assembled tool set, and the session a turn runs in.
pub(crate) struct TurnHost {
    connection: rusqlite::Connection,
    providers: ProviderRegistry,
    resolver: Resolver,
    dispatcher: ToolRegistryDispatcher,
    interrupt: InterruptSignal,
    session_id: String,
    agent: String,
    provider_id: String,
    model_id: String,
}

impl TurnHost {
    /// Open the database and assemble the tools `plan` resolved.
    ///
    /// `approval` is the one collaborator a surface must supply itself, because it is
    /// the one thing the two surfaces genuinely differ on: a headless run has nobody
    /// to ask and must fail closed, while an interactive one has a human and must
    /// ask. Everything else is identical by construction.
    ///
    /// # Errors
    ///
    /// Returns a message when the database cannot be opened or migrated, when the
    /// session cannot be resolved, or when the tools cannot be assembled.
    pub(crate) fn open(
        plan: TurnPlan,
        environment: &StartupEnvironment,
        approval: Arc<dyn PermissionAsker>,
    ) -> Result<Self, String> {
        let env = environment.resolved();
        let worktree = plan
            .project
            .vcs
            .as_ref()
            .map(|_| plan.project.directory.clone());
        let mut providers = ProviderRegistry::new();
        let transport: Arc<dyn Transport> = Arc::new(ReqwestTransport::new(&plan.provider_id));
        let credential = plan.credential.clone();
        providers.register_fallible(
            COMPATIBLE_PROVIDER,
            factory(transport, move |_| credential.clone()),
        );

        let mut connection = oc_db::open_default().map_err(to_string)?;
        oc_db::migration::apply(&mut connection).map_err(to_string)?;
        let now = oc_db::message::now_millis();
        ensure_project(&connection, &plan.project, now)?;
        let session = resolve_session(&mut connection, &plan, now)?;

        let runtime_tools = super::tool_runtime::assemble(
            &plan.directory,
            worktree.as_deref(),
            env,
            &plan.config,
            &plan.agent,
            &plan.provider_id,
            &plan.model_id,
        )?;
        let interrupt = InterruptSignal::new();
        let dispatcher = ToolRegistryDispatcher::new(
            runtime_tools.tools,
            runtime_tools.rules,
            approval,
            InterruptSignal::new(),
            McpToolStatus::Ready,
        );
        Ok(Self {
            connection,
            providers,
            resolver: plan.resolver,
            dispatcher,
            interrupt,
            session_id: session.id,
            agent: plan.agent.name,
            provider_id: plan.provider_id,
            model_id: plan.model_id,
        })
    }

    /// The session every turn this host drives belongs to.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Persist `prompt` as the user's message and run one turn over it.
    ///
    /// The prompt is stamped after whatever the session already holds rather than at
    /// the current millisecond, because a surface that drives several turns from one
    /// host submits prompt *n* immediately after turn *n-1* stored its reply. Two
    /// messages in one millisecond order by their random ids, so an unclamped stamp
    /// would sometimes file a new prompt ahead of the answer it follows and send the
    /// provider a conversation that never happened.
    ///
    /// # Errors
    ///
    /// Returns a message when the prompt cannot be persisted or the turn fails.
    pub(crate) async fn drive(
        &mut self,
        prompt: &str,
        events: TurnEventSender,
    ) -> Result<(), String> {
        let latest = oc_db::message::MessageStore::new(&self.connection)
            .latest_time_created(&self.session_id)
            .map_err(to_string)?;
        persist_user_message(
            &self.connection,
            &self.session_id,
            &self.agent,
            &self.provider_id,
            &self.model_id,
            prompt,
            oc_db::message::created_after(oc_db::message::now_millis(), latest),
        )?;
        run_turn(
            RunTurnRequest::new(
                self.session_id.clone(),
                Uuid::new_v4().simple().to_string(),
                DynamicContext::default(),
            ),
            TurnContext::new(
                &mut self.connection,
                &self.providers,
                &self.resolver,
                &self.dispatcher,
                &self.interrupt,
            ),
            events,
        )
        .await
        .map_err(to_string)?;
        Ok(())
    }
}

fn select_model<'a>(
    catalog: &'a Catalog,
    requested: Option<&str>,
) -> Result<(String, String, &'a oc_llm::catalog::ResolvedModel), String> {
    if let Some(requested) = requested {
        let (provider_id, model_id) = requested
            .split_once('/')
            .ok_or_else(|| format!("model must be provider/model, got {requested:?}"))?;
        let model = catalog
            .model(provider_id, model_id)
            .ok_or_else(|| format!("Model not found: {requested}"))?;
        return Ok((provider_id.to_owned(), model_id.to_owned(), model));
    }

    for provider_id in catalog.provider_ids() {
        let provider = catalog
            .provider(provider_id)
            .ok_or_else(|| format!("catalog listed provider {provider_id} but has no entry"))?;
        let mut model_ids: Vec<&str> = provider.models.keys().map(String::as_str).collect();
        model_ids.sort_by(|left, right| oc_llm::catalog::collate::compare(left, right));
        if let Some(model_id) = model_ids.into_iter().next() {
            let model = provider
                .models
                .get(model_id)
                .ok_or_else(|| format!("catalog lost model {provider_id}/{model_id}"))?;
            return Ok((provider_id.to_owned(), model_id.to_owned(), model));
        }
    }
    Err("no available model; configure a provider credential or provider block".to_owned())
}

fn supports_compatible_transport(npm: &str) -> bool {
    matches!(
        npm,
        "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@openrouter/ai-sdk-provider"
    )
}

fn model_spec(model: &oc_llm::catalog::ResolvedModel) -> Spec {
    let mut spec = Spec::new(COMPATIBLE_PROVIDER).with_surface(ApiSurface::Chat);
    if !model.api.url.is_empty() {
        spec = spec.with_base_url(&model.api.url);
    }
    for (name, value) in &model.headers {
        spec = spec.with_header(name, value);
    }
    for (name, value) in &model.options {
        spec = spec.with_option(name, value.clone());
    }
    spec
}

fn credential_value(credential: &Credential) -> String {
    match credential {
        Credential::Api { key, .. } => key.expose().to_owned(),
        Credential::Oauth { access, .. } => access.expose().to_owned(),
        Credential::WellKnown { token, .. } => token.expose().to_owned(),
    }
}

struct Resolver {
    requested_agent: String,
    system_prompt: String,
    max_steps: u32,
    requested_provider: String,
    requested_model: String,
    wire_model: String,
    spec: Spec,
}

impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == self.requested_agent).then(|| {
            ResolvedAgent::new(
                self.requested_agent.clone(),
                self.system_prompt.clone(),
                self.max_steps,
            )
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<EngineModel> {
        (provider_id == self.requested_provider && model_id == self.requested_model)
            .then(|| EngineModel::new(self.spec.clone(), self.wire_model.clone(), ApiSurface::Chat))
    }
}

fn ensure_project(
    connection: &rusqlite::Connection,
    project: &oc_paths::project::ResolvedProject,
    now: i64,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO project \
             (id, worktree, vcs, time_created, time_updated, sandboxes) \
             VALUES (?1, ?2, ?3, ?4, ?4, '[]') \
             ON CONFLICT (id) DO UPDATE SET \
               worktree = excluded.worktree, \
               vcs = excluded.vcs, \
               time_updated = excluded.time_updated",
            (
                project.id.as_str(),
                project.directory.to_string_lossy().as_ref(),
                project.vcs.as_ref().map(|_| "git"),
                now,
            ),
        )
        .map_err(to_string)?;
    Ok(())
}

fn resolve_session(
    connection: &mut rusqlite::Connection,
    plan: &TurnPlan,
    now: i64,
) -> Result<oc_db::session::Session, String> {
    match &plan.session {
        SessionChoice::Existing(session_id) => {
            return oc_db::session::get(connection, session_id).map_err(to_string);
        }
        SessionChoice::Continue => {
            return oc_db::session::list(
                connection,
                &oc_db::session::ListQuery::directory(plan.directory.to_string_lossy())
                    .active_only()
                    .with_limit(1),
            )
            .map_err(to_string)?
            .into_iter()
            .next()
            .ok_or_else(|| "no session found to continue in the current directory".to_owned());
        }
        SessionChoice::New => {}
    }

    let session_id = prefixed_id("ses");
    let title = plan
        .title
        .clone()
        .unwrap_or_else(|| "New session".to_owned());
    let mut input = oc_db::session::SessionCreate::new(
        &session_id,
        Uuid::new_v4().simple().to_string(),
        &plan.project.id,
        plan.project.directory.to_string_lossy().into_owned(),
        plan.directory.to_string_lossy().into_owned(),
        title,
        crate::COMPATIBILITY_VERSION,
    )
    .at(now);
    input.agent = Some(plan.agent.name.clone());
    input.model =
        Some(json!({"providerID": plan.provider_id, "modelID": plan.model_id}).to_string());
    let transaction = connection.transaction().map_err(to_string)?;
    let creation = oc_db::session::create(&transaction, &input).map_err(to_string)?;
    transaction.commit().map_err(to_string)?;
    Ok(creation.into_session())
}

fn persist_user_message(
    connection: &rusqlite::Connection,
    session_id: &str,
    agent: &str,
    provider_id: &str,
    model_id: &str,
    text: &str,
    now: i64,
) -> Result<(), String> {
    let message_id = prefixed_id("msg");
    let message = oc_db::message::MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": session_id,
        "role": "user",
        "time": {"created": now},
        "agent": agent,
        "model": {"providerID": provider_id, "modelID": model_id}
    }))
    .map_err(to_string)?;
    let part = oc_db::message::PartRecord::from_json(
        json!({
            "id": prefixed_id("prt"),
            "sessionID": session_id,
            "messageID": message.id,
            "type": "text",
            "text": text
        }),
        now,
    )
    .map_err(to_string)?;
    let store = oc_db::message::MessageStore::new(connection);
    store.put_message_at(&message, now).map_err(to_string)?;
    store.put_part_at(&part, now).map_err(to_string)?;
    Ok(())
}

fn prefixed_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
