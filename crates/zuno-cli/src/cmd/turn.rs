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
//! [`zuno_engine::r#loop::TurnEventSender`] and says nothing about what the other end
//! does with it: `run` prints, the TUI folds the events into its component tree.
//! That is the whole reason one driver can serve both.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;
use zuno_agent::model_policy::{AnyModel, ModelChoice, ModelPolicy};
use zuno_agent::reflection::{
    CommandOutcome, ReflectionConfig, ReflectionError, ReflectionFork, ReflectionRequest,
    ReflectionRunner, ReflectionTools, ReflectionTurn, TranscriptEvent, TurnDelivery,
    TurnTranscript,
};
use zuno_auth::{AuthStore, Credential, LoginMethodRegistry};
use zuno_config::schema::provider::ProviderTransport;
use zuno_engine::compaction::{CompactionState, TokenWindow};
use zuno_engine::dispatch::{AuthorizationPolicy, ToolRegistryDispatcher};
use zuno_engine::driver::AgentDriver;
use zuno_engine::r#loop::{
    AgentModelResolver, ResolvedAgent, ResolvedModel as EngineModel, RunTurnRequest,
    ToolConcurrencyLimit, ToolFailureRecovery, TurnContext, TurnError, TurnEvent, TurnEventSender,
    TurnOutcome, TurnRecovery,
};
use zuno_engine::prelude::{
    CompactionSkipped, InternalAgent, InternalProviders, Internals, PreludeContext, PreludeOutcome,
    compact_requested, run_prelude,
};
use zuno_engine::prompt::PromptAssembly;
use zuno_engine::status::{SessionRunGuard, SessionRunRegistry};
use zuno_error::{DbError, ProviderError, Recovery};
use zuno_goal::{
    ContinuationAttempt, ContinuationSuppression, DEFAULT_GOAL_RETRY_INITIAL_DELAY,
    DEFAULT_GOAL_RETRY_JITTER_PERCENT, DEFAULT_GOAL_RETRY_MAX_DELAY,
    DEFAULT_GOAL_RETRY_POLL_INTERVAL, GoalContinuation, GoalFailureDisposition, GoalProjection,
    GoalRetryPolicy, GoalRetryReason, GoalRetryState, GoalStore, GoalTerminalFailure, GoalTurnMode,
    GoalTurnOutcome, QueuedUserInput,
};
use zuno_llm::cache::{DynamicContext, McpToolStatus};
use zuno_llm::catalog::resolved::ModelEndpoint;
use zuno_llm::catalog::{Catalog, CatalogProvenance, CatalogSource, ResolveInput};
use zuno_llm::event::{Message as ProviderMessage, RequestContentBlock, Role, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, CompletionRequest, Provider, ProviderRegistry, Spec, ToolSchema, generation,
};
use zuno_llm::stream::StreamAccumulator;
use zuno_memory::{
    MemoryObserver, MemoryService, PromotionPolicy, Scope, ScopeLimits, ScopePaths, SessionMemory,
};
use zuno_provider_compatible::{ReqwestTransport, Transport};
use zuno_runtime::HarnessRuntime;
use zuno_tool::{
    AllowAll, NeverInterrupted, PermissionAsker, ToolContext, ToolReplayPolicy, erase,
};

use crate::environment::StartupEnvironment;

const COMPATIBLE_PROVIDER: &str = "openai-compatible";

/// The agent every surface falls back to.
pub(crate) const DEFAULT_AGENT: &str = "build";

const DEFAULT_MAX_STEPS: u32 = 100;
const ZUNO_ENABLE_EXPERIMENTAL_MODELS: &str = "ZUNO_ENABLE_EXPERIMENTAL_MODELS";

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
    /// Rebuild a not-yet-persisted session without changing its process identity.
    Prepared(PreparedSessionIdentity),
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

/// Stable process identity for a session whose database row may not exist yet.
#[derive(Clone)]
pub(crate) struct PreparedSessionIdentity {
    id: String,
    materialized: Arc<AtomicBool>,
}

impl PreparedSessionIdentity {
    fn pending(id: String) -> Self {
        Self {
            id,
            materialized: Arc::new(AtomicBool::new(false)),
        }
    }

    fn existing(id: String) -> Self {
        Self {
            id,
            materialized: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The stable id used by every session-scoped component in this process.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// Whether the identity has a durable `session` row.
    pub(crate) fn is_materialized(&self) -> bool {
        self.materialized.load(Ordering::Acquire)
    }

    fn mark_materialized(&self) {
        self.materialized.store(true, Ordering::Release);
    }
}

impl fmt::Debug for PreparedSessionIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedSessionIdentity")
            .field("id", &self.id)
            .field("materialized", &self.is_materialized())
            .finish()
    }
}

impl PartialEq for PreparedSessionIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for PreparedSessionIdentity {}

#[derive(Debug)]
enum SessionMaterializer {
    Existing,
    Pending(Box<zuno_db::session::SessionCreate>),
}

/// Session facts resolved before a [`TurnHost`] is assembled.
///
/// Existing sessions carry their durable row immediately. A new TUI session carries only
/// a prepared insert until the first model-bound input is persisted.
struct PreparedTurnHost {
    identity: PreparedSessionIdentity,
    title: String,
    directory: String,
    usage: zuno_db::session::SessionUsage,
    materializer: SessionMaterializer,
}

/// What a surface asks for, before anything has been resolved.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnOptions {
    /// The working directory, defaulting to the process's.
    pub(crate) directory: Option<PathBuf>,
    /// `provider/model`, defaulting through the agent, config, and catalog.
    pub(crate) model: Option<String>,
    /// The agent name, defaulting to [`DEFAULT_AGENT`].
    pub(crate) agent: Option<String>,
    /// Which session to talk in.
    pub(crate) session: SessionChoice,
    /// The title a newly created session gets.
    pub(crate) title: Option<String>,
    /// The reasoning level to ask the model for, when it supports reasoning.
    ///
    /// `None` means "send no reasoning control", which is not the same as
    /// [`zuno_llm::effort::ReasoningEffort::Off`]: `Off` actively disables thinking on
    /// a model that would otherwise do it, while `None` leaves the provider's own
    /// default in place and keeps the request byte-identical to a build without this
    /// field.
    pub(crate) effort: Option<zuno_llm::effort::ReasoningEffort>,
    /// Whether this host consumes the committed extension composition or the one
    /// pending transaction prepared for a quiescent replacement.
    pub(crate) extension_composition: ExtensionComposition,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExtensionComposition {
    #[default]
    Active,
    Desired,
}

/// Everything resolved from configuration, with no handle open yet.
pub(crate) struct TurnPlan {
    profile: zuno_runtime::HarnessProfile,
    directory: PathBuf,
    project: zuno_paths::project::ResolvedProject,
    config: zuno_config::schema::Config,
    agents: Vec<zuno_catalog::agent::Agent>,
    agent: zuno_catalog::agent::Agent,
    extensions: zuno_extension::ResolvedExtensions,
    extension_scope: zuno_extension::Scope,
    extension_revision: u64,
    extension_transaction: Option<zuno_extension::ExtensionTransaction>,
    extension_prepared: Option<zuno_extension::PreparedTransition>,
    provider_id: String,
    model_id: String,
    auth_store: AuthStore,
    credential: Option<Credential>,
    resolver: Resolver,
    session: SessionChoice,
    title: Option<String>,
    internals: Internals,
    reflection_model: Option<EngineModel>,
    window: TokenWindow,
    notes: Vec<String>,
    /// Every `provider/model` the resolved catalog offers, kept for the model picker.
    ///
    /// The **whole** catalog rather than the session provider's slice, and that is the
    /// correctness argument rather than a convenience: [`select_model`] derives the
    /// provider from the `/` prefix of whatever it is handed, and [`TurnPlan::resolve`]
    /// re-resolves the credential, the token window and the tool set from that provider.
    /// A picker offering only one provider's models therefore withheld choices the
    /// rebuild path could already honour — the defect this field exists to close.
    ///
    /// Filled from [`Catalog::model_lines`], which is the same enumeration `zuno models`
    /// prints. One function, so the two surfaces cannot disagree again.
    catalog_models: Vec<String>,
    /// Canonical reasoning levels the TUI may offer for each catalog model.
    ///
    /// A model that explicitly declares canonical variants contributes only those
    /// variants. A model whose capability says it reasons but declares no variants
    /// receives the provider-neutral scale. This keeps the picker and request builder
    /// from treating an omitted capability flag as stronger evidence than explicit
    /// per-level request options.
    reasoning_efforts: BTreeMap<String, Vec<zuno_llm::effort::ReasoningEffort>>,
    /// Every discovered skill, shared by the prompt catalogue and the `skill` tool.
    ///
    /// One load, one [`Arc`], two consumers — because a tool answering from a second
    /// load could hand back a body for a name the prompt never advertised, or refuse
    /// one it did.
    skills: Arc<zuno_catalog::skill::Skills>,
    /// The `AGENTS.md`-class rule files this session runs under, read once here.
    ///
    /// Loaded during resolution rather than at host construction because the read is
    /// `async` and [`TurnHost::open_with_runtime_and_mcp`] is not — and because these
    /// bytes must not be re-read per turn: a rule file the user edits mid-session
    /// would otherwise change the static prompt prefix underneath the provider's
    /// cache, which is the same reason [`zuno_memory::SessionMemory`] freezes its
    /// blocks at session start.
    instructions: zuno_config::LoadedInstructions,
    /// Catalog facts for the models a delegation may name. See [`delegation_facts`].
    delegation_facts: Arc<zuno_tools::task::FixedFacts>,
    /// Whether any reachable model accepts images, which gates one delegation target.
    vision_available: bool,
    /// Whether the session's model declares reasoning support in the catalog.
    ///
    /// Kept from resolution so a surface can ask without re-resolving the catalog, and
    /// so a key that cycles reasoning levels can refuse on a model that has none rather
    /// than relabel a control the provider would reject.
    reasoning_supported: bool,
    /// The reasoning level this plan resolved with, echoed back for display.
    effort: Option<zuno_llm::effort::ReasoningEffort>,
    /// Fully validated automatic recovery policy for active goals.
    goal_retry_policy: GoalRetryPolicy,
}

impl TurnPlan {
    /// Attach the reservation acquired by a surface-level transition coordinator.
    ///
    /// Server request assembly uses this to hold the old composition closed while
    /// configuration and the candidate profile are prepared. TUI replacement can
    /// continue to reserve inside [`TurnHost::open_with_runtime_and_mcp`] because it
    /// already owns the only foreground host.
    pub(crate) fn use_prepared_extension_transition(
        &mut self,
        prepared: zuno_extension::PreparedTransition,
    ) -> Result<(), String> {
        let transaction = self.extension_transaction.as_ref().ok_or_else(|| {
            "turn plan has no pending extension transaction for the reservation".to_owned()
        })?;
        if transaction.scope() != prepared.scope() || transaction.revision() != prepared.revision()
        {
            return Err(format!(
                "prepared extension transition {} does not match planned revision {}",
                prepared.revision(),
                transaction.revision()
            ));
        }
        self.extension_transaction = None;
        self.extension_prepared = Some(prepared);
        Ok(())
    }

    fn abort_extension_candidate(
        &mut self,
        registry: &zuno_extension::ExtensionRegistry,
    ) -> Result<(), String> {
        if let Some(prepared) = self.extension_prepared.take() {
            return prepared.abort().map_err(to_string);
        }
        if let Some(transaction) = self.extension_transaction.take() {
            return registry.abort(&transaction).map_err(to_string);
        }
        Ok(())
    }

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
        let project = zuno_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.clone());
        let layout = zuno_paths::Layout::resolve(env);
        let config =
            zuno_config::discovery::discover_with(&zuno_config::discovery::DiscoveryOptions::new(
                &directory,
                worktree.as_deref(),
                env.clone(),
            ))
            // `report()`, not `to_string()`: `ConfigError::Invalid` keeps its
            // per-issue detail out of `Display` deliberately, so `to_string` here
            // printed "failed validation (1 issue(s))" and dropped every repair
            // instruction. This is the path both the TUI and `zuno run` take, which
            // made it the one place a config error reached a user unactionable.
            .map_err(|error| error.report())?;
        let extension_scope =
            zuno_extension::Scope::new(worktree.as_deref().unwrap_or(directory.as_path()));
        let static_extensions =
            zuno_extension::discover_static(&directory, worktree.as_deref(), env)
                .map_err(to_string)?;
        let (extensions, extension_revision, extension_transaction) =
            match options.extension_composition {
                ExtensionComposition::Active => (
                    zuno_extension::resolve_active(
                        &extension_scope,
                        &static_extensions,
                        environment.extensions(),
                    )
                    .map_err(to_string)?,
                    environment.extensions().active_revision(&extension_scope),
                    None,
                ),
                ExtensionComposition::Desired => {
                    let transaction = environment
                        .extensions()
                        .pending_transaction(&extension_scope);
                    (
                        zuno_extension::resolve_desired(
                            &extension_scope,
                            &static_extensions,
                            environment.extensions(),
                        )
                        .map_err(to_string)?,
                        environment.extensions().desired_revision(&extension_scope),
                        transaction,
                    )
                }
            };
        let goal_retry_policy = resolve_goal_retry_policy(&config)?;
        let auth_store = AuthStore::resolve(&layout, env);
        let credentials = auth_store.all().map_err(to_string)?.entries;
        let loaded = CatalogSource::resolve(env, &layout)
            .load()
            .await
            .map_err(to_string)?;
        let login_methods = LoginMethodRegistry::native();
        let input = ResolveInput::new()
            .with_config(&config)
            .with_credentials(credentials.clone())
            .with_login_methods(&login_methods)
            .with_env(
                env.iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            )
            .with_experimental_models(env.flag(ZUNO_ENABLE_EXPERIMENTAL_MODELS));
        let catalog = Catalog::resolve(loaded.document(), &input);

        let loaded_agents = zuno_catalog::agent::load_map(&directory, worktree.as_deref(), env)
            .map_err(to_string)?;
        let merged_agents =
            zuno_catalog::agent::merge_agent_maps(&loaded_agents.agents, extensions.agents())
                .map_err(to_string)?;
        let agents = zuno_catalog::agent::list(&merged_agents, &loaded_agents.origins);
        let agent_name = options.agent.as_deref().unwrap_or(DEFAULT_AGENT);
        let agent = agents
            .iter()
            .find(|entry| entry.name == agent_name)
            .cloned()
            .ok_or_else(|| format!("Agent not found: {agent_name}"))?;
        let requested_model = options
            .model
            .as_deref()
            .or(agent.model.as_deref())
            .or(config.model.as_deref());
        let (provider_id, model_id, catalog_model) =
            select_model(&catalog, requested_model, loaded.provenance())?;
        if provider_factory_key(catalog_model.api.transport).is_none() {
            return Err(format!(
                "model {provider_id}/{model_id} has no native provider transport"
            ));
        }
        let catalog_models = picker_model_ids(&catalog);
        let reasoning_efforts = catalog_models
            .iter()
            .filter_map(|qualified| {
                let (provider, model) = qualified.split_once('/')?;
                let resolved = catalog.model(provider, model)?;
                Some((qualified.clone(), selectable_reasoning_efforts(resolved)))
            })
            .collect::<BTreeMap<_, _>>();
        let reasoning_supported = reasoning_efforts
            .get(&format!("{provider_id}/{model_id}"))
            .is_some_and(|levels| !levels.is_empty());
        let vision_available = catalog_models.iter().any(|line| {
            line.split_once('/').is_some_and(|(provider, model)| {
                catalog
                    .model(provider, model)
                    .is_some_and(|model| model.capabilities.input.image)
            })
        });
        let mut prompt_assembly = PromptAssembly::new();
        prompt_assembly
            .push(
                "agent.base",
                agent_prompt_source(&agent),
                agent.prompt.clone().unwrap_or_default(),
            )
            .map_err(to_string)?;
        if let Some(policy) = zuno_agent::builtin::get(&agent.name, vision_available) {
            prompt_assembly
                .push(
                    "agent.policy",
                    format!("zuno-agent::builtin:{}", agent.name),
                    policy.prompt_policy(),
                )
                .map_err(to_string)?;
        }
        let resolver = Resolver {
            requested_agent: agent.name.clone(),
            system_prompt: prompt_assembly.render(),
            prompt_assembly: Some(prompt_assembly),
            max_steps: agent
                .steps
                .map_or(DEFAULT_MAX_STEPS, std::num::NonZeroU32::get),
            requested_provider: provider_id.clone(),
            requested_model: model_id.clone(),
            wire_model: catalog_model.api.id.clone(),
            reasoning_options: session_reasoning_options(
                turn_effort(options.effort, &agent, &provider_id, &model_id),
                catalog_model,
            ),
            spec: with_agent_options(model_spec(&catalog, catalog_model, env)?, &agent),
        };
        let window = TokenWindow {
            context: token_count(catalog_model.limit.context),
            max_output: token_count(catalog_model.limit.output),
        };
        let mut notes = Vec::new();
        let internals = resolve_internals(
            ResolveInternalsInput {
                config: &config,
                catalog: &catalog,
                provider_id: &provider_id,
                model_id: &model_id,
                session_model: catalog_model,
                env,
                plugin_small_model: None,
            },
            &mut notes,
        )?;
        let reflection_model =
            resolve_reflection_model(&config, &catalog, &provider_id, env, &mut notes)?;
        let delegation_facts = Arc::new(delegation_facts(&catalog));
        let skills = Arc::new(
            zuno_catalog::skill::load(&zuno_catalog::skill::SkillOptions::from_config(
                &directory,
                worktree.as_deref(),
                env,
                &config,
            ))
            .await
            .with_overlay(extensions.skills().iter().cloned()),
        );
        let mut runtime_surface =
            zuno_extension::runtime_surface(&extensions, &directory).map_err(to_string)?;
        let mut extension_tools = zuno_extension::lifecycle_tools(
            extension_scope.clone(),
            static_extensions.clone(),
            Arc::clone(environment.extensions()),
        );
        extension_tools.extend(runtime_surface.tools().iter().cloned());
        let extension_contributions =
            zuno_harness::ToolContributions::new(extension_tools).map_err(to_string)?;
        let mut profile = zuno_harness::default_profile_with_tools(extension_contributions);
        if let Some(bundle) = runtime_surface.take_bundle() {
            profile = profile.with_bundle(bundle);
        }
        let instructions =
            zuno_config::Instructions::discover(&zuno_config::InstructionOptions::from_config(
                directory.as_path(),
                worktree.as_deref(),
                env,
                &config,
            ))
            .load()
            .await;
        Ok(Self {
            profile,
            directory,
            project,
            config,
            agents,
            agent,
            extensions,
            extension_scope,
            extension_revision,
            extension_transaction,
            extension_prepared: None,
            auth_store,
            credential: resolved_credential(
                catalog.provider(&provider_id),
                credentials.get(&provider_id),
                env,
            ),
            provider_id,
            model_id,
            resolver,
            session: options.session.clone(),
            title: options.title.clone(),
            internals,
            reflection_model,
            window,
            notes,
            catalog_models,
            reasoning_efforts,
            skills,
            instructions,
            delegation_facts,
            vision_available,
            reasoning_supported,
            effort: options.effort,
            goal_retry_policy,
        })
    }

    /// The directory this turn runs in.
    pub(crate) fn directory(&self) -> &std::path::Path {
        &self.directory
    }

    /// The worktree root, when the directory is under version control.
    pub(crate) fn worktree(&self) -> Option<&std::path::Path> {
        self.project
            .vcs
            .as_ref()
            .map(|_| self.project.directory.as_path())
    }

    /// The merged configuration this turn resolved against.
    ///
    /// Handed out rather than re-discovered by callers: discovery walks the filesystem
    /// and merges layers, so a second call is both slow and free to disagree with the
    /// configuration the turn is actually using.
    pub(crate) const fn config(&self) -> &zuno_config::schema::Config {
        &self.config
    }

    /// The agent that will answer.
    pub(crate) fn agent_name(&self) -> &str {
        &self.agent.name
    }

    /// Every resolved agent, including active static and process extensions.
    pub(crate) fn agents(&self) -> &[zuno_catalog::agent::Agent] {
        &self.agents
    }

    /// The exact skill set shared by prompt assembly and the `skill` tool.
    pub(crate) fn skills(&self) -> &zuno_catalog::skill::Skills {
        &self.skills
    }

    /// Every `provider/model` the catalog offers, in the order `zuno models` prints them.
    ///
    /// Kept from resolution rather than re-derived: rebuilding the catalog means reading
    /// the cache and re-applying plugin extensions, and a picker must not do that.
    pub(crate) fn catalog_model_ids(&self) -> Vec<String> {
        self.catalog_models.clone()
    }

    /// Canonical reasoning levels the model picker may offer for `qualified`.
    pub(crate) fn model_reasoning_efforts(
        &self,
        qualified: &str,
    ) -> Vec<zuno_llm::effort::ReasoningEffort> {
        self.reasoning_efforts
            .get(qualified)
            .cloned()
            .unwrap_or_default()
    }

    /// `provider/model`, as resolved.
    pub(crate) fn qualified_model(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    /// Whether the resolved model declares reasoning support.
    pub(crate) const fn reasoning_supported(&self) -> bool {
        self.reasoning_supported
    }

    /// Whether `qualified` reasons, according to the facts a delegation resolves through.
    pub(crate) fn model_reasons(&self, qualified: &str) -> bool {
        self.delegation_facts.reasons(qualified).unwrap_or(false)
    }

    /// The reasoning level this plan resolved with.
    pub(crate) const fn effort(&self) -> Option<zuno_llm::effort::ReasoningEffort> {
        self.effort
    }

    /// The model's context ceiling, or zero when the catalog declares none.
    pub(crate) const fn context_window(&self) -> u64 {
        self.window.context
    }
}

