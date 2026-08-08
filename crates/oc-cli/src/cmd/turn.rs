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

use oc_agent::model_policy::{AnyModel, ModelChoice, ModelPolicy};
use oc_auth::Credential;
use oc_engine::compaction::{CompactionState, TokenWindow};
use oc_engine::dispatch::ToolRegistryDispatcher;
use oc_engine::interrupt::InterruptSignal;
use oc_engine::r#loop::{
    AgentModelResolver, ResolvedAgent, ResolvedModel as EngineModel, RunTurnRequest, TurnContext,
    TurnError, TurnEvent, TurnEventSender, run_turn,
};
use oc_engine::prelude::{
    InternalAgent, InternalProviders, Internals, PreludeContext, PreludeOutcome, run_prelude,
};
use oc_error::ProviderError;
use oc_llm::cache::{DynamicContext, McpToolStatus};
use oc_llm::catalog::{Catalog, CatalogProvenance, CatalogSource, ResolveInput};
use oc_llm::event::StreamEvent;
use oc_llm::registry::{ApiSurface, Provider, ProviderRegistry, Spec};
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
    internals: Internals,
    window: TokenWindow,
    notes: Vec<String>,
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
        let loaded = CatalogSource::resolve(env, &layout)
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
        let catalog = Catalog::resolve(loaded.document(), &input);

        let agents =
            oc_catalog::agent::load(&directory, worktree.as_deref(), env).map_err(to_string)?;
        let agent_name = options.agent.as_deref().unwrap_or(DEFAULT_AGENT);
        let agent = agents
            .into_iter()
            .find(|entry| entry.name == agent_name)
            .ok_or_else(|| format!("Agent not found: {agent_name}"))?;
        let requested_model = options.model.as_deref().or(agent.model.as_deref());
        let (provider_id, model_id, catalog_model) =
            select_model(&catalog, requested_model, loaded.provenance())?;
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
            spec: model_spec(&catalog, catalog_model, env)?,
        };
        let window = TokenWindow {
            context: token_count(catalog_model.limit.context),
            max_output: token_count(catalog_model.limit.output),
        };
        let mut notes = Vec::new();
        let internals = resolve_internals(
            &config,
            &catalog,
            &provider_id,
            &model_id,
            catalog_model,
            env,
            &mut notes,
        )?;
        Ok(Self {
            directory,
            project,
            config,
            agent,
            credential: resolved_credential(
                catalog.provider(&provider_id),
                credentials.get(&provider_id),
            ),
            provider_id,
            model_id,
            resolver,
            session: options.session.clone(),
            title: options.title.clone(),
            internals,
            window,
            notes,
        })
    }
}

/// A catalog token ceiling, which is JSON and therefore a float, as a token count.
///
/// The catalog stores `limit.context` and `limit.output` as `f64` because
/// `models.dev` publishes them as JSON numbers. A negative or non-finite value is
/// zero rather than a wrapped integer, and zero is already meaningful:
/// [`oc_engine::compaction::CompactionPolicy`] treats a zero window as
/// "compaction cannot be triggered by a threshold", which is the correct reading of a
/// model that declares no window.
fn token_count(limit: f64) -> u64 {
    if limit.is_finite() && limit > 0.0 {
        limit as u64
    } else {
        0
    }
}