/// A catalog token ceiling, which is JSON and therefore a float, as a token count.
///
/// The catalog stores `limit.context` and `limit.output` as `f64` because
/// `models.dev` publishes them as JSON numbers. A negative or non-finite value is
/// zero rather than a wrapped integer, and zero is already meaningful:
/// [`zuno_engine::compaction::CompactionPolicy`] treats a zero window as
/// "compaction cannot be triggered by a threshold", which is the correct reading of a
/// model that declares no window.
fn token_count(limit: f64) -> u64 {
    if limit.is_finite() && limit > 0.0 {
        limit as u64
    } else {
        0
    }
}

fn resolve_goal_retry_policy(
    config: &zuno_config::schema::Config,
) -> Result<GoalRetryPolicy, String> {
    let retry = config.goal.as_ref().and_then(|goal| goal.retry.as_ref());
    let initial_delay = retry
        .and_then(|retry| retry.initial_delay_ms)
        .map_or(DEFAULT_GOAL_RETRY_INITIAL_DELAY, |value| {
            Duration::from_millis(value.get())
        });
    let max_delay = retry
        .and_then(|retry| retry.max_delay_ms)
        .map_or(DEFAULT_GOAL_RETRY_MAX_DELAY, |value| {
            Duration::from_millis(value.get())
        });
    let jitter_percent = retry
        .and_then(|retry| retry.jitter_percent)
        .unwrap_or(DEFAULT_GOAL_RETRY_JITTER_PERCENT);
    let poll_interval = retry
        .and_then(|retry| retry.poll_interval_ms)
        .map_or(DEFAULT_GOAL_RETRY_POLL_INTERVAL, |value| {
            Duration::from_millis(value.get())
        });
    GoalRetryPolicy::new(initial_delay, max_delay, jitter_percent, poll_interval)
        .map_err(|error| format!("invalid goal.retry configuration: {error}"))
}

/// Resolve `compaction`, `title` and `summary` through todo 64's model policy.
///
/// Iterates [`zuno_agent::builtin::INTERNAL_NAMES`] rather than three literals, so an
/// internal added there is resolved here with no edit — which is the whole reason
/// that constant exists. Each name's prompt comes from
/// [`zuno_catalog::agent::builtin`], which is where the upstream native's text lives;
/// nothing is written twice.
///
/// # Why an internal cannot leave the session's provider
///
/// [`ModelPolicy`] may legitimately answer with a model under a different provider,
/// and this function then declines it and records why. [`TurnHost::open_with_mcp`] wires
/// exactly one credential — the session provider's — so honouring a cross-provider
/// answer would mean presenting that credential to a different vendor's endpoint.
/// Falling back to the session's own model costs a larger model for a small job;
/// the alternative costs the user's API key. The note is emitted on the turn's event
/// channel so the downgrade is visible rather than silent.
///
/// The preset rung of the precedence chain is reachable but unfed: nothing in this
/// workspace discovers a [`zuno_agent::model_policy::PresetLibrary`] yet, so only the
/// per-agent override and the session model can answer today.
struct ResolveInternalsInput<'a> {
    config: &'a zuno_config::schema::Config,
    catalog: &'a Catalog,
    provider_id: &'a str,
    model_id: &'a str,
    session_model: &'a zuno_llm::catalog::ResolvedModel,
    env: &'a zuno_paths::Env,
    plugin_small_model: Option<&'a zuno_llm::catalog::ResolvedModel>,
}

fn resolve_internals(
    input: ResolveInternalsInput<'_>,
    notes: &mut Vec<String>,
) -> Result<Internals, String> {
    let ResolveInternalsInput {
        config,
        catalog,
        provider_id,
        model_id,
        session_model,
        env,
        plugin_small_model,
    } = input;
    let session_choice = ModelChoice::new(format!("{provider_id}/{model_id}"));
    let mut policy = ModelPolicy::new().with_session_model(session_choice);
    if let Some(agents) = &config.agent {
        policy = policy.with_agent_overrides(agents);
    }

    let configured_small_model = config.small_model.as_deref().and_then(|qualified| {
        let (small_provider, small_model) = qualified.split_once('/')?;
        if small_provider != provider_id {
            notes.push(format!(
                "small_model: `{qualified}` is served by `{small_provider}`, and only `{provider_id}`'s credential is wired for this turn"
            ));
            return None;
        }
        let model = catalog.model(small_provider, small_model)?;
        if provider_factory_key(model.api.transport).is_none()
            || model_spec(catalog, model, env).is_err()
        {
            notes.push(format!(
                "small_model: `{qualified}` cannot be reached by this runtime"
            ));
            return None;
        }
        Some(model)
    });
    let inherited_small_model = configured_small_model.or(plugin_small_model).filter(|model| {
        if model.provider_id != provider_id {
            notes.push(format!(
                "plugin small model `{}/{}` is served by another provider; using `{provider_id}/{model_id}` instead",
                model.provider_id, model.id
            ));
            return false;
        }
        if provider_factory_key(model.api.transport).is_none()
            || model_spec(catalog, model, env).is_err()
        {
            notes.push(format!(
                "plugin small model `{}/{}` cannot be reached; using `{provider_id}/{model_id}` instead",
                model.provider_id, model.id
            ));
            return false;
        }
        true
    });

    let mut resolved = std::collections::BTreeMap::new();
    for name in zuno_agent::builtin::INTERNAL_NAMES {
        let prompt = internal_prompt(name)?;
        let resolution = policy.resolve(name, &AnyModel);
        notes.extend(resolution.render_diagnostics());
        let chosen = if resolution.inherits_session_model() {
            inherited_small_model.map(|model| (model.id.clone(), model))
        } else {
            resolution.model.as_ref().and_then(|choice| {
                let (chosen_provider, chosen_model) = (choice.provider()?, choice.model_id()?);
                if chosen_provider != provider_id {
                    notes.push(format!(
                        "{name}: `{}` is served by `{chosen_provider}`, and only \
                         `{provider_id}`'s credential is wired for this turn; using \
                         `{provider_id}/{model_id}` instead",
                        choice.model
                    ));
                    return None;
                }
                let model = catalog.model(chosen_provider, chosen_model)?;
                if provider_factory_key(model.api.transport).is_none() {
                    notes.push(format!(
                        "{name}: `{}` has no native provider transport; using \
                         `{provider_id}/{model_id}` instead",
                        choice.model
                    ));
                    return None;
                }
                // Declined rather than fatal, for the same reason as the two above: a
                // per-model `provider.api` can leave one model in a provider without an
                // endpoint while the session's has one, and losing the whole turn over a
                // title agent is worse than downgrading it audibly.
                if let Err(why) = model_spec(catalog, model, env) {
                    notes.push(format!(
                        "{name}: `{}` cannot be reached ({why}); using \
                         `{provider_id}/{model_id}` instead",
                        choice.model
                    ));
                    return None;
                }
                Some((chosen_model.to_owned(), model))
            })
        }
        .unwrap_or_else(|| (model_id.to_owned(), session_model));
        let (chosen_model_id, catalog_model) = chosen;
        resolved.insert(
            name,
            InternalAgent {
                name: name.to_owned(),
                prompt,
                model: EngineModel::new(
                    model_spec(catalog, catalog_model, env)?,
                    catalog_model.api.id.clone(),
                    ApiSurface::Chat,
                )
                .with_catalog_identity(&catalog_model.provider_id, &catalog_model.id),
            },
        );
        debug_assert!(
            catalog.model(provider_id, &chosen_model_id).is_some(),
            "an internal resolved to a model the catalog does not carry"
        );
    }

    let take = |name: &str| -> Result<InternalAgent, String> {
        resolved
            .get(name)
            .cloned()
            .ok_or_else(|| format!("internal agent `{name}` did not resolve"))
    };
    Ok(Internals {
        title: take("title")?,
        compaction: take("compaction")?,
        summary: take("summary")?,
    })
}

/// Resolve reflection only onto an explicitly configured reachable small model.
///
/// Unlike title and compaction, reflection is optional background work. Falling
/// back to the session model would silently spend the user's most expensive model
/// after every cadence trigger, so absence disables the component with a visible
/// note instead.
fn resolve_reflection_model(
    config: &zuno_config::schema::Config,
    catalog: &Catalog,
    provider_id: &str,
    env: &zuno_paths::Env,
    notes: &mut Vec<String>,
) -> Result<Option<EngineModel>, String> {
    if !config.resolved_memory().reflection {
        return Ok(None);
    }
    let Some(qualified) = config.small_model.as_deref() else {
        notes.push(
            "memory reflection disabled: configure `small_model` to extract candidates without using the session model"
                .to_owned(),
        );
        return Ok(None);
    };
    let Some((small_provider, small_model)) = qualified.split_once('/') else {
        notes.push(format!(
            "memory reflection disabled: small_model must be provider/model, got `{qualified}`"
        ));
        return Ok(None);
    };
    if small_provider != provider_id {
        notes.push(format!(
            "memory reflection disabled: `{qualified}` uses `{small_provider}`, but this turn only wires `{provider_id}` credentials"
        ));
        return Ok(None);
    }
    let Some(model) = catalog.model(small_provider, small_model) else {
        notes.push(format!(
            "memory reflection disabled: small_model `{qualified}` is not in the resolved catalog"
        ));
        return Ok(None);
    };
    if provider_factory_key(model.api.transport).is_none() {
        notes.push(format!(
            "memory reflection disabled: small_model `{qualified}` has no native provider transport"
        ));
        return Ok(None);
    }
    let spec = match model_spec(catalog, model, env) {
        Ok(spec) => spec,
        Err(error) => {
            notes.push(format!(
                "memory reflection disabled: small_model `{qualified}` is unreachable ({error})"
            ));
            return Ok(None);
        }
    };
    Ok(Some(
        EngineModel::new(spec, model.api.id.clone(), ApiSurface::Chat)
            .with_catalog_identity(&model.provider_id, &model.id),
    ))
}

/// The upstream native's prompt for one internal agent.
///
/// Read through [`zuno_agent::builtin::internals`] rather than
/// [`zuno_catalog::agent::builtin`] directly, because the roster is what decides which
/// internals this build has — reading past it would let the two disagree about the
/// set while both looked correct.
fn internal_prompt(name: &str) -> Result<String, String> {
    zuno_agent::builtin::internals()
        .into_iter()
        .find(|agent| agent.name == name)
        .and_then(|agent| match agent.output {
            zuno_agent::builtin::OutputContract::EnginePrompt { prompt } => Some(prompt.to_owned()),
            _ => None,
        })
        .ok_or_else(|| format!("internal agent `{name}` declares no prompt"))
}

/// An open database, an assembled tool set, and the session a turn runs in.
pub(crate) struct TurnHost {
    profile_runtime: HarnessRuntime,
    runtime: HarnessRuntime,
    driver: Arc<dyn AgentDriver>,
    database: Arc<zuno_db::pool::Pool>,
    connection: rusqlite::Connection,
    inbox: zuno_db::inbox::SessionInbox,
    providers: ProviderRegistry,
    /// The credential this host presents, kept only so [`describe_turn_failure`]
    /// can prove it never echoes it.
    ///
    /// This is not a second copy of a secret: `providers` already closes over the
    /// same string, because that is how it authenticates. Holding it here names the
    /// one value the rendering seam must scrub, which is otherwise unknowable at
    /// the moment a failure is printed — see [`without_credential`].
    credential: Option<String>,
    resolver: Resolver,
    dispatcher: ToolRegistryDispatcher,
    tool_concurrency: ToolConcurrencyLimit,
    session_id: String,
    session_identity: PreparedSessionIdentity,
    session_directory: String,
    session_usage: zuno_db::session::SessionUsage,
    session_materializer: SessionMaterializer,
    /// The title the session already carried when this host opened it.
    ///
    /// A snapshot, deliberately not kept current: the only writer is the prelude, and a
    /// surface that watches [`TurnEvent::SessionTitled`] learns about that write the
    /// moment it happens. Refreshing this field afterwards would give a reader two ways
    /// to ask the same question and one of them would lag.
    ///
    /// Still the placeholder `create` invented for a session that has never been named,
    /// which is why the accessor filters rather than the field.
    session_title: String,
    agent: String,
    provider_id: String,
    model_id: String,
    extension_scope: zuno_extension::Scope,
    extension_revision: u64,
    extension_ownership: Option<ExtensionOwnership>,
    /// The reasoning level this host resolved with.
    ///
    /// Held here because the host is the source of truth every rebuild path already
    /// reads its model and agent back from (`refresh_mcp_host`). A level kept only in
    /// the launch options would be dropped by the next rebuild that did not carry it.
    effort: Option<zuno_llm::effort::ReasoningEffort>,
    internals: Internals,
    compaction_config: zuno_config::schema::CompactionConfig,
    compaction_state: CompactionState,
    window: TokenWindow,
    notes: Vec<String>,
    commands: zuno_catalog::command::Registry,
    goal_store: Arc<GoalStore>,
    goal_projection: GoalProjection,
    goal_continuation: GoalContinuation,
    runs: SessionRunRegistry,
    background_jobs: super::child_turn::BackgroundJobSupervisor,
    background_executions: Arc<zuno_pty::BackgroundExecutionService>,
    background_reports: super::child_turn::ChildSessionHost,
    product_agents: super::product_agent::NativeProductAgentHost,
    background_reports_recovered: bool,
    last_turn_completed: bool,
    title_sink: Option<Arc<dyn SessionTitleSink>>,
    work_changes: super::child_turn::ChangeNotifier,
    memory: Option<Arc<MemoryService>>,
    reflection: Option<ReflectionFork>,
}

enum ExtensionOwnership {
    Active(zuno_extension::CompositionLease),
    Prepared(zuno_extension::PreparedTransition),
}

impl ExtensionOwnership {
    fn release_after_clean_failure(self) -> Result<(), String> {
        match self {
            Self::Active(lease) => {
                drop(lease);
                Ok(())
            }
            Self::Prepared(transition) => transition.abort().map_err(to_string),
        }
    }

    fn mark_uncertain(self, message: String) {
        match self {
            Self::Active(lease) => lease.mark_uncertain(message),
            Self::Prepared(transition) => transition.mark_uncertain(message),
        }
    }
}

#[derive(Debug)]
enum TurnFailure {
    Engine(TurnError),
    Host(String),
    EventConsumer(String),
    GoalRecovery {
        message: String,
        failure: GoalTerminalFailure,
    },
}

impl TurnFailure {
    fn host(error: impl std::fmt::Display) -> Self {
        Self::Host(error.to_string())
    }

    fn event_consumer(error: impl std::fmt::Display) -> Self {
        Self::EventConsumer(error.to_string())
    }

    fn goal_recovery(message: impl Into<String>, failure: GoalTerminalFailure) -> Self {
        Self::GoalRecovery {
            message: message.into(),
            failure,
        }
    }

    fn rendered(&self, credential: Option<&str>) -> String {
        match self {
            Self::Engine(error) => describe_turn_failure(error, credential),
            Self::Host(message)
            | Self::EventConsumer(message)
            | Self::GoalRecovery { message, .. } => message.clone(),
        }
    }

    fn goal_failure(&self) -> GoalTerminalFailure {
        if let Self::GoalRecovery { failure, .. } = self {
            return *failure;
        }
        let recovery = match self {
            Self::Engine(error) => error.recovery(),
            Self::Host(_) => TurnRecovery::Fail,
            Self::EventConsumer(_) => TurnRecovery::Pause,
            Self::GoalRecovery { .. } => unreachable!("handled above"),
        };
        match recovery {
            TurnRecovery::Retry { reason, after } => GoalTerminalFailure::Retry {
                reason: GoalRetryReason::from(reason),
                retry_after: after,
            },
            TurnRecovery::Compact => GoalTerminalFailure::Retry {
                reason: GoalRetryReason::ContextLimit,
                retry_after: None,
            },
            TurnRecovery::Pause => GoalTerminalFailure::Pause,
            TurnRecovery::Fail => GoalTerminalFailure::Block,
        }
    }
}

/// Told when the prelude names the session, so a live surface can show the name.
///
/// A trait, and declared here rather than taking the TUI's projection directly, for the
/// reason [`RegistryProviders`] gives just below: this module is shared with `zuno run`,
/// which has no panel and must not acquire a view dependency to compile. The interactive
/// surface implements this over its own projection; the headless one supplies nothing and
/// loses nothing, because it already prints the name as a prelude note.
///
/// Synchronous and infallible on purpose. The implementation publishes to a lock and
/// nudges a channel, so there is no failure a turn could act on — and a title that could
/// not be shown must never be able to fail the turn that earned it.
pub(crate) trait SessionTitleSink: Send + Sync {
    /// Note that the session is now named `title`.
    fn publish(&self, title: &str);
}

impl MemoryObserver for super::child_turn::ChangeNotifier {
    fn changed(&self) {
        self.changed();
    }
}

/// The registry answering for whichever spec an internal agent resolved to.
///
/// A newtype rather than an `impl` on [`ProviderRegistry`] because the trait belongs
/// to `zuno-engine` and the registry to `zuno-llm`: neither crate may name the other's
/// concern, and this composition root is the one place that may name both.
struct RegistryProviders<'a>(&'a ProviderRegistry);

impl InternalProviders for RegistryProviders<'_> {
    fn provider_for(&self, agent: &InternalAgent) -> Result<Arc<dyn Provider>, String> {
        self.0
            .resolve(agent.model.provider.clone())
            .map_err(to_string)
    }
}

struct ProviderReflectionRunner {
    provider: Arc<dyn Provider>,
    model: EngineModel,
    events: zuno_db::event_log::SessionEventLog,
}