/// Resolve `compaction`, `title` and `summary` through todo 64's model policy.
///
/// Iterates [`oc_agent::builtin::INTERNAL_NAMES`] rather than three literals, so an
/// internal added there is resolved here with no edit — which is the whole reason
/// that constant exists. Each name's prompt comes from
/// [`oc_catalog::agent::builtin`], which is where the upstream native's text lives;
/// nothing is written twice.
///
/// # Why an internal cannot leave the session's provider
///
/// [`ModelPolicy`] may legitimately answer with a model under a different provider,
/// and this function then declines it and records why. [`TurnHost::open`] wires
/// exactly one credential — the session provider's — so honouring a cross-provider
/// answer would mean presenting that credential to a different vendor's endpoint.
/// Falling back to the session's own model costs a larger model for a small job;
/// the alternative costs the user's API key. The note is emitted on the turn's event
/// channel so the downgrade is visible rather than silent.
///
/// The preset rung of the precedence chain is reachable but unfed: nothing in this
/// workspace discovers a [`oc_agent::model_policy::PresetLibrary`] yet, so only the
/// per-agent override and the session model can answer today.
fn resolve_internals(
    config: &oc_config::schema::Config,
    catalog: &Catalog,
    provider_id: &str,
    model_id: &str,
    session_model: &oc_llm::catalog::ResolvedModel,
    env: &oc_paths::Env,
    notes: &mut Vec<String>,
) -> Result<Internals, String> {
    let session_choice = ModelChoice::new(format!("{provider_id}/{model_id}"));
    let mut policy = ModelPolicy::new().with_session_model(session_choice);
    if let Some(agents) = &config.agent {
        policy = policy.with_agent_overrides(agents);
    }

    let mut resolved = std::collections::BTreeMap::new();
    for name in oc_agent::builtin::INTERNAL_NAMES {
        let prompt = internal_prompt(name)?;
        let resolution = policy.resolve(name, &AnyModel);
        notes.extend(resolution.render_diagnostics());
        let chosen = resolution
            .model
            .as_ref()
            .and_then(|choice| {
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
                if !supports_compatible_transport(&model.api.npm) {
                    notes.push(format!(
                        "{name}: `{}` uses transport {}, which this runtime has no \
                         provider for; using `{provider_id}/{model_id}` instead",
                        choice.model, model.api.npm
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
                ),
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

/// The upstream native's prompt for one internal agent.
///
/// Read through [`oc_agent::builtin::internals`] rather than
/// [`oc_catalog::agent::builtin`] directly, because the roster is what decides which
/// internals this build has — reading past it would let the two disagree about the
/// set while both looked correct.
fn internal_prompt(name: &str) -> Result<String, String> {
    oc_agent::builtin::internals()
        .into_iter()
        .find(|agent| agent.name == name)
        .and_then(|agent| match agent.output {
            oc_agent::builtin::OutputContract::EnginePrompt { prompt } => Some(prompt.to_owned()),
            _ => None,
        })
        .ok_or_else(|| format!("internal agent `{name}` declares no prompt"))
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
    internals: Internals,
    compaction_config: oc_config::schema::CompactionConfig,
    compaction_state: CompactionState,
    window: TokenWindow,
    notes: Vec<String>,
}

/// The registry answering for whichever spec an internal agent resolved to.
///
/// A newtype rather than an `impl` on [`ProviderRegistry`] because the trait belongs
/// to `oc-engine` and the registry to `oc-llm`: neither crate may name the other's
/// concern, and this composition root is the one place that may name both.
struct RegistryProviders<'a>(&'a ProviderRegistry);

impl InternalProviders for RegistryProviders<'_> {
    fn provider_for(&self, agent: &InternalAgent) -> Result<Arc<dyn Provider>, String> {
        self.0
            .resolve(agent.model.provider.clone())
            .map_err(to_string)
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
            internals: plan.internals,
            compaction_config: plan.config.compaction.clone().unwrap_or_default(),
            compaction_state: CompactionState::default(),
            window: plan.window,
            notes: plan.notes,
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
    /// tracker: [`run_turn`] builds a fresh [`oc_llm::cache::PromptCache`] per call
    /// and every prelude write lands before the first `prepare_turn`, so the tracker
    /// only ever sees one prefix. A prelude folded into the loop's continuation would
    /// change the prefix between step 1 and step 2, which is exactly the violation
    /// `oc-llm` refuses.
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
        let outcome = self.run_prelude().await?;
        report_prelude(&events, &self.notes, &outcome).await?;
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
        .map_err(describe_turn_failure)?;
        Ok(())
    }

    /// Run every internal that applies before this turn.
    ///
    /// The `summary` internal is resolved by the same [`resolve_internals`] pass and
    /// reached through the same [`PreludeContext`] as the other two, but nothing here
    /// requests one: no surface in this workspace displays a session summary yet, and
    /// inventing a command to prove the wiring would ship a subcommand upstream does
    /// not have. What matters is that when a surface does want one it calls
    /// [`oc_engine::prelude::summarize`] with this context rather than resolving a
    /// second model of its own — resolving separately is exactly how all three
    /// internals came to be declared and never invoked.
    async fn run_prelude(&mut self) -> Result<PreludeOutcome, String> {
        let providers = RegistryProviders(&self.providers);
        let mut context = PreludeContext {
            connection: &mut self.connection,
            providers: &providers,
            internals: &self.internals,
            compaction: &self.compaction_config,
            window: self.window,
            state: &mut self.compaction_state,
        };
        run_prelude(&self.session_id, &mut context)
            .await
            .map_err(to_string)
    }
}

/// Render a failed turn, naming where a rejected credential is configured.
///
/// `authentication rejected by provider test` is accurate and useless: it names the
/// provider and not one place a user can act. Both places are named here because both
/// are legitimate — [`provider_api_key`] reads the first and [`oc_auth::AuthStore`] the
/// second — and neither the key nor any part of it is echoed, because the message is
/// built from the provider id alone.
///
/// A keyless provider is deliberately **not** refused earlier:
/// [`oc_provider_compatible::CompatibleProvider::new`] documents that a local endpoint
/// legitimately has no credential, so the gateway's rejection is the first moment a
/// missing key is known to be a problem.
fn describe_turn_failure(error: TurnError) -> String {
    match &error {
        TurnError::Provider(ProviderError::Auth { provider, .. }) => format!(
            "{error}: set `provider.{provider}.options.apiKey`, or run \
             `opencode auth login {provider}`"
        ),
        _ => error.to_string(),
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
) -> Result<(String, String, &'a oc_llm::catalog::ResolvedModel), String> {
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
        model_ids.sort_by(|left, right| oc_llm::catalog::collate::compare(left, right));
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
            ", or allow the catalog to load: OPENCODE_DISABLE_MODELS_FETCH is set, so \
             `{origin}` was not contacted and no cached catalog exists at `{}`",
            cache.display()
        ));
    }
    Err(message)
}

fn supports_compatible_transport(npm: &str) -> bool {
    matches!(
        npm,
        "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@openrouter/ai-sdk-provider"
    )
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
/// Spelled as the oracle spells it (`provider.ts:1719`), so an existing
/// `opencode.json` keeps working. Like the endpoint keys it is **excluded** from the
/// forwarded SDK bag — see [`resolved_elsewhere`] — because it is the credential, and
/// the credential travels one way only, through [`oc_provider_compatible::factory`]'s
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
fn provider_api_key(provider: Option<&oc_llm::catalog::ResolvedProvider>) -> Option<String> {
    provider
        .and_then(|provider| provider.options.get(API_KEY_OPTION))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// The credential one turn presents, config first.
///
/// See [`provider_api_key`] for the precedence and why it is that way round.
fn resolved_credential(
    provider: Option<&oc_llm::catalog::ResolvedProvider>,
    stored: Option<&Credential>,
) -> Option<String> {
    provider_api_key(provider).or_else(|| stored.map(credential_value))
}

/// The SDK option bag for one model: the provider's options, with the model's on top.
///
/// `provider.ts:1676` seeds the bag from the provider — `const options = {
/// ...provider.options }` — and model-level options are overlaid deep, the model
/// winning at each colliding leaf (`:1497`). [`model_spec`] forwarded only
/// `model.options`, so every provider-level option was silently dropped:
/// `useCompletionUrls`
/// ([`oc_provider_compatible::surface::use_completion_urls`]), `capabilities` and
/// `extraBody` ([`oc_provider_compatible::provider`]) all have readers, and all were
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
    provider: Option<&oc_llm::catalog::ResolvedProvider>,
    model: &oc_llm::catalog::ResolvedModel,
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
/// `model.api.npm` to pick a factory. Its `url` is the catalog's rung, which is why it
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
    provider: Option<&oc_llm::catalog::ResolvedProvider>,
    model: &oc_llm::catalog::ResolvedModel,
) -> Option<String> {
    provider
        .into_iter()
        .flat_map(|provider| {
            ENDPOINT_OPTIONS
                .into_iter()
                .filter_map(|key| provider.options.get(key))
        })
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .chain(std::iter::once(model.api.url.clone()))
        .find(|url| !url.is_empty())
}

/// Substitute every `${VAR}` in a chosen base URL from the resolved environment.
///
/// `oc_llm::catalog::ModelApi::url` has documented itself as *"possibly containing
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
/// whose host *is* the placeholder. [`oc_paths::Env::value`] is the nullish read that
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
fn expand_variables(url: &str, env: &oc_paths::Env) -> String {
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
/// point: [`oc_provider_compatible::CompatibleProvider::new`] answers a missing base
/// URL with `IncompleteConfiguration`, which surfaces as `unrecoverable provider
/// failure (status=None)` after a turn has already been composed and names nothing a
/// user can act on.
fn model_spec(
    catalog: &Catalog,
    model: &oc_llm::catalog::ResolvedModel,
    env: &oc_paths::Env,
) -> Result<Spec, String> {
    let provider = catalog.provider(&model.provider_id);
    let endpoint = provider_endpoint(provider, model).ok_or_else(|| {
        format!(
            "provider `{}` has no endpoint: set \
             `provider.{}.options.baseURL` (or `options.endpoint`) to the API base URL",
            model.provider_id, model.provider_id
        )
    })?;
    let mut spec = Spec::new(COMPATIBLE_PROVIDER)
        .with_surface(ApiSurface::Chat)
        .with_base_url(expand_variables(&endpoint, env));
    for (name, value) in &model.headers {
        spec = spec.with_header(name, value);
    }
    for (name, value) in forwarded_options(provider, model) {
        spec = spec.with_option(name, value);
    }
    Ok(spec)
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