#[async_trait]
impl ReflectionRunner for ProviderReflectionRunner {
    async fn review(
        &self,
        request: ReflectionRequest,
        tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        let definition = tools.definition();
        let transcript = reflection_transcript_json(&request.transcript);
        let prompt_digest = sha256_hex(request.prompt.as_bytes());
        append_reflection_event(
            &self.events,
            &request.source_session_id,
            "memory.reflection.request",
            json!({
                "sourceMessageID": request.source_message_id,
                "model": {
                    "providerID": self.model.catalog_provider_id,
                    "modelID": self.model.catalog_model_id,
                    "wireID": self.model.model_id,
                },
                "prompt": request.prompt.as_ref(),
                "promptDigest": prompt_digest,
                "compaction": "disabled",
                "transcript": transcript,
                "tool": {
                    "name": &definition.id,
                    "description": &definition.description,
                    "parameters": &definition.parameters,
                }
            }),
        )?;

        let messages = reflection_messages(&request);
        let schema = ToolSchema {
            name: definition.id,
            description: definition.description,
            parameters: definition.parameters,
        };
        let mut stream = self.provider.stream(
            CompletionRequest::new(self.model.model_id.clone(), messages)
                .on_surface(self.model.surface)
                .with_tools(vec![schema]),
        );
        let mut accumulator =
            StreamAccumulator::for_stream(self.model.catalog_provider_id.clone(), "reflection");
        let mut saw_message_end = false;
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::Error { message, .. }) => {
                    return Err(record_reflection_failure(
                        &self.events,
                        &request.source_session_id,
                        message,
                    ));
                }
                Ok(event) => {
                    saw_message_end |= matches!(event, StreamEvent::MessageEnd { .. });
                    if let Err(error) = accumulator.apply(&event) {
                        return Err(record_reflection_failure(
                            &self.events,
                            &request.source_session_id,
                            error.to_string(),
                        ));
                    }
                }
                Err(error) => {
                    return Err(record_reflection_failure(
                        &self.events,
                        &request.source_session_id,
                        error.to_string(),
                    ));
                }
            }
        }
        drop(stream);
        if !saw_message_end {
            return Err(record_reflection_failure(
                &self.events,
                &request.source_session_id,
                "memory reflection stream ended before MessageEnd",
            ));
        }

        let calls = accumulator.tool_calls().to_vec();
        for call in &calls {
            let args = if call.raw_input.trim().is_empty() {
                json!({})
            } else {
                match serde_json::from_str(&call.raw_input) {
                    Ok(args) => args,
                    Err(error) => {
                        return Err(record_reflection_failure(
                            &self.events,
                            &request.source_session_id,
                            format!("memory reflection returned malformed tool arguments: {error}"),
                        ));
                    }
                }
            };
            if let Err(error) = tools
                .dispatch(zuno_agent::reflection::ReflectionToolCall::new(
                    call.id.clone(),
                    call.name.clone(),
                    args,
                ))
                .await
            {
                return Err(record_reflection_failure(
                    &self.events,
                    &request.source_session_id,
                    error.to_string(),
                ));
            }
        }
        append_reflection_event(
            &self.events,
            &request.source_session_id,
            "memory.reflection.outcome",
            json!({
                "status": "completed",
                "toolCalls": calls.len(),
                "text": accumulator.text(),
            }),
        )?;
        Ok(())
    }
}

fn record_reflection_failure(
    events: &zuno_db::event_log::SessionEventLog,
    session_id: &str,
    detail: impl Into<String>,
) -> ReflectionError {
    let detail = detail.into();
    match append_reflection_event(
        events,
        session_id,
        "memory.reflection.outcome",
        json!({"status":"failed","error":&detail}),
    ) {
        Ok(()) => ReflectionError::new(detail),
        Err(event_error) => ReflectionError::new(format!(
            "{detail}; failed to persist reflection outcome: {event_error}"
        )),
    }
}

fn sha256_hex(input: &[u8]) -> String {
    Sha256::digest(input)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn append_reflection_event(
    events: &zuno_db::event_log::SessionEventLog,
    session_id: &str,
    event_type: &str,
    properties: Value,
) -> Result<(), ReflectionError> {
    let properties = properties
        .as_object()
        .cloned()
        .ok_or_else(|| ReflectionError::new("reflection event payload is not an object"))?;
    let event = zuno_db::event_log::NewSessionEvent::new(event_type, properties)
        .map_err(|error| ReflectionError::new(error.to_string()))?;
    events
        .append(session_id, event)
        .map(|_| ())
        .map_err(|error| ReflectionError::new(error.to_string()))
}

fn reflection_messages(request: &ReflectionRequest) -> Vec<ProviderMessage> {
    let mut messages = vec![ProviderMessage::new(
        Role::System,
        request.prompt.to_string(),
    )];
    for event in request.transcript.events() {
        match event {
            TranscriptEvent::User { text } => {
                messages.push(ProviderMessage::new(Role::User, text.clone()));
            }
            TranscriptEvent::Assistant { text } => {
                messages.push(ProviderMessage::new(Role::Assistant, text.clone()));
            }
            TranscriptEvent::Command { command, outcome } => {
                let (status, output) = match outcome {
                    CommandOutcome::Succeeded { output } => ("succeeded", output),
                    CommandOutcome::Failed { output } => ("failed", output),
                };
                messages.push(ProviderMessage::new(
                    Role::User,
                    format!("Command `{command}` {status}:\n{output}"),
                ));
            }
        }
    }
    messages
}

fn reflection_transcript_json(transcript: &TurnTranscript) -> Vec<Value> {
    transcript
        .events()
        .iter()
        .map(|event| match event {
            TranscriptEvent::User { text } => json!({"type":"user","text":text}),
            TranscriptEvent::Assistant { text } => json!({"type":"assistant","text":text}),
            TranscriptEvent::Command { command, outcome } => match outcome {
                CommandOutcome::Succeeded { output } => {
                    json!({"type":"command","command":command,"status":"succeeded","output":output})
                }
                CommandOutcome::Failed { output } => {
                    json!({"type":"command","command":command,"status":"failed","output":output})
                }
            },
        })
        .collect()
}

fn durable_reflection_transcript(
    connection: &rusqlite::Connection,
    session_id: &str,
    assistant_message_id: &str,
) -> Result<TurnTranscript, String> {
    let history = zuno_db::message::MessageStore::new(connection)
        .hydrate_session(session_id)
        .map_err(to_string)?;
    let assistant_index = history
        .iter()
        .position(|message| message.info.id == assistant_message_id)
        .ok_or_else(|| {
            format!(
                "completed assistant message `{assistant_message_id}` is missing from durable history"
            )
        })?;
    let start = history[..assistant_index]
        .iter()
        .rposition(|message| message.info.role == zuno_db::message::MessageRole::User)
        .unwrap_or(assistant_index);
    let mut events = Vec::new();
    for message in &history[start..=assistant_index] {
        for part in &message.parts {
            match (message.info.role, part.kind) {
                (zuno_db::message::MessageRole::User, zuno_db::message::PartKind::Text) => {
                    if let Some(text) = part.data.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        events.push(TranscriptEvent::user(text));
                    }
                }
                (zuno_db::message::MessageRole::Assistant, zuno_db::message::PartKind::Text) => {
                    if let Some(text) = part.data.get("text").and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        events.push(TranscriptEvent::assistant(text));
                    }
                }
                (zuno_db::message::MessageRole::Assistant, zuno_db::message::PartKind::Tool) => {
                    if let Some(event) = reflection_tool_event(&part.data) {
                        events.push(event);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(TurnTranscript::new(events))
}

fn reflection_tool_event(data: &serde_json::Map<String, Value>) -> Option<TranscriptEvent> {
    let tool = data.get("tool")?.as_str()?;
    let state = data.get("state")?.as_object()?;
    let status = state.get("status")?.as_str()?;
    let output = state
        .get("output")
        .or_else(|| state.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let input = state.get("input");
    let command = if tool == "bash" {
        input
            .and_then(Value::as_object)
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| "bash".to_owned())
    } else {
        input.map_or_else(|| tool.to_owned(), |input| format!("{tool} {}", input))
    };
    match status {
        "completed" => Some(TranscriptEvent::command(
            command,
            CommandOutcome::succeeded(output),
        )),
        "error" => Some(TranscriptEvent::command(
            command,
            CommandOutcome::failed(output),
        )),
        _ => None,
    }
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
    /// The headless entry point: no live user to ask, one optional MCP catalog.
    ///
    /// There was an `open` beside this taking no catalog at all, and `zuno run` called
    /// it — which is how the same configuration produced MCP tools in the TUI and none
    /// headlessly. It is gone rather than deprecated: a constructor that silently
    /// drops a capability is one a future caller reaches for again.
    pub(crate) async fn open_with_mcp(
        plan: TurnPlan,
        environment: &StartupEnvironment,
        approval: Arc<dyn PermissionAsker>,
        mcp: Option<zuno_mcp::Catalog>,
    ) -> Result<Self, String> {
        Self::open_with_runtime_and_mcp(
            plan,
            environment,
            approval,
            None,
            SessionRunRegistry::new(),
            mcp,
        )
        .await
    }

    /// The full constructor. **Every** surface reaches this one.
    ///
    /// There is deliberately no wrapper that defaults `mcp` away. Two existed — `open`
    /// and `open_with_runtime` — and between them they were how `zuno run` and
    /// `zuno serve` came to advertise fewer tools than `zuno tui` from identical
    /// configuration, with no error on either path. A surface that genuinely wants no
    /// MCP passes `None` here and says so at its own call site.
    pub(crate) async fn open_with_runtime_and_mcp(
        mut plan: TurnPlan,
        environment: &StartupEnvironment,
        approval: Arc<dyn PermissionAsker>,
        question: Option<Arc<dyn zuno_tools::question::QuestionAsker>>,
        runs: SessionRunRegistry,
        mcp: Option<zuno_mcp::Catalog>,
    ) -> Result<Self, String> {
        let database = match zuno_db::pool::Pool::open_default() {
            Ok(database) => Arc::new(database),
            Err(error) => {
                let error = to_string(error);
                return match plan.abort_extension_candidate(environment.extensions()) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(format!(
                        "{error}; extension candidate abort also failed: {cleanup}"
                    )),
                };
            }
        };
        Self::open_with_runtime_mcp_and_database(
            plan,
            environment,
            approval,
            question,
            runs,
            mcp,
            database,
        )
        .await
    }

    pub(super) async fn open_with_runtime_mcp_and_database(
        mut plan: TurnPlan,
        environment: &StartupEnvironment,
        approval: Arc<dyn PermissionAsker>,
        question: Option<Arc<dyn zuno_tools::question::QuestionAsker>>,
        runs: SessionRunRegistry,
        mcp: Option<zuno_mcp::Catalog>,
        database: Arc<zuno_db::pool::Pool>,
    ) -> Result<Self, String> {
        let mut extension_ownership = Some(match plan.extension_prepared.take() {
            Some(prepared) => ExtensionOwnership::Prepared(prepared),
            None => match plan.extension_transaction.take() {
                Some(transaction) => ExtensionOwnership::Prepared(
                    environment
                        .extensions()
                        .begin_transition(&transaction)
                        .map_err(to_string)?,
                ),
                None => ExtensionOwnership::Active(
                    environment
                        .extensions()
                        .acquire_active(&plan.extension_scope, plan.extension_revision)
                        .map_err(to_string)?,
                ),
            },
        });
        let env = environment.resolved();
        let worktree = plan
            .project
            .vcs
            .as_ref()
            .map(|_| plan.project.directory.clone());
        let presented = plan.credential.as_ref().map(credential_value);
        let providers = provider_registry(
            &plan.provider_id,
            plan.credential.clone(),
            Some(plan.auth_store.clone()),
        );

        let prepared_session = (|| -> Result<_, String> {
            let mut connection = database.open_connection().map_err(to_string)?;
            zuno_db::migration::apply(&mut connection).map_err(to_string)?;
            let now = zuno_db::message::now_millis();
            ensure_project(&connection, &plan.project, now)?;
            let prepared = prepare_turn_host(&connection, &plan, now)?;
            Ok((connection, prepared))
        })();
        let (connection, prepared) = match prepared_session {
            Ok(prepared) => prepared,
            Err(error) => {
                let ownership = extension_ownership
                    .take()
                    .expect("extension ownership exists before session preparation");
                return match ownership.release_after_clean_failure() {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(format!(
                        "{error}; extension transition abort also failed: {cleanup}"
                    )),
                };
            }
        };
        let profile_runtime = HarnessRuntime::new("profile");
        let profile = plan.profile;
        if let Err(error) = profile_runtime.activate_profile(profile).await {
            let shutdown = profile_runtime.shutdown().await;
            let ownership = extension_ownership
                .take()
                .expect("extension ownership exists before host assembly");
            return match shutdown {
                Ok(()) => match ownership.release_after_clean_failure() {
                    Ok(()) => Err(to_string(error)),
                    Err(cleanup) => Err(format!(
                        "{error}; extension transition abort also failed: {cleanup}"
                    )),
                },
                Err(shutdown) => {
                    ownership.mark_uncertain(format!(
                        "profile activation failed ({error}) and cleanup was not authoritative \
                         ({shutdown})"
                    ));
                    Err(format!(
                        "profile activation failed: {error}; profile cleanup failed: {shutdown}"
                    ))
                }
            };
        }
        let assembled = (|| -> Result<Self, String> {
            let _profile_id = profile_runtime
                .active_profile_id()
                .ok_or_else(|| "profile runtime has no active profile".to_owned())?;
            let runtime = profile_runtime.child(format!("session:{}", prepared.identity.id()));
            let driver = runtime
                .service::<dyn AgentDriver>()
                .ok_or_else(|| "profile did not register an agent driver".to_owned())?;
            let tool_manifest = runtime
                .service::<zuno_harness::ToolManifest>()
                .ok_or_else(|| "profile did not register a tool manifest".to_owned())?;
            let tool_contributions = runtime
                .service::<zuno_harness::ToolContributions>()
                .ok_or_else(|| "profile did not register tool contributions".to_owned())?;
            let todo_store = Arc::clone(&database);
            let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&todo_store));
            let goal_store = Arc::new(GoalStore::open_default().map_err(to_string)?);
            let goal_projection = GoalProjection::new(worktree.as_deref(), prepared.identity.id())
                .ok_or_else(|| {
                    format!(
                        "session id `{}` cannot name a goal projection",
                        prepared.identity.id()
                    )
                })?;
            let goal_continuation = GoalContinuation::new(Arc::clone(&goal_store), runs.clone())
                .with_retry_policy(plan.goal_retry_policy);
            let active_goal = if prepared.identity.is_materialized() {
                goal_store
                    .goal(prepared.identity.id())
                    .map_err(to_string)?
                    .is_some_and(|goal| goal.status == zuno_goal::GoalStatus::Active)
            } else {
                false
            };

            let memory_root = worktree.as_deref().unwrap_or(&plan.directory);
            let command_worktree = memory_root.to_string_lossy();
            let discovered_commands =
                zuno_catalog::command::load_map(&plan.directory, worktree.as_deref(), env)
                    .map_err(to_string)?;
            let configured_commands = match plan.config.command.as_ref() {
                Some(config) => {
                    zuno_catalog::command::merge_command_maps(&discovered_commands, config)
                        .map_err(to_string)?
                }
                None => discovered_commands,
            };
            let extension_commands = zuno_catalog::command::merge_command_maps(
                &configured_commands,
                plan.extensions.workflows(),
            )
            .map_err(to_string)?;
            let mcp_prompts = mcp
                .as_ref()
                .map_or_else(Vec::new, zuno_mcp::Catalog::prompts);
            let commands = zuno_catalog::command::Registry::build(
                &zuno_catalog::command::Sources::new(&command_worktree)
                    .with_config(Some(&extension_commands))
                    .with_mcp_prompts(&mcp_prompts),
            );
            let memory_settings = plan.config.resolved_memory();
            let memory_paths = ScopePaths::discover(memory_root);
            configure_resident_memory(&mut plan.resolver, &plan.config, memory_paths.clone())?;
            let background_jobs = environment.background_jobs(&plan.directory);
            let work_changes = background_jobs.notifier();
            let memory = if memory_settings.enabled {
                let promotion = match memory_settings.promotion {
                    zuno_config::schema::MemoryPromotion::Review => PromotionPolicy::Review,
                    zuno_config::schema::MemoryPromotion::HighConfidence => {
                        PromotionPolicy::HighConfidence {
                            threshold: (memory_settings.auto_confidence * 10_000.0).round() as u16,
                        }
                    }
                    zuno_config::schema::MemoryPromotion::Automatic => PromotionPolicy::Automatic,
                };
                let service = Arc::new(
                    MemoryService::new(
                        Arc::clone(&database),
                        memory_paths,
                        ScopeLimits::new(
                            memory_settings.global_char_limit,
                            memory_settings.project_char_limit,
                        ),
                        promotion,
                    )
                    .with_observer(Arc::new(work_changes.clone())),
                );
                service.reconcile().map_err(to_string)?;
                Some(service)
            } else {
                None
            };
            let memory_tool = memory
                .as_ref()
                .filter(|_| memory_settings.tool)
                .map(|service| erase(zuno_tools::MemoryTool::new(Arc::clone(service))));
            let mut notes = plan.notes;
            let reflection = match (memory.as_ref(), plan.reflection_model.take()) {
                (Some(service), Some(model)) => match providers.resolve(model.provider.clone()) {
                    Ok(provider) => Some(ReflectionFork::new(
                        ReflectionConfig {
                            enabled: memory_settings.reflection,
                            turn_interval: memory_settings.nudge_interval,
                        },
                        Arc::new(ProviderReflectionRunner {
                            provider,
                            model,
                            events: zuno_db::event_log::SessionEventLog::new(Arc::clone(&database)),
                        }),
                        erase(zuno_tools::MemoryTool::reflection(Arc::clone(service))),
                    )),
                    Err(error) => {
                        notes.push(format!(
                        "memory reflection disabled: small-model provider could not start ({error})"
                    ));
                        None
                    }
                },
                _ => None,
            };
            plan.resolver.append_prompt_section(
                "extensions",
                "zuno-extension::active-packages",
                plan.extensions.prompt_section(),
            )?;
            announce_instructions(&mut plan.resolver, &plan.instructions, &mut notes)?;
            announce_skills(&mut plan.resolver, &plan.skills, &mut notes)?;
            let child_host =
                super::child_turn::ChildSessionHost::new(super::child_turn::ChildSessionContext {
                    database: Arc::clone(&database),
                    environment: environment.clone(),
                    directory: plan.directory.clone(),
                    approval: Arc::clone(&approval),
                    question: question.clone(),
                    runs: runs.clone(),
                    mcp: mcp.clone(),
                    parent_agent: plan.agent.name.clone(),
                    parent_model: format!("{}/{}", plan.provider_id, plan.model_id),
                    parent_effort: plan.effort,
                    supervisor: background_jobs.clone(),
                })?;
            let product_agents = super::product_agent::NativeProductAgentHost::new(
                &plan.config,
                env,
                plan.directory.clone(),
                Arc::clone(&database),
                child_host.wake_handle(),
                background_jobs.clone(),
            )?;
            let background_executions = environment
                .background_executions(&plan.directory)
                .map_err(to_string)?;
            let delegation_agents = delegation_agents(&plan.agents, plan.vision_available)?;

            let runtime_tools = super::tool_runtime::assemble(
                &plan.directory,
                worktree.as_deref(),
                env,
                &plan.config,
                &plan.agent,
                super::tool_runtime::ToolSelection {
                    provider_id: &plan.provider_id,
                    model_id: &plan.model_id,
                    manifest: tool_manifest,
                    contributions: tool_contributions,
                    question,
                    background_executions: Arc::clone(&background_executions),
                    todo_store,
                    goal_store: Arc::clone(&goal_store),
                    mcp_loader: mcp.map(|catalog| {
                        Arc::new(catalog.loader()) as Arc<dyn zuno_tools::registry::McpToolLoader>
                    }),
                    skills: Arc::clone(&plan.skills),
                    delegation: super::tool_runtime::Delegation {
                        host: Arc::new(child_host.clone()),
                        facts: Arc::clone(&plan.delegation_facts)
                            as Arc<dyn zuno_tools::task::ProviderFacts>,
                        targets: delegation_agents.targets,
                        agent_models: delegation_agents.models,
                        session_model: zuno_agent::model_policy::ModelChoice::new(format!(
                            "{}/{}",
                            plan.provider_id, plan.model_id
                        )),
                        limits: zuno_tools::task::DelegationLimits {
                            subagent_depth: plan
                                .config
                                .subagent_depth
                                .unwrap_or(zuno_tools::task::DEFAULT_SUBAGENT_DEPTH),
                        },
                        vision_available: plan.vision_available,
                    },
                    product_agents: Arc::new(product_agents.clone()),
                    job_controller: Arc::new(background_jobs.clone()),
                    memory: memory_tool,
                },
            )?;
            // Joins the notes so shadowing reaches whatever surface is watching: the
            // headless runs print them, and the TUI draws them in the transcript. This is
            // what replaces the registry's own `eprintln!` without going quiet.
            notes.extend(
                runtime_tools
                    .suppressions
                    .iter()
                    .map(|suppression| format!("warning: {suppression}")),
            );
            let dispatcher = ToolRegistryDispatcher::new(
                runtime_tools.tools,
                runtime_tools.rules,
                approval,
                AuthorizationPolicy::from_strict(plan.config.strict_authorization()),
                McpToolStatus::Ready,
            );
            let tool_concurrency =
                ToolConcurrencyLimit::new(plan.config.resolved_concurrency().tool_calls)
                    .expect("configuration validates tool concurrency");
            Ok(Self {
                profile_runtime: profile_runtime.clone(),
                runtime,
                driver,
                database,
                connection,
                inbox,
                providers,
                credential: presented,
                resolver: plan.resolver,
                dispatcher,
                tool_concurrency,
                session_id: prepared.identity.id().to_owned(),
                session_identity: prepared.identity,
                session_directory: prepared.directory,
                session_usage: prepared.usage,
                session_materializer: prepared.materializer,
                session_title: prepared.title,
                agent: plan.agent.name,
                provider_id: plan.provider_id,
                model_id: plan.model_id,
                extension_scope: plan.extension_scope,
                extension_revision: plan.extension_revision,
                extension_ownership: extension_ownership.take(),
                effort: plan.effort,
                internals: plan.internals,
                compaction_config: plan.config.compaction.clone().unwrap_or_default(),
                compaction_state: CompactionState::default(),
                window: plan.window,
                notes,
                commands,
                goal_store,
                goal_projection,
                goal_continuation,
                runs,
                background_jobs,
                background_executions,
                background_reports: child_host,
                product_agents,
                background_reports_recovered: false,
                last_turn_completed: active_goal,
                title_sink: None,
                work_changes,
                memory,
                reflection,
            })
        })();
        match assembled {
            Ok(host) => Ok(host),
            Err(error) => {
                let shutdown = profile_runtime.shutdown().await;
                let ownership = extension_ownership
                    .take()
                    .expect("failed host assembly retains extension ownership");
                match shutdown {
                    Ok(()) => match ownership.release_after_clean_failure() {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(format!(
                            "{error}; extension transition abort also failed: {cleanup}"
                        )),
                    },
                    Err(shutdown) => {
                        ownership.mark_uncertain(format!(
                            "turn host assembly failed ({error}) and profile cleanup was not \
                             authoritative ({shutdown})"
                        ));
                        Err(format!(
                            "{error}; profile cleanup after host assembly failure also failed: \
                             {shutdown}"
                        ))
                    }
                }
            }
        }
    }

    /// Report generated session names to `sink` as well as to the transcript.
    pub(crate) fn set_title_sink(&mut self, sink: Arc<dyn SessionTitleSink>) {
        self.title_sink = Some(sink);
    }

    /// The session every turn this host drives belongs to.
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Stable identity used by the TUI's exit handoff and rebuild paths.
    pub(crate) fn session_identity(&self) -> PreparedSessionIdentity {
        self.session_identity.clone()
    }

    /// Session choice that preserves a pending identity across a host rebuild.
    pub(crate) fn rebuild_session_choice(&self) -> SessionChoice {
        if self.session_identity.is_materialized() {
            SessionChoice::Existing(self.session_id.clone())
        } else {
            SessionChoice::Prepared(self.session_identity.clone())
        }
    }

    /// Whether this host already has a durable session row.
    pub(crate) fn is_session_materialized(&self) -> bool {
        self.session_identity.is_materialized()
    }

    /// Durable usage restored with an existing session.
    pub(crate) const fn session_usage(&self) -> zuno_db::session::SessionUsage {
        self.session_usage
    }

    /// Recent active root sessions in the directory this host is already running in.
    ///
    /// The current row supplies the directory rather than the process cwd. That keeps a
    /// TUI resumed with `--session` scoped to the session it actually opened, and it
    /// prevents a picker selection from crossing into a different configuration,
    /// worktree, LSP, or snapshot composition.
    pub(super) fn recent_sessions(
        &self,
        limit: u32,
    ) -> Result<Vec<zuno_db::session::Session>, zuno_error::DbError> {
        recent_sessions(&self.connection, &self.session_directory, limit)
    }

    /// Resolve one picker target while rechecking the provider's scope rules.
    ///
    /// A picker result is an internal message, but it is still stale input by the time
    /// the host consumes it: another process may have archived the row, and a future
    /// client could send an identifier that was never offered. Revalidate the target at
    /// the consumer boundary so session switching cannot cross a directory or enter a
    /// child/archived session merely because the view was out of date.
    pub(super) fn switchable_session(
        &self,
        session_id: &str,
    ) -> Result<Option<zuno_db::session::Session>, zuno_error::DbError> {
        switchable_session(&self.connection, &self.session_directory, session_id)
    }

    /// Rename a session through the same transactional store used by every other surface.
    pub(super) fn rename_session(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<i64, zuno_error::DbError> {
        zuno_db::session::Store::new(&self.database).set_title(session_id, title)
    }

    /// Delete a session and its complete child subtree through the durable store.
    pub(super) fn delete_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<String>, zuno_error::DbError> {
        zuno_db::session::Store::new(&self.database).remove(session_id)
    }

    /// Whether deleting this host's session would race a background subagent write.
    pub(super) fn has_running_background_tasks(&self) -> bool {
        self.background_jobs.has_running_tasks(&self.session_id)
    }

    pub(super) fn background_executions(&self) -> Arc<zuno_pty::BackgroundExecutionService> {
        Arc::clone(&self.background_executions)
    }

    /// Ask the live executor to cancel one job owned by this session.
    pub(super) async fn cancel_job(
        &mut self,
        job_id: &str,
    ) -> Result<zuno_tools::job_cancel::CancelOutcome, String> {
        let store = zuno_db::job::AgentJobStore::new(Arc::clone(&self.database));
        let job = store.get(job_id).map_err(to_string)?;
        if job.parent_session_id != self.session_id {
            return Err(format!(
                "job `{job_id}` is not owned by session `{}`",
                self.session_id
            ));
        }
        if job.status.is_terminal() {
            return Ok(zuno_tools::job_cancel::CancelOutcome {
                requested: false,
                message: format!("job `{job_id}` is already terminal"),
            });
        }
        zuno_tools::job_cancel::JobController::cancel(
            &self.background_jobs,
            &self.session_id,
            job_id,
        )
        .await
    }

    pub(super) fn work_state(&self) -> Result<zuno_types::WorkStateProjection, String> {
        let goal = self
            .goal_store
            .goal(&self.session_id)
            .map_err(to_string)?
            .map(|goal| zuno_types::GoalStateProjection {
                objective: goal.objective,
                status: goal.status.as_str().to_owned(),
                tokens_used: goal.tokens_used,
                token_budget: goal.token_budget,
            });
        let todos = zuno_tools::todo::TodoStore::list(
            &zuno_tools::todo::SqliteTodoStore::new(Arc::clone(&self.database)),
            &self.session_id,
        )
        .map_err(to_string)?
        .into_iter()
        .map(|todo| zuno_types::TodoProjection {
            content: todo.content,
            status: todo.status.as_str().to_owned(),
            priority: todo.priority.as_str().to_owned(),
        })
        .collect();
        let jobs = zuno_db::job::AgentJobStore::new(Arc::clone(&self.database))
            .list_for_parent(&self.session_id)
            .map_err(to_string)?
            .into_iter()
            .map(|job| {
                let subject = match job.subject {
                    zuno_db::job::JobSubject::ChildSession { session_id } => {
                        format!("child {session_id}")
                    }
                    zuno_db::job::JobSubject::ProductAgent {
                        product, instance, ..
                    } => format!("{product} · {instance}"),
                };
                zuno_types::JobProjection {
                    id: job.id,
                    subject,
                    status: job.status.as_str().to_owned(),
                    report_delivery: job.report_delivery.as_str().to_owned(),
                }
            })
            .collect();
        let (memory_candidates, memory_entries) = match &self.memory {
            Some(memory) => (
                memory.candidates().map_err(to_string)?,
                memory.entries().map_err(to_string)?,
            ),
            None => (Vec::new(), Vec::new()),
        };
        Ok(zuno_types::WorkStateProjection {
            goal,
            todos,
            jobs,
            memory_candidates,
            memory_entries,
        })
    }

    pub(super) fn work_state_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.work_changes.subscribe()
    }

    pub(super) fn memory_apply(&self, id: &str) -> Result<(), String> {
        self.memory_service()?
            .apply(id)
            .map(|_| ())
            .map_err(to_string)
    }

    pub(super) fn memory_reject(&self, id: &str) -> Result<(), String> {
        self.memory_service()?
            .reject(id)
            .map(|_| ())
            .map_err(to_string)
    }

    pub(super) fn memory_undo(&self, id: &str) -> Result<(), String> {
        self.memory_service()?
            .undo(id)
            .map(|_| ())
            .map_err(to_string)
    }

    pub(super) fn memory_edit_and_apply(&self, id: &str, content: String) -> Result<(), String> {
        let service = self.memory_service()?;
        let candidate = service
            .candidates()
            .map_err(to_string)?
            .into_iter()
            .find(|candidate| candidate.id == id)
            .ok_or_else(|| format!("memory candidate `{id}` no longer exists"))?;
        service
            .edit(
                id,
                Some(content),
                candidate.old_text,
                candidate.reason,
                f64::from(candidate.confidence) / 10_000.0,
            )
            .map_err(to_string)?;
        service.apply(id).map(|_| ()).map_err(to_string)
    }

    pub(super) fn memory_remove(
        &self,
        scope: zuno_types::MemoryScope,
        content: String,
    ) -> Result<(), String> {
        self.memory_service()?
            .remove_entry(
                scope,
                content,
                "removed by the user from /memory".to_owned(),
                Some(self.session_id.clone()),
            )
            .map(|_| ())
            .map_err(to_string)
    }

    fn memory_service(&self) -> Result<&MemoryService, String> {
        self.memory
            .as_deref()
            .ok_or_else(|| "resident memory is disabled".to_owned())
    }

    /// The active harness profile that assembled this session.
    pub(crate) fn lifecycle_snapshots(&self) -> [zuno_runtime::RuntimeSnapshot; 2] {
        [self.profile_runtime.snapshot(), self.runtime.snapshot()]
    }

    /// The name a resumed session already had, or [`None`] for one never named.
    ///
    /// Filtered through [`zuno_db::session::is_default_title`] rather than returned raw,
    /// because the column is `NOT NULL` and a brand-new session holds
    /// `New session - <instant>` — a machine-generated placeholder that would read as a
    /// real name on any surface that displays it, and would then be replaced by the
    /// generated title a moment later. One predicate, shared with the generator that
    /// decides whether to spend a request, so a title this answers `None` for is exactly
    /// a title the prelude is about to write.
    pub(crate) fn session_title(&self) -> Option<&str> {
        if !self.is_session_materialized() {
            return None;
        }
        if zuno_db::session::is_default_title(&self.session_title) {
            return None;
        }
        Some(&self.session_title)
    }

    /// The persisted history a resumed session opens with, exactly as the model will get it.
    ///
    /// [`zuno_engine::r#loop::hydrate_retained_history`] and not a second query, and that
    /// is the whole point: it is the *same* function `run_turn` calls before it builds a
    /// request, so the rows, their `(time_created, id)` order and the compaction boundary
    /// are one decision rather than two that can drift. A surface reading
    /// [`zuno_db::message::MessageStore::hydrate_session`] instead would show a compacted
    /// head the model has already forgotten — the same class of lie as showing nothing,
    /// pointed the other way.
    ///
    /// Returned as stored rows rather than as view messages because this module is shared
    /// with `zuno run`, which has no panel; the projection onto transcript parts lives in
    /// `tui_replay`.
    ///
    /// # Errors
    ///
    /// Any query or decode failure from the two phases of the hydration. A caller reports
    /// it and opens the session anyway — see `tui_replay::replay_notice`.
    pub(crate) fn resumed_history(
        &self,
    ) -> Result<Vec<zuno_db::message::MessageWithParts>, zuno_error::DbError> {
        if !self.is_session_materialized() {
            return Ok(Vec::new());
        }
        zuno_engine::r#loop::hydrate_retained_history(&self.connection, &self.session_id)
    }

    /// Carry notes a caller produced *before* the host existed onto the transcript.
    ///
    /// An MCP server that fails to connect is discovered before the host is built —
    /// the catalog has to be populated first — so its note has nowhere to go until
    /// there is a host to report through. Dropping it instead is this defect class
    /// exactly: a server whose tools are silently absent.
    pub(crate) fn push_notes(&mut self, notes: impl IntoIterator<Item = String>) {
        self.notes.extend(notes);
    }

    /// How many tools this session offers the model.
    ///
    /// Read off the assembled dispatcher rather than recomputed, so the figure a
    /// surface shows is the one the model is actually given — the two diverging is how
    /// a tool count becomes a claim nobody can act on.
    pub(crate) fn tool_count(&self) -> usize {
        use zuno_engine::r#loop::ToolDispatcher as _;
        self.dispatcher.available_tools().definitions.len()
    }

    pub(crate) fn agent_name(&self) -> &str {
        &self.agent
    }

    pub(crate) fn qualified_model(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    /// Extension revision this host was assembled against.
    pub(crate) const fn extension_revision(&self) -> u64 {
        self.extension_revision
    }

    pub(crate) fn extension_scope(&self) -> &zuno_extension::Scope {
        &self.extension_scope
    }

    /// Publish a desired extension composition only after the old owner is gone.
    pub(crate) fn activate_extension_composition(&mut self) -> Result<(), String> {
        let Some(ownership) = self.extension_ownership.take() else {
            return Err("turn host has no extension composition ownership".to_owned());
        };
        self.extension_ownership = Some(match ownership {
            ExtensionOwnership::Active(lease) => ExtensionOwnership::Active(lease),
            ExtensionOwnership::Prepared(transition) => {
                ExtensionOwnership::Active(transition.commit().map_err(to_string)?)
            }
        });
        Ok(())
    }

    fn require_active_extension_composition(&self) -> Result<(), String> {
        match self.extension_ownership {
            Some(ExtensionOwnership::Active(_)) => Ok(()),
            Some(ExtensionOwnership::Prepared(_)) => Err(
                "turn host cannot run before its prepared extension composition is committed"
                    .to_owned(),
            ),
            None => Err("turn host extension composition is already released".to_owned()),
        }
    }

    /// The reasoning level this host resolved with.
    pub(crate) const fn effort(&self) -> Option<zuno_llm::effort::ReasoningEffort> {
        self.effort
    }

    /// Commands available to interactive discovery, in catalog listing order.
    pub(crate) fn commands(&self) -> impl Iterator<Item = &zuno_catalog::command::Info> {
        self.commands.list()
    }

    /// A handle that aborts whichever turn this host has live.
    ///
    /// Resolving the live turn by session id rather than capturing a signal is what
    /// lets the TUI hold one handle across every turn it drives — see
    /// [`SessionRunRegistry::control`].
    pub(crate) fn control(&self) -> zuno_engine::status::SessionControl {
        self.runs.control(self.session_id.clone())
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), String> {
        let session = self.runtime.shutdown().await.map_err(to_string);
        let profile = self.profile_runtime.shutdown().await.map_err(to_string);
        let outcome = match (session, profile) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(session), Ok(())) => Err(format!("session runtime shutdown failed: {session}")),
            (Ok(()), Err(profile)) => Err(format!("profile runtime shutdown failed: {profile}")),
            (Err(session), Err(profile)) => Err(format!(
                "session runtime shutdown failed: {session}; profile runtime shutdown failed: \
                 {profile}"
            )),
        };
        match outcome {
            Ok(()) => match self.extension_ownership.take() {
                Some(ExtensionOwnership::Active(lease)) => {
                    drop(lease);
                    Ok(())
                }
                Some(ExtensionOwnership::Prepared(transition)) => {
                    transition.abort().map_err(to_string)
                }
                None => Ok(()),
            },
            Err(error) => {
                if let Some(ownership) = self.extension_ownership.take() {
                    ownership.mark_uncertain(error.clone());
                }
                Err(error)
            }
        }
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
    /// # The prelude runs between the prompt and the turn, in that order
    ///
    /// The `title` internal names a session from its opening exchange, so it needs the
    /// prompt already stored; the turn needs whatever the `compaction` internal
    /// decided, so it has to run after both. Sequencing rather than forking — upstream
    /// forks the title (`session/prompt.ts:1133-1138`) — makes the number of provider
    /// requests per turn a fact instead of a race, which is what lets the perf
    /// harness's `completed_tool_turns` arithmetic mean anything.
    ///
    /// This is also why the prelude cannot disturb todo 31's append-only cache
    /// tracker: [`run_turn`] builds a fresh [`zuno_llm::cache::PromptCache`] per call
    /// and every prelude write lands before the first `prepare_turn`, so the tracker
    /// only ever sees one prefix. A prelude folded into the loop's continuation would
    /// change the prefix between step 1 and step 2, which is exactly the violation
    /// `zuno-llm` refuses.
    ///
    /// # Errors
    ///
    /// Returns a message when the prompt cannot be persisted, the prelude cannot read
    /// the session, or the turn fails.
    pub(crate) async fn drive(
        &mut self,
        prompt: &str,
        events: TurnEventSender,
    ) -> Result<(), String> {
        self.drive_with_message_id(prompt, None, events).await
    }

    pub(crate) async fn drive_content(
        &mut self,
        prompt: &str,
        content: &[RequestContentBlock],
        events: TurnEventSender,
    ) -> Result<(), String> {
        let guard = self
            .runs
            .begin_turn(self.session_id.clone())
            .map_err(to_string)?;
        self.drive_input(prompt, None, Some(content), &guard, events)
            .await
    }

    pub(crate) async fn drive_command(
        &mut self,
        command: &str,
        arguments: &str,
        events: TurnEventSender,
    ) -> Result<(), String> {
        let resolved = match self
            .commands
            .resolve(command, arguments)
            .map_err(to_string)?
        {
            zuno_catalog::command::Resolution::Ready(resolved) => resolved,
            zuno_catalog::command::Resolution::PendingMcp(_) => {
                return Err(format!(
                    "command `{command}` requires a connected MCP prompt provider"
                ));
            }
        };
        if resolved.subtask == Some(true) {
            return Err(format!(
                "command `{command}` requires subtask execution, which this surface cannot host"
            ));
        }
        let guard = self
            .runs
            .begin_turn(self.session_id.clone())
            .map_err(to_string)?;
        self.drive_input(&resolved.prompt, None, None, &guard, events)
            .await
    }

    pub(crate) async fn drive_with_message_id(
        &mut self,
        prompt: &str,
        message_id: Option<&str>,
        events: TurnEventSender,
    ) -> Result<(), String> {
        let guard = self
            .runs
            .begin_turn(self.session_id.clone())
            .map_err(to_string)?;
        self.drive_input(prompt, message_id, None, &guard, events)
            .await
    }

    async fn recover_background_reports(&mut self) -> Result<(), String> {
        if self.background_reports_recovered {
            return Ok(());
        }
        self.product_agents
            .recover_uncertain(&self.session_id)
            .await?;
        self.background_reports
            .recover_pending_reports(&self.session_id)
            .await?;
        self.background_reports_recovered = true;
        Ok(())
    }

    pub(crate) async fn drive_with_message_id_and_guard(
        &mut self,
        prompt: &str,
        message_id: Option<&str>,
        guard: SessionRunGuard,
        events: TurnEventSender,
    ) -> Result<(), String> {
        self.drive_input(prompt, message_id, None, &guard, events)
            .await
    }

    async fn drive_input(
        &mut self,
        prompt: &str,
        message_id: Option<&str>,
        content: Option<&[RequestContentBlock]>,
        guard: &SessionRunGuard,
        events: TurnEventSender,
    ) -> Result<(), String> {
        self.require_active_extension_composition()?;
        let latest = zuno_db::message::MessageStore::new(&self.connection)
            .latest_time_created(&self.session_id)
            .map_err(to_string)?;
        let (message, parts) = prepare_user_message(
            UserMessageInput {
                session_id: &self.session_id,
                agent: &self.agent,
                provider_id: &self.provider_id,
                model_id: &self.model_id,
                text: prompt,
                message_id,
                now: zuno_db::message::created_after(zuno_db::message::now_millis(), latest),
            },
            content,
        )?;
        let materialized = self.persist_user_input(&message, &parts)?;
        if materialized {
            events
                .publish(TurnEvent::SessionMaterialized {
                    session_id: self.session_id.clone(),
                    title: self.session_title.clone(),
                })
                .await
                .map_err(to_string)?;
        }
        self.recover_background_reports().await?;
        self.goal_projection
            .ingest(&self.goal_store)
            .map_err(to_string)?;
        let usage_before = goal_usage(&self.connection, &self.session_id)?;
        let started = Instant::now();
        let result = self.drive_input_unaccounted(guard, events.clone()).await;
        match result {
            Ok(outcome) => {
                self.last_turn_completed = outcome
                    .as_ref()
                    .is_some_and(|outcome| matches!(outcome, TurnOutcome::Completed { .. }));
                self.finish_goal_turn(usage_before, started, outcome.as_ref())?;
                self.schedule_reflection(outcome.as_ref(), &events).await;
                Ok(())
            }
            Err(error) => self
                .handle_turn_failure(usage_before, started, error, &events)
                .await
                .map(|_| ()),
        }
    }

    async fn drive_input_unaccounted(
        &mut self,
        guard: &SessionRunGuard,
        events: TurnEventSender,
    ) -> Result<Option<TurnOutcome>, TurnFailure> {
        let outcome = self.run_prelude().await?;
        report_prelude(&events, &self.notes, &outcome)
            .await
            .map_err(TurnFailure::event_consumer)?;
        if !outcome.continue_turn {
            return Ok(None);
        }
        let dynamic_context = self.goal_dynamic_context().map_err(TurnFailure::host)?;
        self.execute_turn_unaccounted(dynamic_context, guard, events)
            .await
    }

    fn persist_user_input(
        &mut self,
        message: &zuno_db::message::MessageRecord,
        parts: &[zuno_db::message::PartRecord],
    ) -> Result<bool, String> {
        let durable_input = zuno_db::inbox::NewSessionInput::new(
            format!("inp_{}", message.id),
            self.session_id.clone(),
            json!({
                "message": message.to_json(),
                "parts": parts.iter().map(zuno_db::message::PartRecord::to_json).collect::<Vec<_>>(),
            }),
            zuno_db::inbox::InputDelivery::NextStep,
            message.time_created,
        );
        match &self.session_materializer {
            SessionMaterializer::Existing => {
                let transaction = self.connection.unchecked_transaction().map_err(to_string)?;
                zuno_db::inbox::admit_and_promote_in(&transaction, durable_input)
                    .map_err(to_string)?;
                persist_prepared_user_message(&transaction, message, parts).map_err(to_string)?;
                transaction.commit().map_err(to_string)?;
                Ok(false)
            }
            SessionMaterializer::Pending(input) => {
                let mut input = input.clone();
                input.time = Some(message.time_created);
                let transaction = self.connection.transaction().map_err(to_string)?;
                zuno_db::session::create(&transaction, &input).map_err(to_string)?;
                zuno_db::inbox::admit_and_promote_in(&transaction, durable_input)
                    .map_err(to_string)?;
                persist_prepared_user_message(&transaction, message, parts).map_err(to_string)?;
                transaction.commit().map_err(to_string)?;
                self.session_materializer = SessionMaterializer::Existing;
                self.session_identity.mark_materialized();
                Ok(true)
            }
        }
    }

    pub(crate) async fn continue_goal_if_idle(
        &mut self,
        queued_input: QueuedUserInput,
        events: TurnEventSender,
    ) -> Result<bool, String> {
        self.require_active_extension_composition()?;
        if !self.last_turn_completed {
            return Ok(false);
        }
        self.goal_projection
            .ingest(&self.goal_store)
            .map_err(to_string)?;
        let mode = if self.agent == "plan" {
            GoalTurnMode::Plan
        } else {
            GoalTurnMode::Work
        };
        let queued_input = if queued_input == QueuedUserInput::Present
            || !self
                .inbox
                .pending(&self.session_id)
                .map_err(to_string)?
                .is_empty()
        {
            QueuedUserInput::Present
        } else {
            QueuedUserInput::Absent
        };
        let prepared = match self
            .goal_continuation
            .prepare_if_idle(&self.session_id, mode, queued_input)
            .map_err(to_string)?
        {
            ContinuationAttempt::Prepared(prepared) => prepared,
            ContinuationAttempt::Suppressed(ContinuationSuppression::RetryBackoff {
                remaining,
            }) => {
                tokio::time::sleep(
                    remaining.min(self.goal_continuation.retry_policy().poll_interval()),
                )
                .await;
                return Ok(true);
            }
            ContinuationAttempt::Suppressed(_) => return Ok(false),
        };
        if !self
            .goal_continuation
            .is_current(&prepared)
            .map_err(to_string)?
        {
            return Ok(true);
        }
        let usage_before = goal_usage(&self.connection, &self.session_id)?;
        let started = Instant::now();
        let retry_reason = self
            .goal_store
            .retry_state(&self.session_id)
            .map_err(to_string)?
            .map(|retry| retry.reason);
        let result = async {
            if retry_reason == Some(GoalRetryReason::ContextLimit) {
                self.recover_goal_context().await?;
                self.goal_store
                    .mark_retry_context_compacted(&self.session_id)
                    .map_err(TurnFailure::host)?;
            }
            self.continue_goal_unaccounted(&prepared, events.clone())
                .await
        }
        .await;
        match result {
            Ok(outcome) => {
                self.last_turn_completed = outcome
                    .as_ref()
                    .is_some_and(|outcome| matches!(outcome, TurnOutcome::Completed { .. }));
                self.finish_goal_turn(usage_before, started, outcome.as_ref())?;
                self.schedule_reflection(outcome.as_ref(), &events).await;
                Ok(self.last_turn_completed)
            }
            Err(error) => {
                self.handle_turn_failure(usage_before, started, error, &events)
                    .await
            }
        }
    }

    async fn continue_goal_unaccounted(
        &mut self,
        prepared: &zuno_goal::PreparedContinuation,
        events: TurnEventSender,
    ) -> Result<Option<TurnOutcome>, TurnFailure> {
        let dynamic_context = dynamic_context_from_goal_entry(prepared.entry());
        let prelude = self.run_prelude().await;
        let prelude = match prelude {
            Ok(prelude) if prelude.continue_turn => prelude,
            Ok(_) => return Ok(None),
            Err(error) => return Err(error),
        };
        report_prelude(&events, &self.notes, &prelude)
            .await
            .map_err(TurnFailure::event_consumer)?;
        self.execute_turn_unaccounted(dynamic_context, prepared.run_guard(), events)
            .await
    }

    async fn execute_turn_unaccounted(
        &mut self,
        dynamic_context: DynamicContext,
        guard: &SessionRunGuard,
        events: TurnEventSender,
    ) -> Result<Option<TurnOutcome>, TurnFailure> {
        let context = TurnContext::new(
            &mut self.connection,
            &self.providers,
            &self.resolver,
            &self.dispatcher,
            guard.interrupt_signal(),
        )
        .with_live_inputs(guard, &self.inbox)
        .with_tool_concurrency(self.tool_concurrency);
        let outcome = self
            .driver
            .drive(
                RunTurnRequest::new(
                    self.session_id.clone(),
                    Uuid::new_v4().simple().to_string(),
                    dynamic_context,
                )
                .with_context_limit(self.window.context),
                context,
                events.clone(),
            )
            .await;
        outcome.map(Some).map_err(TurnFailure::Engine)
    }

    async fn recover_goal_context(&mut self) -> Result<(), TurnFailure> {
        self.compaction_state.reset_retryable_failure();
        let providers = RegistryProviders(&self.providers);
        let noop_hooks = zuno_engine::compaction::NoopCompactionHooks;
        let mut context = PreludeContext {
            connection: &mut self.connection,
            providers: &providers,
            internals: &self.internals,
            compaction: &self.compaction_config,
            window: self.window,
            state: &mut self.compaction_state,
            hooks: &noop_hooks,
        };
        compact_requested(&self.session_id, &mut context, true)
            .await
            .map(|_| ())
            .map_err(|error| match error {
                CompactionSkipped::Database(error) => {
                    TurnFailure::Engine(TurnError::Database(error))
                }
                CompactionSkipped::Reason(message) => TurnFailure::host(format!(
                    "goal context compaction could not start: {message}"
                )),
                CompactionSkipped::Stopped {
                    reason,
                    message,
                    recovery,
                } => {
                    let failure = match recovery {
                        Recovery::Retry { after } => GoalTerminalFailure::Retry {
                            reason: GoalRetryReason::ContextLimit,
                            retry_after: after,
                        },
                        Recovery::Reauthenticate => GoalTerminalFailure::Pause,
                        Recovery::Compact | Recovery::Fail => GoalTerminalFailure::Block,
                    };
                    TurnFailure::goal_recovery(
                        format!("goal context compaction stopped ({reason:?}): {message}"),
                        failure,
                    )
                }
            })
    }

    fn goal_dynamic_context(&self) -> Result<DynamicContext, String> {
        self.goal_continuation
            .injection(&self.session_id)
            .map_err(to_string)
            .map(|entry| {
                entry.map_or_else(DynamicContext::default, |entry| {
                    dynamic_context_from_goal_entry(&entry)
                })
            })
    }

    fn finish_goal_turn(
        &mut self,
        usage_before: GoalUsage,
        started: Instant,
        outcome: Option<&TurnOutcome>,
    ) -> Result<(), String> {
        self.record_goal_usage(usage_before, started)?;
        match outcome {
            Some(TurnOutcome::Completed {
                unresolved_tool_failures,
                ..
            }) => {
                if let Some(failure) = goal_tool_failure(unresolved_tool_failures) {
                    self.goal_continuation
                        .record_terminal_failure(&self.session_id, failure)
                        .map_err(to_string)?;
                } else {
                    self.goal_continuation
                        .record_turn_outcome(&self.session_id, GoalTurnOutcome::Progress)
                        .map_err(to_string)?;
                }
                self.compaction_state.reset_after_turn_success();
            }
            Some(TurnOutcome::Interrupted { .. }) => {
                self.goal_continuation
                    .record_terminal_failure(&self.session_id, GoalTerminalFailure::Pause)
                    .map_err(to_string)?;
            }
            None => {}
        }
        self.write_goal_projection()?;
        self.work_changes.changed();
        Ok(())
    }

    async fn schedule_reflection(
        &mut self,
        outcome: Option<&TurnOutcome>,
        events: &TurnEventSender,
    ) {
        let (
            Some(reflection),
            Some(TurnOutcome::Completed {
                assistant_message_id,
                steps,
                ..
            }),
        ) = (&self.reflection, outcome)
        else {
            return;
        };
        let transcript = match durable_reflection_transcript(
            &self.connection,
            &self.session_id,
            assistant_message_id,
        ) {
            Ok(transcript) => transcript,
            Err(error) => {
                let _ = events
                    .publish(TurnEvent::Provider {
                        step: *steps,
                        event: StreamEvent::StatusDetail {
                            detail: format!(
                                "warning: memory reflection skipped because the delivered turn could not be replayed: {error}"
                            ),
                        },
                    })
                    .await;
                return;
            }
        };
        let context = ToolContext::new(
            self.session_id.clone(),
            assistant_message_id.clone(),
            format!("reflection_{}", Uuid::new_v4().simple()),
            self.agent.clone(),
            Arc::new(AllowAll),
            Arc::new(NeverInterrupted),
        );
        let task = reflection.spawn_after_turn(ReflectionTurn::new(
            TurnDelivery::new(true, false),
            transcript,
            context,
        ));
        if let Some(task) = task {
            self.background_jobs.supervise_handle(
                format!("reflection_{assistant_message_id}"),
                self.session_id.clone(),
                tokio_util::sync::CancellationToken::new(),
                task,
            );
        }
    }

    fn finish_goal_error(
        &self,
        usage_before: GoalUsage,
        started: Instant,
        failure: GoalTerminalFailure,
    ) -> Result<GoalFailureDisposition, String> {
        self.record_goal_usage(usage_before, started)?;
        let disposition = self
            .goal_continuation
            .record_terminal_failure(&self.session_id, failure)
            .map_err(to_string)?;
        self.write_goal_projection()?;
        self.work_changes.changed();
        Ok(disposition)
    }

    async fn handle_turn_failure(
        &mut self,
        usage_before: GoalUsage,
        started: Instant,
        failure: TurnFailure,
        events: &TurnEventSender,
    ) -> Result<bool, String> {
        let rendered = failure.rendered(self.credential.as_deref());
        let disposition = self
            .finish_goal_error(usage_before, started, failure.goal_failure())
            .map_err(|goal_error| {
                format!(
                    "{rendered}; additionally failed to record terminal goal state: {goal_error}"
                )
            })?;
        match disposition {
            GoalFailureDisposition::RetryScheduled(retry) => {
                self.last_turn_completed = true;
                report_goal_retry(events, &retry, &rendered).await?;
                Ok(true)
            }
            GoalFailureDisposition::Paused(_)
            | GoalFailureDisposition::Blocked(_)
            | GoalFailureDisposition::NoActiveGoal => {
                self.last_turn_completed = false;
                Err(rendered)
            }
        }
    }

    fn record_goal_usage(&self, before: GoalUsage, started: Instant) -> Result<(), String> {
        let after = goal_usage(&self.connection, &self.session_id)?;
        let token_delta = after.tokens.saturating_sub(before.tokens);
        let elapsed = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
        self.goal_store
            .record_usage(&self.session_id, token_delta, elapsed)
            .map_err(to_string)?;
        Ok(())
    }

    fn write_goal_projection(&self) -> Result<(), String> {
        if let Some(goal) = self.goal_store.goal(&self.session_id).map_err(to_string)? {
            self.goal_projection.write(&goal).map_err(to_string)?;
        }
        Ok(())
    }

    pub(crate) async fn compact(&mut self, automatic: bool) -> Result<(), String> {
        self.require_active_extension_composition()?;
        self.goal_projection
            .ingest(&self.goal_store)
            .map_err(to_string)?;
        let usage_before = goal_usage(&self.connection, &self.session_id)?;
        let started = Instant::now();
        let result = self.compact_unaccounted(automatic).await;
        match result {
            Ok(()) => {
                self.last_turn_completed = true;
                self.finish_goal_turn(usage_before, started, None)
            }
            Err(error) => {
                self.last_turn_completed = false;
                match self.finish_goal_error(usage_before, started, GoalTerminalFailure::Block) {
                    Ok(_) => Err(error),
                    Err(goal_error) => Err(format!(
                        "{error}; additionally failed to record terminal goal state: {goal_error}"
                    )),
                }
            }
        }
    }

    async fn compact_unaccounted(&mut self, automatic: bool) -> Result<(), String> {
        let providers = RegistryProviders(&self.providers);
        let noop_hooks = zuno_engine::compaction::NoopCompactionHooks;
        let mut context = PreludeContext {
            connection: &mut self.connection,
            providers: &providers,
            internals: &self.internals,
            compaction: &self.compaction_config,
            window: self.window,
            state: &mut self.compaction_state,
            hooks: &noop_hooks,
        };
        compact_requested(&self.session_id, &mut context, automatic)
            .await
            .map(|_| ())
            .map_err(|error| format!("manual compaction failed: {error:?}"))
    }

    /// Run every internal that applies before this turn.
    ///
    /// The `summary` internal is resolved by the same [`resolve_internals`] pass and
    /// reached through the same [`PreludeContext`] as the other two, but nothing here
    /// requests one: no surface in this workspace displays a session summary yet, and
    /// inventing a command to prove the wiring would ship a subcommand upstream does
    /// not have. What matters is that when a surface does want one it calls
    /// [`zuno_engine::prelude::summarize`] with this context rather than resolving a
    /// second model of its own — resolving separately is exactly how all three
    /// internals came to be declared and never invoked.
    async fn run_prelude(&mut self) -> Result<PreludeOutcome, TurnFailure> {
        let providers = RegistryProviders(&self.providers);
        let noop_hooks = zuno_engine::compaction::NoopCompactionHooks;
        let mut context = PreludeContext {
            connection: &mut self.connection,
            providers: &providers,
            internals: &self.internals,
            compaction: &self.compaction_config,
            window: self.window,
            state: &mut self.compaction_state,
            hooks: &noop_hooks,
        };
        let outcome = run_prelude(&self.session_id, &mut context)
            .await
            .map_err(TurnError::Database)
            .map_err(TurnFailure::Engine)?;
        // Here rather than in either caller, because both `drive_input` and
        // `continue_goal_unaccounted` reach the prelude through this method — and a title
        // published from only one of them appears or not depending on whether the turn was
        // typed or continued, which is not a distinction the panel should show.
        //
        // At most one publish per session, and that is the generator's guarantee rather
        // than a count kept here: `generate_title` answers `None` unless
        // `zuno_db::session::is_default_title` still holds, and the write that answers it
        // clears the predicate before any later turn asks. So this runs on the turn that
        // named the session and on no other.
        if let Some(title) = &outcome.title {
            self.session_title = title.clone();
            if let Some(sink) = &self.title_sink {
                sink.publish(title);
            }
        }
        Ok(outcome)
    }
}

fn goal_tool_failure(recoveries: &[ToolFailureRecovery]) -> Option<GoalTerminalFailure> {
    if recoveries.is_empty() {
        return None;
    }
    let reason = if recoveries
        .iter()
        .any(|recovery| recovery.replay_policy == ToolReplayPolicy::Never)
    {
        GoalRetryReason::ToolUncertain
    } else {
        GoalRetryReason::ToolTransient
    };
    let retry_after = recoveries
        .iter()
        .filter_map(|recovery| recovery.retry_after)
        .max();
    Some(GoalTerminalFailure::Retry {
        reason,
        retry_after,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct GoalUsage {
    tokens: i64,
}

fn goal_usage(connection: &rusqlite::Connection, session_id: &str) -> Result<GoalUsage, String> {
    connection
        .query_row(
            "SELECT COALESCE(SUM(\
               COALESCE(json_extract(data, '$.tokens.input'), 0) + \
               COALESCE(json_extract(data, '$.tokens.output'), 0) + \
               COALESCE(json_extract(data, '$.tokens.reasoning'), 0) + \
               COALESCE(json_extract(data, '$.tokens.cache.read'), 0) + \
               COALESCE(json_extract(data, '$.tokens.cache.write'), 0)\
             ), 0) \
             FROM message \
             WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant'",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|tokens| GoalUsage { tokens })
        .map_err(to_string)
}

fn dynamic_context_from_goal_entry(
    entry: &zuno_engine::compaction::TranscriptEntry,
) -> DynamicContext {
    let text = entry
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    DynamicContext::new(text)
}

/// How many bytes of skill catalogue may enter the system prompt.
///
/// Sized against the real corpus rather than picked round: this machine's 189 skills
/// render to roughly 113 KB verbose, and 32 KB fits the great majority of them while
/// keeping the catalogue a fraction of a small model's context. The trim is reported,
/// never silent — see [`announce_skills`].
const SKILL_PROMPT_BUDGET: usize = 32 * 1024;

/// System-level trigger rules for the skill catalogue below.
const SKILL_USAGE_POLICY: &str = "\
Skills are mandatory trigger rules, not optional suggestions. Before taking any task action, \
compare the request with the advertised skill descriptions. If the user names a skill or the \
task clearly matches one, call the `skill` tool first, read the complete skill body, and follow \
it for that turn. Load only the minimal matching set, in the order needed. Do not claim a skill \
was used unless its tool call completed successfully, and do not substitute the catalogue \
description for the skill body.";

/// Put the discovered skills in the system prompt, and say so if any did not fit.
///
/// Discovery has run since todo 14, and until now its only consumer was a TUI status
/// line: the model was never told a single skill existed, so no skill could ever
/// activate and the `skill` tool had no names to be called with. This is the other
/// half of that tool — a catalogue without a loader is unusable, and a loader without
/// a catalogue is uncallable.
///
/// [`zuno_catalog::skill::Form::Verbose`] rather than `List` because it carries each
/// skill's `location`, which leaves `read` as a working fallback if the `skill` tool
/// is denied by an agent's permissions.
fn announce_skills(
    resolver: &mut Resolver,
    skills: &zuno_catalog::skill::Skills,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    if skills.all().is_empty() {
        return Ok(());
    }
    let rendered = skills.render_within(zuno_catalog::skill::Form::Verbose, SKILL_PROMPT_BUDGET);
    if rendered.rendered == 0 {
        notes.push(format!(
            "warning: no skill fits the {SKILL_PROMPT_BUDGET}-byte prompt budget, so none \
             were advertised; shorten the longest `description` in your skill frontmatter"
        ));
        return Ok(());
    }
    if rendered.omitted > 0 {
        notes.push(format!(
            "warning: {} of {} skills did not fit the {SKILL_PROMPT_BUDGET}-byte prompt \
             budget and were not advertised to the model",
            rendered.omitted,
            rendered.rendered + rendered.omitted,
        ));
    }
    resolver.append_prompt_section(
        "skills.policy",
        "zuno skill trigger policy",
        SKILL_USAGE_POLICY,
    )?;
    resolver.append_prompt_section("skills.catalog", "discovered skills", rendered.text)
}

/// How many bytes of instruction files may enter the system prompt.
///
/// Twice [`SKILL_PROMPT_BUDGET`], because these are the rules the user wrote for this
/// repository rather than a catalogue of capabilities they may never invoke, and
/// because the realistic corpus is larger than it looks: the `AGENTS.md` files on one
/// developer machine here run from 4 KB to 80 KB, and the global-plus-project pair is
/// typically under 15 KB. 64 KB admits that whole range and still bounds one
/// pathological file from consuming a small model's context.
const INSTRUCTION_PROMPT_BUDGET: usize = 64 * 1024;

/// Put the `AGENTS.md`-class rules in the system prompt, and say what did not fit.
///
/// # Placement: after memory, before the skill catalogue
///
/// The oracle builds `[...environment, ...instructions, ...mcp, ...skills]` under the
/// agent prompt (`session/prompt.ts:1257-1269` at 1.18.13, with the agent prompt
/// prepended in `session/llm/request.ts:56-66`). Instructions therefore sit **before**
/// skills, and this call is placed to match: getting that backwards would silently
/// change which text wins when a rule file and a skill description disagree.
///
/// Resident memory takes the oracle's `environment` slot — it is the workspace-facts
/// segment, machine-maintained and frozen at session start — so the assembled order
/// here is agent prompt, memory, instructions, skills, which maps one-to-one onto the
/// oracle's.
///
/// # Whole files are admitted or dropped, never cut
///
/// [`announce_skills`] may drop individual skills because each is independent and an
/// unmentioned skill merely goes unused. A rule file cut mid-sentence is worse than
/// an absent one: "do X unless Y" truncated after "do X" inverts the rule the user
/// wrote, while they go on believing it is in force. So the budget admits complete
/// blocks in discovery order and names, by path and size, every file it had to leave
/// out. Admitted contents are persisted in `session.prompt.assembled`, because
/// model-visible input must remain reconstructable. They are not printed to the
/// operational log.
///
/// Warnings from the load ([`zuno_config::WarningKind`]) are surfaced the same way. A
/// *missing* instruction file never reaches here at all, because discovery only
/// records paths that exist — which is what keeps the common case, a project with no
/// `AGENTS.md`, completely silent.
fn announce_instructions(
    resolver: &mut Resolver,
    loaded: &zuno_config::LoadedInstructions,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    for warning in loaded.warnings() {
        notes.push(format!("warning: {warning}"));
    }

    let mut admitted_bytes = 0usize;
    for (index, entry) in loaded.entries().iter().enumerate() {
        let block = entry.render();
        let projected = if admitted_bytes == 0 {
            block.len()
        } else {
            admitted_bytes + 2 + block.len()
        };
        if projected > INSTRUCTION_PROMPT_BUDGET {
            notes.push(format!(
                "warning: instruction file {} ({} bytes) did not fit the \
                 {INSTRUCTION_PROMPT_BUDGET}-byte prompt budget, so none of its rules are in \
                 force; shorten it or remove it from `instructions`",
                entry.source(),
                block.len(),
            ));
            continue;
        }
        admitted_bytes = projected;
        resolver.append_prompt_section(format!("instructions.{index}"), entry.source(), block)?;
    }

    Ok(())
}

fn configure_resident_memory(
    resolver: &mut Resolver,
    config: &zuno_config::schema::Config,
    paths: ScopePaths,
) -> Result<(), String> {
    let memory = config.resolved_memory();
    let limits = ScopeLimits::new(memory.global_char_limit, memory.project_char_limit);
    let session = SessionMemory::open_configured(
        memory.resident,
        paths.for_scope(zuno_memory::Scope::Global),
        paths.for_scope(zuno_memory::Scope::Project),
        limits,
    )
    .map_err(to_string)?;
    let Some(session) = session else {
        return Ok(());
    };
    for scope in Scope::ALL {
        resolver.append_prompt_section(
            match scope {
                Scope::Global => "memory.global",
                Scope::Project => "memory.project",
            },
            session.store(scope).path().display().to_string(),
            session.frozen_block(scope),
        )?;
    }
    Ok(())
}

/// Render a failed turn: its category, then every cause it wraps, then what to do.
///
/// # Why the cause chain is walked here and not per variant
///
/// A [`TurnError`] classifies; the detail hangs off it as a `#[source]`. Rendering
/// `error.to_string()` prints the classification and discards the detail, so a wrong
/// hostname, a dead port, a TLS refusal and an unexpanded `${VAR}` all read
/// `transient provider failure (status=None)` — seven words naming nothing the user
/// can act on, with the URL sitting one `source()` call away the whole time.
///
/// That defect was fixed twice by hand before this: once for a missing endpoint and
/// once for a rejected credential, each by composing a better message for that one
/// variant. Both fixes were correct and neither generalised, so the third instance
/// reached a user anyway. [`zuno_error::source::describe`] walks the chain once for
/// every variant, so there is no fourth instance to find.
///
/// # Naming where a rejected credential is configured
///
/// `authentication rejected by provider test` is accurate and useless: it names the
/// provider and not one place a user can act. Both places are named because both are
/// legitimate — [`provider_api_key`] reads the first and [`zuno_auth::AuthStore`] the
/// second. The advice is separated by `;` rather than `:` because everything before
/// it is now a chain of causes, and a colon would present guidance as one more cause.
///
/// A keyless provider is deliberately **not** refused earlier:
/// [`zuno_provider_compatible::CompatibleProvider::new`] documents that a local endpoint
/// legitimately has no credential, so the gateway's rejection is the first moment a
/// missing key is known to be a problem.
///
/// # The chain is scrubbed before anyone reads it
///
/// Walking causes means rendering whatever a peer put in a 401 body, and a gateway
/// that echoes the key it rejected is a real shape — a vendor answering `Incorrect
/// API key provided: sk-…` turns a diagnostic into a disclosure. The message the
/// credential travelled in is therefore filtered through [`without_credential`]
/// before it is returned, so the guarantee that no key material reaches a terminal
/// survives the walk that made the rest of the failure legible.
fn describe_turn_failure(error: &TurnError, credential: Option<&str>) -> String {
    let mut message = zuno_error::source::describe(error);
    if let TurnError::Provider(ProviderError::Auth { provider, .. }) = error {
        message.push_str(&format!(
            "; set `provider.{provider}.options.apiKey`, or run \
             `zuno auth login {provider}`"
        ));
    }
    without_credential(message, credential)
}

/// What stands in for a credential that would otherwise have been printed.
const REDACTED: &str = "<redacted>";

/// Remove every occurrence of `credential` from `message`.
///
/// Exact removal rather than a search for credential-shaped text: this is the one
/// secret the turn actually presented, so matching it is complete for the value that
/// matters and cannot be defeated by a vendor formatting it unexpectedly. A pattern
/// guess would be both weaker here and prone to redacting a user's prose.
///
/// A short credential over-redacts — a one-character key blanks every occurrence of
/// that character. That is the deliberate direction to fail in: a mangled message is
/// a cosmetic loss and a leaked key is not, and no real credential is short enough
/// for it to happen. An **empty** credential is skipped, because `str::replace` with
/// an empty pattern inserts the replacement between every character;
/// [`provider_api_key`] documents why an empty key is a legitimate configuration and
/// therefore reaches this function.
fn without_credential(message: String, credential: Option<&str>) -> String {
    match credential {
        Some(secret) if !secret.is_empty() => message.replace(secret, REDACTED),
        Some(_) | None => message,
    }
}

/// Put every prelude decision on the turn's own event channel.
///
/// The channel and not stderr, because the interactive surface owns the terminal and a
/// line written past it either vanishes or corrupts the frame — the same argument
/// `tui.rs` makes for reporting turn failures this way. Reporting at all is the point:
/// a session that could not be named and a history that could not be compacted are
/// both losses the user is entitled to see, and "no output" is how the three internals
/// stayed missing through 3,057 passing tests.
async fn report_prelude(
    events: &TurnEventSender,
    notes: &[String],
    outcome: &PreludeOutcome,
) -> Result<(), String> {
    let mut details: Vec<String> = notes.to_vec();
    if let Some(title) = &outcome.title {
        details.push(format!("session titled: {title}"));
    }
    if outcome.compacted {
        details.push("history compacted before this turn".to_owned());
    }
    details.extend(outcome.skipped.iter().cloned());
    for detail in details {
        events
            .publish(TurnEvent::Provider {
                step: 0,
                event: StreamEvent::StatusDetail { detail },
            })
            .await
            .map_err(to_string)?;
    }
    Ok(())
}

async fn report_goal_retry(
    events: &TurnEventSender,
    retry: &GoalRetryState,
    failure: &str,
) -> Result<(), String> {
    let delay = Duration::from_millis(u64::try_from(retry.delay_ms).unwrap_or_default());
    events
        .publish(TurnEvent::Provider {
            step: 0,
            event: StreamEvent::Error {
                message: format!(
                    "{failure}; active goal retry {} is scheduled in {delay:?} ({})",
                    retry.attempt,
                    retry.reason.as_str()
                ),
                retry_after: Some(delay),
            },
        })
        .await
        .map_err(to_string)
}

/// Pick the model this turn runs on.
///
/// Takes the [`CatalogProvenance`] rather than just the catalog because an absent
/// model means two different things. The lookup happens against the **resolved**
/// catalog, config already merged in, so a config that fully specifies its provider
/// and model succeeds here with no catalog at all — that is todo 108's fix, and it is
/// why the provenance is consulted only after the lookup has already failed.
fn select_model<'a>(
    catalog: &'a Catalog,
    requested: Option<&str>,
    provenance: &CatalogProvenance,
) -> Result<(String, String, &'a zuno_llm::catalog::ResolvedModel), String> {
    if let Some(requested) = requested {
        let (provider_id, model_id) = requested
            .split_once('/')
            .ok_or_else(|| format!("model must be provider/model, got {requested:?}"))?;
        let model = catalog.model(provider_id, model_id).ok_or_else(|| {
            provenance
                .unresolved_model(requested)
                .map_or_else(|| format!("Model not found: {requested}"), to_string)
        })?;
        return Ok((provider_id.to_owned(), model_id.to_owned(), model));
    }

    for provider_id in catalog.provider_ids() {
        let provider = catalog
            .provider(provider_id)
            .ok_or_else(|| format!("catalog listed provider {provider_id} but has no entry"))?;
        let mut model_ids: Vec<&str> = provider.models.keys().map(String::as_str).collect();
        model_ids.sort_by(|left, right| zuno_llm::catalog::collate::compare(left, right));
        if let Some(model_id) = model_ids.into_iter().next() {
            let model = provider
                .models
                .get(model_id)
                .ok_or_else(|| format!("catalog lost model {provider_id}/{model_id}"))?;
            return Ok((provider_id.to_owned(), model_id.to_owned(), model));
        }
    }
    let mut message =
        "no available model; configure a provider credential or provider block".to_owned();
    if let CatalogProvenance::FetchForbidden { origin, cache } = provenance {
        // Nothing was requested, so there is no model to name — but "no available
        // model" alone would let a forbidden fetch read as "you configured nothing".
        message.push_str(&format!(
            ", or allow the catalog to load: ZUNO_DISABLE_MODELS_FETCH is set, so \
             `{origin}` was not contacted and no cached catalog exists at `{}`",
            cache.display()
        ));
    }
    Err(message)
}

struct DelegationAgents {
    targets: zuno_tools::task::DelegationTargets,
    models: Vec<(String, ModelChoice)>,
}

/// Resolve the exact child-agent roster from the same catalog the child host uses.
///
/// Native agents keep `zuno-agent`'s role and capability gates. Configured,
/// Markdown, and extension agents become targets when their public mode includes
/// subagent use. This closes the gap where a custom agent could be selected directly
/// but `task` rejected its name.
fn delegation_agents(
    agents: &[zuno_catalog::agent::Agent],
    vision_available: bool,
) -> Result<DelegationAgents, String> {
    let native_targets = zuno_tools::task::valid_targets(vision_available);
    let names = agents
        .iter()
        .filter(|agent| {
            matches!(
                agent.mode,
                zuno_catalog::agent::AgentMode::Subagent | zuno_catalog::agent::AgentMode::All
            )
        })
        .filter(|agent| {
            !agent.source.is_native()
                || native_targets
                    .iter()
                    .any(|candidate| candidate == &agent.name)
        })
        .map(|agent| agent.name.clone())
        .collect::<Vec<_>>();
    let target_names = names.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let models = agents
        .iter()
        .filter(|agent| target_names.contains(agent.name.as_str()))
        .filter_map(|agent| {
            let model = agent.model.as_ref()?;
            let mut choice = ModelChoice::new(model.clone());
            choice.variant = agent.variant.clone();
            Some((agent.name.clone(), choice))
        })
        .collect();
    let targets = zuno_tools::task::DelegationTargets::new(names).map_err(to_string)?;
    Ok(DelegationAgents { targets, models })
}

/// Catalog facts for every reachable model, for [`zuno_tools::task::ProviderFacts`].
///
/// Built here because this is where the catalog is already resolved, and keyed on the
/// same `provider/model` string [`zuno_agent::model_policy::ModelChoice`] carries, so
/// a delegation naming a model and this map agree by construction.
///
/// Three of the four facts are read from the catalog: `reasoning` from the model's
/// declared capability, `variants` from its declared variants, and `family` from the
/// transport [`provider_factory_key`] already resolves, so a transport added there
/// cannot silently fall through to the wrong request shape here.
///
/// `effort` is [`EffortCapabilities::default`], and that is a real limitation rather
/// than a chosen value: nothing in the resolved catalog carries a model's adaptive or
/// token-budget reasoning shape, so there is nothing to read. The consequence is
/// bounded — it applies only when a delegation passes an explicit `effort` that the
/// model does not itself declare a variant for, and it yields the named-effort shape
/// rather than a budget one. Everything else about the delegation is unaffected.
fn delegation_facts(catalog: &Catalog) -> zuno_tools::task::FixedFacts {
    let mut facts = zuno_tools::task::FixedFacts::new();
    // Walked through `model_lines`, the same enumeration `zuno models` prints and
    // `picker_model_ids` fills the model picker from, so "a model a delegation may
    // name" and "a model this build offers" are one list.
    for line in catalog.model_lines() {
        let Some((provider_id, model_id)) = line.split_once('/') else {
            continue;
        };
        let Some(model) = catalog.model(provider_id, model_id) else {
            continue;
        };
        let Some(family) = effort_family(model.api.transport) else {
            continue;
        };
        facts = facts.with(
            line.clone(),
            zuno_tools::task::ModelFacts {
                family,
                reasoning: !selectable_reasoning_efforts(model).is_empty(),
                effort: zuno_llm::effort::EffortCapabilities::default(),
                variants: model.variants.clone(),
            },
        );
    }
    facts
}

/// Which request-shape family a transport's reasoning options belong to.
///
/// Keyed off the resolved native transport used by provider construction.
fn effort_family(transport: Option<ProviderTransport>) -> Option<zuno_llm::effort::ProviderFamily> {
    use zuno_llm::effort::ProviderFamily;
    let family = match transport? {
        ProviderTransport::Anthropic | ProviderTransport::GoogleVertexAnthropic => {
            ProviderFamily::Anthropic
        }
        ProviderTransport::Bedrock | ProviderTransport::BedrockMantle => ProviderFamily::Bedrock,
        ProviderTransport::Google | ProviderTransport::GoogleVertex => ProviderFamily::Google,
        ProviderTransport::Openrouter => ProviderFamily::OpenRouter,
        ProviderTransport::Openai | ProviderTransport::OpenaiCompatible => ProviderFamily::OpenAi,
    };
    Some(family)
}

fn provider_factory_key(transport: Option<ProviderTransport>) -> Option<&'static str> {
    match transport? {
        ProviderTransport::Anthropic => Some("anthropic"),
        ProviderTransport::Bedrock => Some("amazon-bedrock"),
        ProviderTransport::BedrockMantle => Some("amazon-bedrock/mantle"),
        ProviderTransport::Google => Some("google"),
        ProviderTransport::GoogleVertex => Some("google-vertex"),
        ProviderTransport::GoogleVertexAnthropic => Some("google-vertex/anthropic"),
        ProviderTransport::Openai => Some("openai"),
        ProviderTransport::OpenaiCompatible | ProviderTransport::Openrouter => {
            Some(COMPATIBLE_PROVIDER)
        }
    }
}

fn provider_registry(
    provider_id: &str,
    credential: Option<Credential>,
    auth_store: Option<AuthStore>,
) -> ProviderRegistry {
    let mut providers = ProviderRegistry::new();

    let transport: Arc<dyn Transport> = Arc::new(ReqwestTransport::new(provider_id));
    let compatible_credential = credential.as_ref().map(credential_value);
    providers.register_fallible(
        COMPATIBLE_PROVIDER,
        zuno_provider_compatible::factory(transport, move |_| compatible_credential.clone()),
    );

    let anthropic_credential = credential.clone();
    providers.register_fallible(
        "anthropic",
        zuno_provider_anthropic::factory(move |_| anthropic_credential.clone()),
    );

    let openai_credential = credential.clone();
    providers.register_fallible(
        "openai",
        zuno_provider_openai::factory(move |_| openai_credential.clone(), auth_store),
    );

    providers.register_fallible("amazon-bedrock", |spec| {
        zuno_provider_bedrock::factory(spec)
            .map_err(|error| zuno_llm::registry::Declined::Failed(ProviderError::fatal(error)))
    });
    providers.register_fallible("amazon-bedrock/mantle", |spec| {
        zuno_provider_bedrock::factory(spec)
            .map_err(|error| zuno_llm::registry::Declined::Failed(ProviderError::fatal(error)))
    });

    let google_credential = credential.as_ref().map(credential_value);
    providers.register_fallible(
        "google",
        zuno_provider_google::google_factory(move |_| google_credential.clone()),
    );

    let vertex_credential = credential.as_ref().map(credential_value);
    providers.register_fallible(
        "google-vertex",
        zuno_provider_google::vertex_gemini_factory(move |_| vertex_credential.clone()),
    );

    let vertex_anthropic_credential = credential.as_ref().map(credential_value);
    providers.register_fallible(
        "google-vertex/anthropic",
        zuno_provider_google::vertex_anthropic_factory(move |_| {
            vertex_anthropic_credential.clone()
        }),
    );

    providers
}

/// The provider-option keys that name an endpoint, in precedence order.
///
/// `endpoint` first — `provider.ts:355-358` spells the fallback
/// `options?.endpoint ?? options?.baseURL`, so a provider carrying both is dialled at
/// `endpoint`. Both are also excluded from the SDK option bag by [`forwarded_options`]:
/// they are a URL, not a parameter, and they travel as [`Spec::base_url`].
const ENDPOINT_OPTIONS: [&str; 2] = ["endpoint", "baseURL"];

/// The option key carrying a configured API key.
///
/// Spelled as the oracle spells it (`provider.ts:1719`), so config *content*
/// authored for opencode keeps working once it is under Zuno's filename. Like the endpoint keys it is **excluded** from the
/// forwarded SDK bag — see [`resolved_elsewhere`] — because it is the credential, and
/// the credential travels one way only, through [`zuno_provider_compatible::factory`]'s
/// lookup.
const API_KEY_OPTION: &str = "apiKey";

/// Whether an option is resolved into a dedicated [`Spec`] field or into the credential,
/// and therefore must never also be forwarded in the SDK option bag.
///
/// Each such key has exactly one answerer: `endpoint`/`baseURL` answer
/// [`Spec::base_url`] via [`provider_endpoint`], and `apiKey` answers the credential via
/// [`provider_api_key`]. Forwarding them as well would be inert today — `Spec::options`
/// is read by allow-listed key — and inert-today is precisely how a request body grows a
/// field named after a URL, or worse a field carrying key material, the moment somebody
/// widens that read.
///
/// Derived from [`ENDPOINT_OPTIONS`] rather than restating its entries, so a third
/// endpoint spelling added there is excluded here with no second edit.
fn resolved_elsewhere(name: &str) -> bool {
    ENDPOINT_OPTIONS.contains(&name) || name == API_KEY_OPTION
}

/// The ceiling a build will accept even when a model's catalog entry claims more.
///
/// `ProviderTransform.OUTPUT_TOKEN_MAX` (`provider/transform.ts:18`).
const OUTPUT_TOKEN_MAX: u64 = 32_000;

/// The output-token ceiling for one model, as the oracle computes it.
///
/// `Math.min(model.limit.output, OUTPUT_TOKEN_MAX) || OUTPUT_TOKEN_MAX`
/// (`provider/transform.ts:1412-1414`). The `||` is load-bearing rather than
/// defensive: a catalog entry that declares no output limit deserialises to `0`
/// (`catalog/models_dev.rs:116-127` defaults the field), and sending `max_tokens: 0`
/// asks a model for an empty completion. JavaScript's falsy `0` turns that into the
/// ceiling; this reproduces the same choice explicitly.
///
/// # Why a ceiling exists at all
///
/// Without it a catalog entry advertising a million-token output would be forwarded
/// verbatim, and providers differ on whether that is rejected outright or silently
/// billed. Clamping keeps a bad catalog row from becoming a bad request.
fn output_ceiling(model: &zuno_llm::catalog::ResolvedModel) -> u64 {
    let declared = token_count(model.limit.output);
    match declared.min(OUTPUT_TOKEN_MAX) {
        0 => OUTPUT_TOKEN_MAX,
        ceiling => ceiling,
    }
}

/// The key a provider's own options declare, if any.
///
/// # Why this is primary and the stored credential is the fallback
///
/// `provider.ts:1719` is
/// `if (options["apiKey"] === undefined && provider.key) options["apiKey"] = provider.key`
/// — the config's key is consulted first and the credential fills in only when the
/// option is absent. Ours had it inverted: [`credential_value`] was the *only* source,
/// so a user who followed the upstream docs and put `baseURL` and `apiKey` together in
/// `provider.<id>.options` sent no `Authorization` header at all. Measured against a
/// real listener before the fix: `AUTH=None`, twice.
///
/// # An explicitly empty key is a key, not an absence
///
/// The oracle tests `apiKey` for `=== undefined`, not for `!== ""` as it tests
/// `baseURL` at `:1699`. The asymmetry is right on both sides: an empty `baseURL`
/// cannot be dialled, whereas an empty `apiKey` is a user saying "this endpoint takes
/// no key" — and falling back to a stored credential there would present a real vendor
/// key to a local endpoint the user never authorised. A non-string value falls through
/// instead, because it cannot be a bearer token.
fn provider_api_key(provider: Option<&zuno_llm::catalog::ResolvedProvider>) -> Option<String> {
    provider
        .and_then(|provider| provider.options.get(API_KEY_OPTION))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The credential one turn presents, config first.
///
/// See [`provider_api_key`] for the precedence and why it is that way round.
fn resolved_credential(
    provider: Option<&zuno_llm::catalog::ResolvedProvider>,
    stored: Option<&Credential>,
    env: &zuno_paths::Env,
) -> Option<Credential> {
    provider_api_key(provider)
        .map(|key| Credential::Api {
            key: zuno_auth::Secret::new(key),
            metadata: None,
        })
        .or_else(|| stored.cloned())
        .or_else(|| {
            provider.and_then(|provider| {
                provider.env.iter().find_map(|name| {
                    env.truthy_value(name).map(|key| Credential::Api {
                        key: zuno_auth::Secret::new(key),
                        metadata: None,
                    })
                })
            })
        })
}

/// Every `provider/model` a picker may offer.
///
/// A named function rather than an inline call so the choice of enumeration is one
/// testable decision. It is [`Catalog::model_lines`] — the same function `zuno models`
/// prints from (`models.rs`'s `provider_ids` + `print_models` walk resolves to the same
/// pairs in the same order). Reading the session provider's slice here instead is exactly
/// the defect that let `/model` show one provider while `zuno models` showed ten.
fn picker_model_ids(catalog: &Catalog) -> Vec<String> {
    catalog.model_lines()
}

/// The SDK option bag for one model: the provider's options, with the model's on top.
///
/// `provider.ts:1676` seeds the bag from the provider — `const options = {
/// ...provider.options }` — and model-level options are overlaid deep, the model
/// winning at each colliding leaf (`:1497`). [`model_spec`] forwarded only
/// `model.options`, so every provider-level option was silently dropped:
/// `useCompletionUrls`
/// ([`zuno_provider_compatible::surface::use_completion_urls`]), `capabilities` and
/// `extraBody` ([`zuno_provider_compatible::provider`]) all have readers, and all were
/// inert when set where the docs say to set them.
///
/// # Why the overlay is deep rather than a replace
///
/// The bag's values are objects — `extraBody`, `capabilities`, `modelCapabilities` — so
/// a shallow overlay would make a model that narrows one `extraBody` key discard every
/// other key the provider set. Upstream's `mergeDeep` does not, and neither does this.
///
/// # Direction is load-bearing
///
/// Provider first, model second, because the later write wins. Swapping the two is
/// caught by `turn_tests::a_model_option_wins_over_a_provider_option_of_the_same_name`
/// and, on the wire, by
/// `provider_options::a_model_level_option_overrides_the_provider_level_one_of_the_same_name`.
fn forwarded_options(
    provider: Option<&zuno_llm::catalog::ResolvedProvider>,
    model: &zuno_llm::catalog::ResolvedModel,
) -> serde_json::Map<String, serde_json::Value> {
    let mut bag = serde_json::Map::new();
    let sources = provider
        .map(|provider| &provider.options)
        .into_iter()
        .chain(std::iter::once(&model.options));
    for source in sources {
        for (name, value) in source {
            if resolved_elsewhere(name) {
                continue;
            }
            overlay(&mut bag, name, value);
        }
    }
    bag
}

/// Write `value` at `name`, merging two objects rather than replacing one with the other.
///
/// `value` is the overlay, so it wins at every leaf it names and leaves the rest of an
/// existing object intact. This is `mergeDeep(existing, incoming)` (`provider.ts:1497`)
/// narrowed to the one direction this module needs.
fn overlay(
    bag: &mut serde_json::Map<String, serde_json::Value>,
    name: &str,
    value: &serde_json::Value,
) {
    if let (Some(serde_json::Value::Object(target)), serde_json::Value::Object(source)) =
        (bag.get_mut(name), value)
    {
        for (key, value) in source {
            overlay(target, key, value);
        }
        return;
    }
    bag.insert(name.to_owned(), value.clone());
}

/// Where a provider's transport endpoint comes from, in the oracle's order.
///
/// The precedence is `resolveSDK`'s
/// (`packages/opencode/src/provider/provider.ts:1698-1700`), with the bedrock loader's
/// `endpoint ?? baseURL` normalisation folded in (`:355-358`):
///
/// 1. `provider.<id>.options.endpoint`
/// 2. `provider.<id>.options.baseURL`
/// 3. the catalog's `model.api.url`
///
/// # Why this is not resolved during the catalog merge
///
/// `model.api.url` is the merge's own ladder — `config.provider.api` → `provider.api`
/// → the catalog's entry (`:1455`) — and it is what `opencode models --verbose`
/// prints. Upstream leaves that field alone and chooses the transport URL here, at
/// SDK construction, because `options` belongs to the *provider* and `api` belongs to
/// the *model*. Promoting an option into `api.url` during the merge would make the
/// printed catalog disagree with the oracle's and would leave two readers of the same
/// question. After this function, [`Spec::base_url`] is the only answer to it.
///
/// # Why `api` is not a URL first
///
/// Upstream treats `model.api` as an SDK-shape hint: `:230-232` reads
/// `model.api.endpoint` to choose `sdk.responses` over `sdk.chat`, and `:368` reads
/// `model.api.transport` to pick a factory. Its `url` is the catalog's rung, which is why it
/// is last here rather than first. A provider configured the documented way — endpoint
/// in `options.baseURL`, nothing top-level — carries no `api.url` at all.
///
/// # Ordering is load-bearing
///
/// `endpoint` must be consulted before `baseURL`, and both before `api.url`. Any
/// reordering is caught by
/// `provider_endpoint::endpoint_wins_over_base_url_when_both_are_set` and by
/// [`crate::cmd::turn_tests`]'s ladder test, which point the losing rungs at a dead
/// port precisely so a swap fails instead of passing more slowly.
fn provider_endpoint(
    provider: Option<&zuno_llm::catalog::ResolvedProvider>,
    model: &zuno_llm::catalog::ResolvedModel,
) -> Option<String> {
    provider_option_endpoint(provider)
        .into_iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .chain(std::iter::once(model.api.url.clone()))
        .find(|url| !url.is_empty())
}

fn provider_option_endpoint(
    provider: Option<&zuno_llm::catalog::ResolvedProvider>,
) -> Option<&serde_json::Value> {
    let provider = provider?;
    ENDPOINT_OPTIONS.iter().find_map(|key| {
        provider
            .options
            .get(*key)
            .filter(|value| value.as_str().is_some_and(|url| !url.is_empty()))
    })
}

fn openai_surface(
    provider: Option<&zuno_llm::catalog::ResolvedProvider>,
    model: &zuno_llm::catalog::ResolvedModel,
) -> ApiSurface {
    match model.api.endpoint {
        Some(ModelEndpoint::Chat) => ApiSurface::Chat,
        Some(ModelEndpoint::Responses) => ApiSurface::Responses,
        Some(ModelEndpoint::Messages) => ApiSurface::Messages,
        None if provider_option_endpoint(provider).is_some() => ApiSurface::Chat,
        None => ApiSurface::Default,
    }
}

/// Substitute every `${VAR}` in a chosen base URL from the resolved environment.
///
/// `zuno_llm::catalog::ModelApi::url` has documented itself as *"possibly containing
/// `${VAR}` placeholders"* since the catalog was ported, and nothing expanded them:
/// `https://${REGION}.api.example.com/v1` was handed to the transport with the braces
/// still in it, so a parameterised gateway could not be dialled at all. This is
/// `resolveSDK`'s second pass (`provider.ts:1712-1715`).
///
/// # An unset variable keeps its placeholder
///
/// The oracle's replacer is `(item, key) => envs[String(key)] ?? item` — `?? item`, so a
/// name the environment does not carry yields the original `${VAR}` text. Substituting
/// the empty string instead would turn one typo into `http:///v1`, a URL that fails
/// naming nothing the user wrote, and would silently collapse the authority of any URL
/// whose host *is* the placeholder. [`zuno_paths::Env::value`] is the nullish read that
/// matches `envs[key]`, which also means a variable set to the empty string **does**
/// substitute empty — `""` is not nullish in JavaScript either.
///
/// # Where the two passes went
///
/// Upstream expands twice, and the first pass is
/// `varsLoaders[model.providerID]` — a provider-specific hook registered by the
/// `azure`, `amazon-bedrock`, `google-vertex` and `cloudflare` custom loaders
/// (`provider.ts:270`, `:364`, `:521`, `:760`) so that, for example, a bedrock URL can
/// name `${AWS_REGION}` and be filled from `options.region`. **This workspace has no
/// custom-loader registry at all**, so there is no second source to consult and
/// inventing one to mirror the shape would ship a hook with nothing behind it. Only the
/// environment pass is ported. When a loader registry does arrive it belongs *before*
/// this call, because upstream's environment pass runs last and therefore cannot be
/// overridden by a loader.
fn expand_variables(url: &str, env: &zuno_paths::Env) -> String {
    let bytes = url.as_bytes();
    let mut expanded = String::with_capacity(url.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        // `[^}]+` in the oracle's regex: at least one character, none of them `}`, so
        // `${}` is not a placeholder and an unterminated `${` is not one either.
        if bytes[cursor] == b'$'
            && bytes.get(cursor + 1) == Some(&b'{')
            && let Some(offset) = url[cursor + 2..].find('}')
            && offset > 0
        {
            let key = &url[cursor + 2..cursor + 2 + offset];
            let end = cursor + 3 + offset;
            expanded.push_str(env.value(key).unwrap_or(&url[cursor..end]));
            cursor = end;
            continue;
        }
        let character = url[cursor..]
            .chars()
            .next()
            .unwrap_or_else(|| unreachable!("cursor is inside the string and aligned"));
        expanded.push(character);
        cursor += character.len_utf8();
    }
    expanded
}

/// The transport spec for one model: endpoint, headers and forwarded options.
///
/// The option bag comes from [`forwarded_options`], which seeds it from the provider
/// and overlays the model's on top. It does **not** carry the endpoint keys or `apiKey`;
/// see [`resolved_elsewhere`].
///
/// # The two endpoint steps are ordered, and the order is load-bearing
///
/// [`provider_endpoint`] chooses a rung and [`expand_variables`] then substitutes into
/// whatever it chose — never the reverse. The oracle's `iife` reads
/// `options["baseURL"] !== ""` *before* expanding (`provider.ts:1699-1700` then `:1712`), so a
/// rung is accepted or skipped on its **unexpanded** text: `"baseURL": "${EMPTY}"` with
/// `EMPTY=""` set is a non-empty rung that wins the ladder and then expands to nothing,
/// rather than an empty rung that falls through to the catalog. Expanding first would
/// invert that, and is caught by
/// `turn_tests::a_rung_is_chosen_before_expansion_not_after`.
///
/// # Errors
///
/// Returns a message naming the key to set when neither the provider's options nor the
/// catalog supply an endpoint. Refusing here rather than at the transport is the whole
/// point: [`zuno_provider_compatible::CompatibleProvider::new`] answers a missing base
/// URL with `IncompleteConfiguration`, which surfaces as `unrecoverable provider
/// failure (status=None)` after a turn has already been composed and names nothing a
/// user can act on.
fn model_spec(
    catalog: &Catalog,
    model: &zuno_llm::catalog::ResolvedModel,
    env: &zuno_paths::Env,
) -> Result<Spec, String> {
    let provider = catalog.provider(&model.provider_id);
    let transport = model.api.transport.ok_or_else(|| {
        format!(
            "model `{}/{}` has no native provider transport",
            model.provider_id, model.id
        )
    })?;
    let custom_openai =
        transport == ProviderTransport::Openai && provider_option_endpoint(provider).is_some();
    let factory_key = if custom_openai {
        COMPATIBLE_PROVIDER
    } else {
        provider_factory_key(Some(transport))
            .ok_or_else(|| format!("unsupported provider transport `{transport}`"))?
    };
    let surface = match transport {
        ProviderTransport::Anthropic | ProviderTransport::GoogleVertexAnthropic => {
            ApiSurface::Messages
        }
        ProviderTransport::Openai => openai_surface(provider, model),
        // Keep the generic compatible transport unresolved here. The native
        // compatible provider still has the provider id and model endpoint map, so
        // its Rust profiles can select Azure, Copilot, or a declared Responses
        // surface. `Default` becomes Chat only after those rules have had a chance
        // to run.
        ProviderTransport::OpenaiCompatible => ApiSurface::Default,
        ProviderTransport::Openrouter => ApiSurface::Chat,
        _ => ApiSurface::Default,
    };
    let mut spec = Spec::new(&model.provider_id)
        .with_factory(factory_key)
        .with_surface(surface);
    if let Some(endpoint) = provider_endpoint(provider, model) {
        spec = spec.with_base_url(expand_variables(&endpoint, env));
    } else if factory_key == COMPATIBLE_PROVIDER {
        return Err(format!(
            "provider `{}` has no endpoint: set \
             `provider.{}.options.baseURL` (or `options.endpoint`) to the API base URL",
            model.provider_id, model.provider_id
        ));
    }
    if (factory_key == "amazon-bedrock" || factory_key == "amazon-bedrock/mantle")
        && let Some(region) = provider_string_option(provider, "region")
            .or_else(|| env.value("AWS_REGION").map(str::to_owned))
            .or_else(|| env.value("AWS_DEFAULT_REGION").map(str::to_owned))
    {
        spec = spec.with_region(region);
    }
    if factory_key == "google-vertex" || factory_key == "google-vertex/anthropic" {
        if let Some(project) = provider_string_option(provider, "project")
            .or_else(|| env.value("GOOGLE_VERTEX_PROJECT").map(str::to_owned))
            .or_else(|| env.value("GOOGLE_CLOUD_PROJECT").map(str::to_owned))
            .or_else(|| env.value("GCP_PROJECT").map(str::to_owned))
            .or_else(|| env.value("GCLOUD_PROJECT").map(str::to_owned))
        {
            spec = spec.with_project(project);
        }
        let location = provider_string_option(provider, "location")
            .or_else(|| env.value("GOOGLE_VERTEX_LOCATION").map(str::to_owned))
            .or_else(|| env.value("GOOGLE_CLOUD_LOCATION").map(str::to_owned))
            .or_else(|| env.value("VERTEX_LOCATION").map(str::to_owned))
            .unwrap_or_else(|| "us-central1".to_owned());
        spec = spec.with_region(location);
    }
    for (name, value) in &model.headers {
        spec = spec.with_header(name, value);
    }
    for (name, value) in forwarded_options(provider, model) {
        spec = spec.with_option(name, value);
    }
    if !spec
        .options
        .keys()
        .any(|name| generation::MAX_TOKENS_KEYS.contains(&name.as_str()))
    {
        spec = spec.with_option(generation::MAX_TOKENS, json!(output_ceiling(model)));
    }
    if factory_key == COMPATIBLE_PROVIDER {
        // `family::resolve` accepts an unlisted identity only when its resolved model
        // explicitly selects the generic compatible transport. Carry that typed
        // decision across this boundary rather than making the family guess.
        spec = spec.with_option(
            zuno_provider_compatible::family::TRANSPORT_OPTION,
            json!(ProviderTransport::OpenaiCompatible.as_str()),
        );
        if let Some(endpoint) = model.api.endpoint {
            spec = spec.with_option(
                zuno_provider_compatible::surface::MODEL_ENDPOINTS_OPTION,
                json!({ model.api.id.clone(): endpoint }),
            );
        }
    }
    Ok(spec)
}

fn provider_string_option(
    provider: Option<&zuno_llm::catalog::ResolvedProvider>,
    name: &str,
) -> Option<String> {
    provider
        .and_then(|provider| provider.options.get(name))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn credential_value(credential: &Credential) -> String {
    match credential {
        Credential::Api { key, .. } => key.expose().to_owned(),
        Credential::Oauth { access, .. } => access.expose().to_owned(),
        Credential::WellKnown { token, .. } => token.expose().to_owned(),
    }
}

/// Overlay the agent's sampling declarations onto the model's resolved [`Spec`].
///
/// # Why the agent wins
///
/// The oracle merges the agent's option bag last —
/// `mergeOptions(mergeOptions(base, model.options), agent.options)`
/// (`session/llm/request.ts:91`) — and prefers the agent's sampling scalars over the
/// per-model defaults it would otherwise compute:
/// `input.agent.temperature ?? ProviderTransform.temperature(input.model)` and
/// `input.agent.topP ?? ProviderTransform.topP(input.model)` (`:124-127`). Running
/// after [`model_spec`] reproduces both, and it also means an agent may raise or
/// lower the output ceiling `model_spec` defaulted, because `options` merges over it.
///
/// # Why `top_p` is written under the camelCase name
///
/// [`AgentConfig::top_p`](zuno_config::schema::AgentConfig) is the *config* spelling
/// the oracle accepts (`v1/config/agent.ts:19` declares `top_p`), and
/// `agent.ts:286` immediately assigns it to `item.topP`. Adapters read the SDK
/// vocabulary, so the rename happens here rather than in six adapters.
///
/// A field the agent left unset writes nothing, so an agent that declares no
/// sampling leaves the request byte-identical to one resolved without an agent.
fn with_agent_options(mut spec: Spec, agent: &zuno_catalog::agent::Agent) -> Spec {
    for (name, value) in &agent.options {
        if resolved_elsewhere(name) {
            continue;
        }
        spec = spec.with_option(name.clone(), value.clone());
    }
    if let Some(temperature) = agent.temperature {
        spec = spec.with_option(generation::TEMPERATURE, json!(temperature));
    }
    if let Some(top_p) = agent.top_p {
        spec = spec.with_option(generation::TOP_P, json!(top_p));
    }
    spec
}

/// The effort level this turn should resolve, session choice first.
///
/// # Why the agent's variant is a fallback and not an override
///
/// A session-level choice is a live user action — the effort picker — and the
/// agent's `variant` is a configured default, so the live choice wins. That is also
/// the oracle's order: `input.variant ?? (ag.variant && ...)`
/// (`session/prompt.ts:654`).
///
/// # Why the agent's model must match
///
/// The same line gates the agent's variant on `same`, computed at `:648` as the
/// agent's *own* configured model equalling the model being sent to, and the config
/// schema says so in prose: "applies only when using the agent's configured model"
/// (`v1/config/agent.ts:16-17`). The reason is that a variant names a level *this
/// model* declares; carried onto a model switched to by hand it would either name a
/// variant that model never declared or, worse, silently select a different level
/// than the name means. An agent with no `model` therefore never contributes one —
/// which is why that combination is rejected at parse time rather than accepted and
/// rejected by the native agent schema.
fn turn_effort(
    session: Option<zuno_llm::effort::ReasoningEffort>,
    agent: &zuno_catalog::agent::Agent,
    provider_id: &str,
    model_id: &str,
) -> Option<zuno_llm::effort::ReasoningEffort> {
    session.or_else(|| {
        let declared = agent.model.as_deref()?;
        let (declared_provider, declared_model) = declared.split_once('/')?;
        (declared_provider == provider_id && declared_model == model_id)
            .then(|| agent.variant.as_deref()?.parse().ok())
            .flatten()
    })
}

/// Canonical effort variants a model explicitly declares, weakest first.
fn declared_reasoning_efforts(
    model: &zuno_llm::catalog::ResolvedModel,
) -> Vec<zuno_llm::effort::ReasoningEffort> {
    zuno_llm::effort::ReasoningEffort::ALL
        .into_iter()
        .filter(|effort| model.variants.contains_key(effort.as_str()))
        .collect()
}

/// Reasoning levels safe for an interactive selector to offer on `model`.
///
/// Explicit canonical variants are authoritative even when a custom provider omitted
/// the coarse `reasoning` capability. When no variants are declared, a true capability
/// means the provider-neutral scale is available through the generic mapping.
fn selectable_reasoning_efforts(
    model: &zuno_llm::catalog::ResolvedModel,
) -> Vec<zuno_llm::effort::ReasoningEffort> {
    let declared = declared_reasoning_efforts(model);
    if declared.is_empty() && model.capabilities.reasoning {
        zuno_llm::effort::ReasoningEffort::ALL.to_vec()
    } else {
        declared
    }
}

/// The provider-native reasoning controls for `effort` on `model`, if any.
///
/// Empty when the session chose no level, or when the catalog says the model does not
/// reason. The capability check is what stops a level chosen on one model leaking onto
/// the next: [`TurnPlan::resolve`] runs again on every model switch, so a model without
/// reasoning resolves to no controls even while the session still remembers a level.
///
/// [`EffortCapabilities::default`] is passed for the same reason [`delegation_facts`]
/// passes it: the resolved catalog carries no adaptive or token-budget shape. The
/// consequence is bounded and stated there — a model that declares its own variant for
/// the level still wins, because `resolve_effort` prefers a declared variant over any
/// generic mapping.
fn session_reasoning_options(
    effort: Option<zuno_llm::effort::ReasoningEffort>,
    model: &zuno_llm::catalog::ResolvedModel,
) -> serde_json::Map<String, serde_json::Value> {
    let declared_efforts = declared_reasoning_efforts(model);
    let Some(effort) =
        effort.filter(|effort| model.capabilities.reasoning || declared_efforts.contains(effort))
    else {
        return serde_json::Map::new();
    };
    let Some(family) = effort_family(model.api.transport) else {
        return serde_json::Map::new();
    };
    let mut declared = zuno_llm::effort::DeclaredVariants::new();
    for (name, options) in &model.variants {
        if let Ok(level) = name.parse::<zuno_llm::effort::ReasoningEffort>() {
            declared = declared.with(level, options.clone());
        }
    }
    zuno_llm::effort::resolve_effort(
        family,
        effort,
        zuno_llm::effort::EffortCapabilities::default(),
        &declared,
    )
    .options
}

struct Resolver {
    requested_agent: String,
    system_prompt: String,
    prompt_assembly: Option<PromptAssembly>,
    max_steps: u32,
    requested_provider: String,
    requested_model: String,
    wire_model: String,
    reasoning_options: serde_json::Map<String, serde_json::Value>,
    spec: Spec,
}

impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == self.requested_agent).then(|| {
            let agent = ResolvedAgent::new(
                self.requested_agent.clone(),
                self.system_prompt.clone(),
                self.max_steps,
            );
            match &self.prompt_assembly {
                Some(assembly) => agent.with_prompt_assembly(assembly.clone()),
                None => agent,
            }
        })
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<EngineModel> {
        (provider_id == self.requested_provider && model_id == self.requested_model).then(|| {
            EngineModel::new(
                self.spec.clone(),
                self.wire_model.clone(),
                self.spec.surface,
            )
            .with_catalog_identity(&self.requested_provider, &self.requested_model)
            .with_reasoning_options(self.reasoning_options.clone())
        })
    }
}

impl Resolver {
    fn append_prompt_section(
        &mut self,
        id: impl Into<String>,
        source: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<(), String> {
        let content = content.into();
        if content.is_empty() {
            return Ok(());
        }
        if let Some(assembly) = &mut self.prompt_assembly {
            assembly.push(id, source, content).map_err(to_string)?;
            self.system_prompt = assembly.render();
        } else if self.system_prompt.is_empty() {
            self.system_prompt = content;
        } else {
            self.system_prompt.push_str("\n\n");
            self.system_prompt.push_str(&content);
        }
        Ok(())
    }
}

fn agent_prompt_source(agent: &zuno_catalog::agent::Agent) -> String {
    match &agent.source {
        zuno_catalog::agent::AgentSource::Native => format!("native:{}", agent.name),
        zuno_catalog::agent::AgentSource::NativeOverridden => {
            format!("native+configuration:{}", agent.name)
        }
        zuno_catalog::agent::AgentSource::Config => format!("configuration:agent.{}", agent.name),
        zuno_catalog::agent::AgentSource::Markdown { path } => path.display().to_string(),
    }
}

fn ensure_project(
    connection: &rusqlite::Connection,
    project: &zuno_paths::project::ResolvedProject,
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

fn prepare_turn_host(
    connection: &rusqlite::Connection,
    plan: &TurnPlan,
    now: i64,
) -> Result<PreparedTurnHost, String> {
    match &plan.session {
        SessionChoice::Existing(session_id) => {
            let session = zuno_db::session::get(connection, session_id).map_err(to_string)?;
            return Ok(prepared_existing_session(session));
        }
        SessionChoice::Continue => {
            let session = zuno_db::session::list(
                connection,
                &zuno_db::session::ListQuery::directory(plan.directory.to_string_lossy())
                    .active_only()
                    .with_limit(1),
            )
            .map_err(to_string)?
            .into_iter()
            .next()
            .ok_or_else(|| "no session found to continue in the current directory".to_owned())?;
            return Ok(prepared_existing_session(session));
        }
        SessionChoice::Prepared(identity) if identity.is_materialized() => {
            let session = zuno_db::session::get(connection, identity.id()).map_err(to_string)?;
            return Ok(prepared_existing_session(session));
        }
        SessionChoice::New | SessionChoice::Prepared(_) => {}
    }

    let identity = match &plan.session {
        SessionChoice::Prepared(identity) => identity.clone(),
        SessionChoice::New => PreparedSessionIdentity::pending(prefixed_id("ses")),
        SessionChoice::Continue | SessionChoice::Existing(_) => {
            unreachable!("existing choices returned above")
        }
    };
    let title = plan
        .title
        .clone()
        .unwrap_or_else(|| "New session".to_owned());
    let mut input = zuno_db::session::SessionCreate::new(
        identity.id(),
        Uuid::new_v4().simple().to_string(),
        &plan.project.id,
        plan.project.directory.to_string_lossy().into_owned(),
        plan.directory.to_string_lossy().into_owned(),
        title.clone(),
        crate::RUST_PACKAGE_VERSION,
    )
    .at(now);
    input.agent = Some(plan.agent.name.clone());
    input.model = Some(zuno_db::session::model_reference(
        &plan.provider_id,
        &plan.model_id,
    ));
    Ok(PreparedTurnHost {
        identity,
        title,
        directory: plan.directory.to_string_lossy().into_owned(),
        usage: zuno_db::session::SessionUsage::default(),
        materializer: SessionMaterializer::Pending(Box::new(input)),
    })
}

fn prepared_existing_session(session: zuno_db::session::Session) -> PreparedTurnHost {
    PreparedTurnHost {
        identity: PreparedSessionIdentity::existing(session.id),
        title: session.title,
        directory: session.directory,
        usage: session.usage,
        materializer: SessionMaterializer::Existing,
    }
}

#[cfg(test)]
fn resolve_session(
    connection: &mut rusqlite::Connection,
    plan: &TurnPlan,
    now: i64,
) -> Result<zuno_db::session::Session, String> {
    let prepared = prepare_turn_host(connection, plan, now)?;
    match prepared.materializer {
        SessionMaterializer::Existing => {
            zuno_db::session::get(connection, prepared.identity.id()).map_err(to_string)
        }
        SessionMaterializer::Pending(mut input) => {
            input.time = Some(now);
            let transaction = connection.transaction().map_err(to_string)?;
            let creation = zuno_db::session::create(&transaction, &input).map_err(to_string)?;
            transaction.commit().map_err(to_string)?;
            Ok(creation.into_session())
        }
    }
}

fn recent_sessions(
    connection: &rusqlite::Connection,
    directory: &str,
    limit: u32,
) -> Result<Vec<zuno_db::session::Session>, zuno_error::DbError> {
    let mut query = zuno_db::session::ListQuery::directory(directory)
        .active_only()
        .with_limit(limit);
    query.roots = true;
    zuno_db::session::list(connection, &query)
}

fn switchable_session(
    connection: &rusqlite::Connection,
    directory: &str,
    target_session_id: &str,
) -> Result<Option<zuno_db::session::Session>, zuno_error::DbError> {
    let target = zuno_db::session::get(connection, target_session_id)?;
    if target.directory != directory || !target.is_root() || target.is_archived() {
        return Ok(None);
    }
    Ok(Some(target))
}

struct UserMessageInput<'a> {
    session_id: &'a str,
    agent: &'a str,
    provider_id: &'a str,
    model_id: &'a str,
    text: &'a str,
    message_id: Option<&'a str>,
    now: i64,
}

#[cfg(test)]
fn persist_user_message(
    connection: &rusqlite::Connection,
    input: UserMessageInput<'_>,
) -> Result<(), String> {
    let message_id = input
        .message_id
        .map_or_else(|| prefixed_id("msg"), str::to_owned);
    let message = zuno_db::message::MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": input.session_id,
        "role": "user",
        "time": {"created": input.now},
        "agent": input.agent,
        "model": {"providerID": input.provider_id, "modelID": input.model_id}
    }))
    .map_err(to_string)?;
    let part = zuno_db::message::PartRecord::from_json(
        json!({
            "id": prefixed_id("prt"),
            "sessionID": input.session_id,
            "messageID": message.id,
            "type": "text",
            "text": input.text
        }),
        input.now,
    )
    .map_err(to_string)?;
    let store = zuno_db::message::MessageStore::new(connection);
    store
        .put_message_at(&message, input.now)
        .map_err(to_string)?;
    store.put_part_at(&part, input.now).map_err(to_string)?;
    Ok(())
}

fn prepare_user_message(
    input: UserMessageInput<'_>,
    content: Option<&[RequestContentBlock]>,
) -> Result<
    (
        zuno_db::message::MessageRecord,
        Vec<zuno_db::message::PartRecord>,
    ),
    String,
> {
    let message_id = input
        .message_id
        .map_or_else(|| prefixed_id("msg"), str::to_owned);
    let mut message = zuno_db::message::MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": input.session_id,
        "role": "user",
        "time": {"created": input.now},
        "agent": input.agent,
        "model": {"providerID": input.provider_id, "modelID": input.model_id}
    }))
    .map_err(to_string)?;
    let parts = match content {
        Some(content) => request_content_parts(&input, &message.id, content)?,
        None => vec![
            zuno_db::message::PartRecord::from_json(
                json!({
                    "id": prefixed_id("prt"),
                    "sessionID": input.session_id,
                    "messageID": message.id,
                    "type": "text",
                    "text": input.text
                }),
                input.now,
            )
            .map_err(to_string)?,
        ],
    };

    for part in &parts {
        if part.session_id != input.session_id || part.message_id != message.id {
            return Err("prepared part belongs to a different message or session".to_owned());
        }
    }
    message
        .data
        .insert("role".to_owned(), Value::String("user".to_owned()));
    Ok((message, parts))
}

fn request_content_parts(
    input: &UserMessageInput<'_>,
    message_id: &str,
    content: &[RequestContentBlock],
) -> Result<Vec<zuno_db::message::PartRecord>, String> {
    if content.is_empty() {
        return Err("resolved prompt content must not be empty".to_owned());
    }
    content
        .iter()
        .enumerate()
        .map(|(offset, block)| {
            let value = match block {
                RequestContentBlock::Text { text } => json!({
                    "id": prefixed_id("prt"),
                    "sessionID": input.session_id,
                    "messageID": message_id,
                    "type": "text",
                    "text": text,
                }),
                RequestContentBlock::Image { media_type, data } => json!({
                    "id": prefixed_id("prt"),
                    "sessionID": input.session_id,
                    "messageID": message_id,
                    "type": "file",
                    "mime": media_type,
                    "data": data,
                    "url": format!("data:{media_type};base64,{data}"),
                }),
                RequestContentBlock::SignedThinking { .. }
                | RequestContentBlock::ProviderEncryptedReasoning { .. }
                | RequestContentBlock::ToolUse { .. }
                | RequestContentBlock::ToolResult { .. } => {
                    return Err(
                        "resolved user prompt content may contain only text and images".to_owned(),
                    );
                }
            };
            let created = input
                .now
                .saturating_add(i64::try_from(offset).unwrap_or(i64::MAX));
            zuno_db::message::PartRecord::from_json(value, created).map_err(to_string)
        })
        .collect()
}

fn persist_prepared_user_message(
    connection: &rusqlite::Connection,
    message: &zuno_db::message::MessageRecord,
    parts: &[zuno_db::message::PartRecord],
) -> Result<(), DbError> {
    let store = zuno_db::message::MessageStore::new(connection);
    store.put_message_at(message, message.time_created)?;
    for part in parts {
        store.put_part_at(part, part.time_created)?;
    }
    Ok(())
}

pub(super) fn prefixed_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(super) fn finish_with_shutdown<T>(
    outcome: Result<T, String>,
    shutdown: Result<(), String>,
) -> Result<T, String> {
    match (outcome, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Err(error), Err(shutdown)) => {
            Err(format!("{error}; host shutdown also failed: {shutdown}"))
        }
    }
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
