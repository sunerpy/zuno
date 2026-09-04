//! The one place a command turns configuration into an executable tool set.
//!
//! Todo 44 assembled the registry and todo 56 built `run`, and the two were never
//! joined: `run` constructed its dispatcher with an empty tool vector and
//! [`zuno_tool::AllowAll`], so no production path could execute a tool and no
//! production path consulted a permission rule. Both surfaces that drive a turn
//! come through here instead, because a second assembly site is how the two
//! diverge — one gaining a tool, or a permission gate, that the other lacks.
//!
//! # Why the approval collaborator refuses rather than prompts
//!
//! [`zuno_permission`]'s evaluator resolves `allow` and `deny` itself and only calls
//! the approval collaborator for `ask`. A headless run has nobody to ask: stdin is
//! the prompt source or a pipe, and stdout is the model's answer. Blocking would
//! hang a non-interactive invocation forever and prompting would corrupt the
//! output, so [`HeadlessApproval`] fails closed and names the rule the operator
//! must add. That is a deliberate divergence from the interactive surface, not an
//! oversight: `external_directory`, `doom_loop` and reading a `.env` file all
//! resolve to `ask` under the default ruleset, and each of them is a decision a
//! person should make.
//!
//! # Which built-ins are registered, and why the list is short of the slot table
//!
//! [`zuno_tools::registry::BUILTIN_ORDER`] is the canonical native slot order. This module
//! registers the ones whose implementation needs nothing but the workspace, the
//! database, and the collaborators [`ToolSelection`] carries. `plan_exit` needs a
//! live user to answer, `lsp` has no implementation in `zuno-tools` at all, and
//! `execute` is registered by the builder itself behind an experimental flag. An
//! unregistered slot is simply absent from the assembled vector, so the model is
//! never told about a tool that cannot run.
//!
//! # Why an absent slot is a defect and not a default
//!
//! `task` and `skill` sat unregistered here while both implementations were complete
//! and tested. Nothing failed loudly: the model was told those tools did not exist,
//! so it never called them, so no test that built its own registry noticed. A test
//! asserting a slot is present must therefore call [`assemble`] — the function
//! `zuno run`, `zuno serve` and the TUI all reach — because a hand-assembled registry
//! would have passed throughout. `tests/tool_runtime.rs` is that assertion.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use zuno_agent::profile::AgentProfile;
use zuno_agent::profile::ShellFilesystemAccess;
use zuno_config::schema::Config;
use zuno_config::schema::permission::PermissionMode;
use zuno_config::schema::sandbox::{
    SandboxBackendSelection as ConfigSandboxBackendSelection, SandboxMode as ConfigSandboxMode,
    SandboxNetworkMode, SandboxUnavailableAction as ConfigSandboxUnavailableAction,
};
use zuno_error::ToolError;
use zuno_orchestration::{CapabilitySnapshot, ToolSchemaIdentity, sha256_json};
use zuno_paths::Env;
use zuno_permission::Rule;
use zuno_permission::visibility::permission_key;
use zuno_sandbox::{
    NetworkAccess, SandboxBackendRequest, SandboxBackendSelection, SandboxError, SandboxMode,
    SandboxPolicy, SandboxResolution, SandboxResolutionKind, SandboxResolver,
    SandboxUnavailableAction, SystemSandboxResolver,
};
use zuno_tool::{
    OutputLimits, PermissionAsk, PermissionAsker, PermissionOrigin, Tool, ToolUiIntent, erase,
};
use zuno_tools::FileTools;
use zuno_tools::exposure::ExposureFlags;
use zuno_tools::question::{QuestionAsker, QuestionTool};
use zuno_tools::registry::{
    BuiltinSlot, CustomTool, McpToolLoader, McpToolSnapshot, RegistryFlags, ResolveInput,
    ToolRegistryBuilder,
};
use zuno_tools::search_common::{SearchScope, SearchTooling};
use zuno_tools::websearch::gating::{SearchConfig, require_provider};

/// The executable tools and the ruleset that governs them, for one turn.
pub(crate) struct ToolRuntime {
    /// Every executable, permission-filtered tool in provider order.
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// Connected tools whose schemas are revealed through `tool_search`.
    pub(crate) deferred_tool_ids: Vec<String>,
    /// The merged ruleset the dispatcher re-evaluates before every call.
    pub(crate) rules: Vec<Rule>,
    /// Same-name replacements made while assembling, for the host to surface.
    ///
    /// Carried out of assembly rather than printed inside it: a shadowed built-in is
    /// a defect a user must see, but which surface can say so — stderr, a transcript
    /// line — is the host's decision, not the registry's.
    pub(crate) suppressions: Vec<String>,
    /// Durable model-visible notice and one-time host warning for a Shell that runs
    /// natively under a confined request: the trusted `run-unconfined` fallback or an
    /// explicit `sandbox.backend: native` selection.
    pub(crate) sandbox_notice: Option<String>,
}

pub(crate) struct ToolSelection<'a> {
    pub(crate) provider_id: &'a str,
    pub(crate) model_id: &'a str,
    pub(crate) manifest: Arc<zuno_harness::ToolManifest>,
    pub(crate) contributions: Arc<zuno_harness::ToolContributions>,
    pub(crate) public_http: Arc<zuno_network::PublicHttpClient>,
    pub(crate) question: Option<Arc<dyn QuestionAsker>>,
    pub(crate) background_executions: Arc<zuno_pty::BackgroundExecutionService>,
    /// Test seam for a resolver supplied by the composition root.
    ///
    /// Production passes `None` and performs native discovery only when Shell
    /// survives the final Agent capability intersection.
    pub(crate) sandbox: Option<Arc<dyn SandboxResolver>>,
    pub(crate) todo_store: Arc<zuno_db::pool::Pool>,
    pub(crate) work_observer: Arc<dyn zuno_tools::WorkStateObserver>,
    pub(crate) goal_store: Arc<zuno_goal::GoalStore>,
    pub(crate) interaction_policy: zuno_goal::InteractionPolicy,
    pub(crate) mcp_loader: Option<Arc<dyn McpToolLoader>>,
    pub(crate) skills: Arc<zuno_catalog::skill::Skills>,
    pub(crate) skill_catalog: Option<Arc<zuno_catalog::skill::catalog::SkillCatalogService>>,
    pub(crate) capability: Arc<CapabilitySnapshot>,
    pub(crate) delegation: Delegation,
    pub(crate) product_agents: Arc<dyn zuno_tools::product_agent::ProductAgentHost>,
    pub(crate) workflows: Arc<dyn zuno_tools::workflow::WorkflowHost>,
    pub(crate) councils: Arc<dyn zuno_tools::council::CouncilHost>,
    pub(crate) job_controller: Arc<dyn zuno_tools::job_cancel::JobController>,
    pub(crate) memory: Option<Arc<dyn Tool>>,
    pub(crate) experience_search: Option<Arc<dyn Tool>>,
    /// Exact provider-visible tools from the immutable parent Attempt.
    pub(crate) tool_authority: Option<Arc<[ToolSchemaIdentity]>>,
}

#[derive(Clone)]
struct FrozenMcpToolLoader {
    tools: Vec<CustomTool>,
    eager_tool_ids: Vec<String>,
}

impl McpToolLoader for FrozenMcpToolLoader {
    fn tools(&self) -> Vec<CustomTool> {
        self.tools.clone()
    }

    fn eager_tool_ids(&self) -> Vec<String> {
        self.eager_tool_ids.clone()
    }

    fn snapshot(&self) -> McpToolSnapshot {
        McpToolSnapshot {
            tools: self.tools.clone(),
            eager_tool_ids: self.eager_tool_ids.clone(),
        }
    }
}

/// The collaborators `task` needs, which only a surface that can drive a turn has.
///
/// **Deliberately not `Option`.** `task` went unregistered for exactly as long as it
/// took nobody to pass a host, and an optional field turns that back into a runtime
/// choice a test can only catch by looking. Required, the compiler catches it: the
/// sole caller of [`assemble`] is
/// [`super::turn::TurnHost::open_with_runtime_mcp_and_observers`],
/// which by construction can host a child turn, so there is no surface that legitimately
/// assembles a turn's tools and cannot delegate.
pub(crate) struct Delegation {
    /// Creates the child session and drives its turn.
    pub(crate) host: Arc<dyn zuno_tools::task::ChildTurnHost>,
    /// Catalog facts for the models a delegation may name.
    pub(crate) facts: Arc<dyn zuno_tools::task::ProviderFacts>,
    /// Exact configured/native agent roster the child host can resolve.
    pub(crate) targets: zuno_tools::task::DelegationTargets,
    /// Per-agent model choices resolved from the same catalog entries.
    pub(crate) agent_models: Vec<(String, zuno_agent::model_policy::ModelChoice)>,
    /// The parent session's model, the precedence ladder's floor.
    pub(crate) session_model: zuno_agent::model_policy::ModelChoice,
    /// Team-wide preset routes frozen when the parent turn was resolved.
    pub(crate) presets: zuno_agent::model_policy::PresetLibrary,
    /// The hop budget from `subagent_depth`.
    pub(crate) limits: zuno_tools::task::DelegationLimits,
    /// Whether the catalog holds a vision-capable model, which gates one target.
    pub(crate) vision_available: bool,
    /// Exact session-frozen authority for model-facing child model selection.
    pub(crate) subagent_model_policy: zuno_tools::task::SubagentModelPolicy,
}

/// Assemble the registry for `agent` and project it onto `provider_id`/`model_id`.
///
/// # Errors
///
/// Returns a message when the file tools, the shell tool, or the todo store cannot
/// be created for `directory`.
pub(crate) fn assemble(
    directory: &Path,
    worktree: Option<&Path>,
    env: &Env,
    config: &Config,
    selected_profile: &AgentProfile,
    selection: ToolSelection<'_>,
) -> Result<ToolRuntime, String> {
    let selected_agent = selected_profile.definition();
    let search = SearchConfig::from_profile(
        |key| env.value(key).map(str::to_owned),
        config.web_search.as_ref(),
    );
    if search.enabled {
        require_provider(&search).map_err(to_string)?;
    }

    let flags = RegistryFlags {
        exposure: ExposureFlags::from_lookup(|key| env.value(key).map(str::to_owned)),
        search: search.clone(),
        experimental_lsp_tool: false,
        experimental_code_mode: false,
    };
    let harness_tool_names = selection
        .contributions
        .tools()
        .iter()
        .map(|tool| tool.id().to_owned())
        .collect::<BTreeSet<_>>();
    let frozen_mcp = selection
        .mcp_loader
        .as_ref()
        .map(|loader| loader.snapshot());
    let mcp_tool_identities = frozen_mcp
        .iter()
        .flat_map(|snapshot| snapshot.tools.iter())
        .map(|tool| tool.definition().schema_identity())
        .collect::<Vec<_>>();
    let eager_mcp_tool_ids = frozen_mcp
        .iter()
        .flat_map(|snapshot| snapshot.eager_tool_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    let dynamic_tool_names = harness_tool_names
        .iter()
        .cloned()
        .chain(
            frozen_mcp
                .iter()
                .flat_map(|snapshot| snapshot.tools.iter())
                .map(|tool| tool.id().to_owned()),
        )
        .collect::<BTreeSet<_>>();
    let dynamic_tool_ids = dynamic_tool_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let rules = selected_profile.rules_with_extension_tools(&dynamic_tool_ids);

    let file_tools = FileTools::new(directory).map_err(to_string)?;
    let mut builder = ToolRegistryBuilder::new(directory, file_tools, flags)
        .with_builtin_slots(selection.manifest.slots().iter().copied())
        .with_harness_tools(selection.contributions.tools().iter().cloned());
    let scope = SearchScope {
        directory: directory.to_path_buf(),
        worktree: worktree.map_or_else(|| directory.to_path_buf(), Path::to_path_buf),
    };
    let tooling = SearchTooling::deferred(scope);
    let mut sandbox_notice = None;
    let shell = shell_visible(selected_profile, selected_agent, &selection.manifest).then(|| {
        let policy = sandbox_policy(directory, config, selected_profile, &rules)?;
        let requested_mode = policy.mode();
        let resolver = selection
            .sandbox
            .clone()
            .unwrap_or_else(|| Arc::new(SystemSandboxResolver));
        let resolution = resolver
            .resolve(policy, sandbox_backend_request(config))
            .map_err(|error| render_sandbox_error(error, requested_mode))?;
        sandbox_notice = native_notice(&resolution);
        let (backend, execution_policy) = resolution.into_execution();
        zuno_tools::shell::ShellTool::with_sandbox_backend(
            directory,
            config.shell.as_deref(),
            backend,
            execution_policy,
        )
        .map_err(to_string)
        .map(|tool| {
            tool.with_background_executions(Arc::clone(&selection.background_executions))
                .with_output_limits(OutputLimits::from_config(config.tool_output.as_ref()))
        })
    });
    let shell = shell.transpose()?;
    if selection.interaction_policy.allows_question()
        && selection.manifest.contains(BuiltinSlot::Question)
        && let Some(asker) = selection.question.clone()
    {
        builder
            .register_builtin(BuiltinSlot::Question, erase(QuestionTool::new(asker)))
            .map_err(|error| error.to_string())?;
    }
    let Delegation {
        host,
        facts,
        mut targets,
        agent_models,
        session_model,
        presets,
        limits,
        vision_available,
        subagent_model_policy,
    } = selection.delegation;
    if let Some(delegates) = selected_profile.capabilities().delegation_targets() {
        let allowed = delegates
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let unknown = allowed
            .iter()
            .filter(|name| {
                !targets
                    .as_slice()
                    .iter()
                    .any(|target| target.as_str() == **name)
            })
            .copied()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(format!(
                "agents.{}.delegates references unavailable agents: {}",
                selected_agent.name,
                unknown.join(", ")
            ));
        }
        targets = zuno_tools::task::DelegationTargets::new(
            targets
                .as_slice()
                .iter()
                .filter(|target| allowed.contains(target.as_str()))
                .cloned(),
        )
        .map_err(to_string)?;
    }
    let mut task = zuno_tools::task::TaskTool::new(host, facts)
        .with_targets(targets)
        .with_session_model(session_model)
        .with_presets(presets)
        .with_limits(limits)
        .with_vision_available(vision_available)
        .with_subagent_model_policy(subagent_model_policy.clone());
    for (agent, model) in agent_models {
        task = task.with_agent_override(agent, model);
    }
    if selection.manifest.contains(BuiltinSlot::Task) {
        let task_tool = if subagent_model_policy.enabled() {
            erase(task.clone().selectable())
        } else {
            erase(task.clone())
        };
        builder
            .register_builtin(BuiltinSlot::Task, task_tool)
            .map_err(|error| error.to_string())?;
    }
    if !selection.capability.workflows.is_empty() {
        let workflow = zuno_tools::workflow::WorkflowTool::new(
            selection.capability.workflows.clone(),
            task.clone(),
            Arc::clone(&selection.workflows),
        )?;
        builder.register_configured_builtin(erase(workflow));
    }
    if !selection.capability.councils.is_empty() {
        let council = zuno_tools::council::CouncilTool::new(
            selection.capability.councils.clone(),
            task,
            Arc::clone(&selection.councils),
        )?;
        builder.register_configured_builtin(erase(council));
    }
    if selection.manifest.contains(BuiltinSlot::Job) {
        builder
            .register_builtin(
                BuiltinSlot::Job,
                erase(zuno_tools::job::JobTool::new(Arc::clone(
                    &selection.todo_store,
                ))),
            )
            .map_err(|error| error.to_string())?;
        builder.register_configured_builtin(erase(zuno_tools::job_cancel::JobCancelTool::new(
            Arc::clone(&selection.todo_store),
            Arc::clone(&selection.job_controller),
        )));
        builder.register_configured_builtin(erase(
            zuno_tools::job_reconcile::JobReconcileTool::new(Arc::clone(&selection.todo_store)),
        ));
    }
    for (slot, tool) in [
        (
            BuiltinSlot::Invalid,
            erase(zuno_tools::invalid::InvalidTool::new()),
        ),
        (
            BuiltinSlot::Background,
            erase(zuno_tools::BackgroundTool::new(Arc::clone(
                &selection.background_executions,
            ))),
        ),
        (
            BuiltinSlot::Glob,
            erase(zuno_tools::GlobTool::new(tooling.clone())),
        ),
        (BuiltinSlot::Grep, erase(zuno_tools::GrepTool::new(tooling))),
        (
            BuiltinSlot::Fetch,
            erase(zuno_tools::WebFetchTool::with_public_client(Arc::clone(
                &selection.public_http,
            ))),
        ),
        (
            BuiltinSlot::Search,
            erase(zuno_tools::WebSearchTool::with_config(search)),
        ),
        (
            BuiltinSlot::Skill,
            match &selection.skill_catalog {
                Some(catalog) => erase(zuno_tools::SkillTool::with_catalog(Arc::clone(catalog))),
                None => erase(zuno_tools::SkillTool::new(Arc::clone(&selection.skills))),
            },
        ),
    ] {
        if selection.manifest.contains(slot) {
            builder
                .register_builtin(slot, tool)
                .map_err(|error| error.to_string())?;
        }
    }
    if selection.manifest.contains(BuiltinSlot::Shell)
        && let Some(shell) = shell
    {
        builder
            .register_builtin(BuiltinSlot::Shell, Arc::new(shell))
            .map_err(|error| error.to_string())?;
    }
    if let Some(snapshot) = frozen_mcp {
        builder = builder.with_mcp_loader(Arc::new(FrozenMcpToolLoader {
            tools: snapshot.tools,
            eager_tool_ids: snapshot.eager_tool_ids,
        }));
    }

    if let Some(memory) = selection.memory {
        builder.register_configured_builtin(memory);
    }
    if let Some(experience_search) = selection.experience_search {
        builder.register_configured_builtin(experience_search);
    }
    let mut product_tool_names = BTreeSet::new();
    for (instance, product) in config.product_agent.iter().flatten() {
        if !product.is_enabled() {
            continue;
        }
        product.validate(instance)?;
        let tool_name = product.resolved_tool_name();
        if native_tool_name(tool_name, &harness_tool_names) {
            return Err(format!(
                "productAgent.{instance}.toolName `{tool_name}` collides with a native tool"
            ));
        }
        if !product_tool_names.insert(tool_name.to_owned()) {
            return Err(format!(
                "enabled product-agent instances must have distinct toolName values; \
                 `{tool_name}` is registered more than once"
            ));
        }
        builder.register_configured_builtin(erase(
            zuno_tools::product_agent::ProductAgentTool::new(
                tool_name,
                instance,
                zuno_tools::product_agent::product_id(product.kind),
                Arc::clone(&selection.product_agents),
            ),
        ));
    }
    for tool in zuno_goal::goal_tools(Arc::clone(&selection.goal_store)) {
        builder.register_configured_builtin(tool);
    }
    // Registered beside the goal tools but not part of `goal_tools`, because a
    // capability claim is not a goal operation: it records what the session believes a
    // provider or model can do, and why it believes it. The goal gate then refuses to
    // complete over a belief nobody verified. The `bedrock-model-capability-review`
    // Skill declares this tool as required, so it stays invisible wherever the tool is
    // not registered.
    builder.register_configured_builtin(erase(zuno_goal::CapabilityClaimTool::new(Arc::clone(
        &selection.goal_store,
    ))));
    if selection.interaction_policy.allows_goal_request_input() && selection.question.is_some() {
        builder.register_configured_builtin(erase(zuno_goal::GoalRequestInputTool::new(
            Arc::clone(&selection.goal_store),
        )));
    }
    for tool in zuno_tools::work_state_tools_with_observer(
        Arc::clone(&selection.todo_store),
        Arc::clone(&selection.work_observer),
    ) {
        builder.register_configured_builtin(tool);
    }

    let registry = builder.build();
    let mut suppressions = registry
        .diagnostics()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut tools = registry.resolve(ResolveInput::new(
        selection.model_id,
        selection.provider_id,
        &rules,
    ));
    let explicit_tool_allowlist = selected_agent
        .tools
        .as_ref()
        .map(|tools| tools.iter().map(String::as_str).collect::<BTreeSet<_>>());
    if let Some(allowlist) = &explicit_tool_allowlist {
        tools.retain(|tool| allowlist.contains(tool.id()));
    }
    if !selected_profile.capabilities().can_delegate() {
        tools.retain(|tool| tool.ui_intent() != ToolUiIntent::Subagent);
    }
    tools.retain(|tool| match tool.id() {
        zuno_tools::question::WIRE_ID => selection.interaction_policy.allows_question(),
        zuno_goal::REQUEST_GOAL_INPUT_TOOL_ID => {
            selection.interaction_policy.allows_goal_request_input()
        }
        _ => true,
    });
    if let Some(authority) = selection.tool_authority.as_deref() {
        tools.retain(|tool| {
            let identity = tool.definition().schema_identity();
            authority.iter().any(|expected| expected == &identity)
        });
    }
    let mut deferred_tool_ids = if selection.tool_authority.is_some() {
        Vec::new()
    } else {
        tools
            .iter()
            .filter(|tool| {
                let identity = tool.definition().schema_identity();
                mcp_tool_identities
                    .iter()
                    .any(|candidate| candidate == &identity)
                    && explicit_tool_allowlist
                        .as_ref()
                        .is_none_or(|allowlist| !allowlist.contains(tool.id()))
                    && !eager_mcp_tool_ids.contains(tool.id())
            })
            .map(|tool| tool.id().to_owned())
            .collect::<Vec<_>>()
    };
    if !deferred_tool_ids.is_empty()
        && tools
            .iter()
            .any(|tool| tool.id() == zuno_engine::dispatch::TOOL_SEARCH_ID)
    {
        deferred_tool_ids.clear();
        suppressions.push(format!(
            "registered tool `{}` conflicts with Zuno's progressive-discovery tool; \
             connected tool schemas remain eagerly visible for this turn",
            zuno_engine::dispatch::TOOL_SEARCH_ID
        ));
    }
    Ok(ToolRuntime {
        tools,
        deferred_tool_ids,
        rules,
        suppressions,
        sandbox_notice,
    })
}

pub(crate) fn sandbox_unavailable_action(config: &Config) -> SandboxUnavailableAction {
    match config.sandbox_on_unavailable() {
        ConfigSandboxUnavailableAction::Deny => SandboxUnavailableAction::Deny,
        ConfigSandboxUnavailableAction::RunUnconfined => SandboxUnavailableAction::RunUnconfined,
    }
}

pub(crate) fn sandbox_backend_selection(config: &Config) -> SandboxBackendSelection {
    match config.sandbox_backend() {
        ConfigSandboxBackendSelection::Auto => SandboxBackendSelection::Auto,
        ConfigSandboxBackendSelection::Native => SandboxBackendSelection::Native,
    }
}

/// The trusted resolver inputs a composition hands the sandbox resolver.
///
/// Both come from `config`, which discovery has already narrowed to what trusted
/// layers may say: a project layer cannot select `native` or `run-unconfined`.
pub(crate) fn sandbox_backend_request(config: &Config) -> SandboxBackendRequest {
    SandboxBackendRequest::new(
        sandbox_unavailable_action(config),
        sandbox_backend_selection(config),
    )
}

/// The durable notice for a Shell that runs natively under a confined request.
///
/// Two resolutions produce one: the trusted `run-unconfined` fallback after an
/// eligible discovery failure, and the explicit `sandbox.backend: native` selection.
/// Each names its own cause first, then says the same thing about what is and is
/// not enforced, because the model and the user read this before trusting a
/// command's isolation. A confined resolution and an explicit `danger-full-access`
/// request carry no notice: the first is enforced, the second was asked for by name.
fn native_notice(resolution: &SandboxResolution) -> Option<String> {
    let requested = resolution.requested_policy();
    let effective = resolution.execution_policy();
    let opening = match resolution.kind() {
        SandboxResolutionKind::UnavailableFallback => {
            let reason = resolution
                .fallback_reason()
                .expect("fallback resolution has a typed reason");
            format!(
                "The requested OS sandbox is unavailable ({code}: {reason}).",
                code = reason.code()
            )
        }
        SandboxResolutionKind::TrustedNative => {
            "The native Shell backend was selected explicitly (sandbox.backend: native).".to_owned()
        }
        SandboxResolutionKind::Confined
        | SandboxResolutionKind::ExplicitNative
        | SandboxResolutionKind::Legacy => return None,
    };
    Some(format!(
        "{opening} Shell commands are running without OS isolation using the Zuno process \
         user's host authority. Requested authority: mode={requested_mode}, \
         network={requested_network}. Effective authority: mode={effective_mode}, \
         network={effective_network}. The requested `{requested_mode}` authority is recorded \
         but not OS-enforced: its write restrictions, network denial, writable-root limits, \
         and protected paths cannot be enforced by an OS sandbox in this state. Permission mode \
         `{permission_mode}`, permission rules, approvals, catastrophic-command refusals, \
         timeouts, and cancellation still apply. Do not describe shell execution as sandboxed.",
        requested_mode = requested.mode().as_str(),
        requested_network = requested.network().as_str(),
        effective_mode = effective.mode().as_str(),
        effective_network = effective.network().as_str(),
        permission_mode = effective.approval_mode(),
    ))
}

/// Render a refused sandbox resolution for the surface that opened this composition.
///
/// Every cause keeps its `zuno-sandbox` rendering except an unsupported platform: that
/// one used to be a bare `OS sandbox is not implemented for platform` with nothing to
/// do about it, and it is the whole story on macOS and Windows. The typed code, the
/// deployment report and `zuno debug sandbox` are untouched; only this text is.
fn render_sandbox_error(error: SandboxError, requested_mode: SandboxMode) -> String {
    match error {
        SandboxError::UnsupportedPlatform(platform) => {
            unsupported_platform_refusal(&platform, requested_mode)
        }
        other => other.to_string(),
    }
}

/// The refusal a user reads when this platform has no confined sandbox backend.
///
/// Names the platform, says whether the trusted fallback would apply to this request,
/// and lists every remedy with the layer that may set it. A write-capable request may
/// take the `run-unconfined` fallback; a read-only request never falls back, and for
/// it the remedy is the explicit trusted `sandbox.backend: native` selection, which
/// keeps the permission mode and records the read-only contract as unenforced. None
/// of the remedies is confinement, and the text says so rather than letting one read
/// like a fix.
#[must_use]
pub(crate) fn unsupported_platform_refusal(platform: &str, requested_mode: SandboxMode) -> String {
    let opening = format!(
        "OS sandbox is not implemented for platform `{platform}`: {platform} has no confined \
         sandbox backend, so the Shell tool cannot be registered under the requested \
         `{requested}` authority.",
        requested = requested_mode.as_str(),
    );
    if requested_mode == SandboxMode::ReadOnly {
        return format!(
            "{opening} A read-only request never falls back: `zuno --sandbox-on-unavailable \
             run-unconfined`, `ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`, and a trusted \
             `\"sandbox\": {{\"onUnavailable\": \"run-unconfined\"}}` do not apply to it. To run \
             this Agent's Shell natively while keeping your permission mode, select the native \
             backend explicitly: `zuno --sandbox-backend native`, `ZUNO_SANDBOX_BACKEND=native`, \
             or `\"sandbox\": {{\"backend\": \"native\"}}` in a trusted global, managed, \
             environment, or CLI configuration layer (a project layer cannot select it). The \
             requested `read-only` authority is then recorded but not OS-enforced: the Agent's \
             tool contract, your permission rules, and the Shell risk gate are what remain. \
             That is not confinement: commands run with the Zuno process user's host authority."
        );
    }
    format!(
        "{opening} The request is write-capable, so the trusted `run-unconfined` fallback \
         applies: Shell would run natively with the Zuno process user's host authority while \
         your permission mode is kept. To continue that way, pass `zuno \
         --sandbox-on-unavailable run-unconfined`, set \
         `ZUNO_SANDBOX_ON_UNAVAILABLE=run-unconfined`, or set `\"sandbox\": \
         {{\"onUnavailable\": \"run-unconfined\"}}` in a trusted global, managed, environment, or \
         CLI configuration layer (a project layer cannot enable it). `zuno --sandbox-backend \
         native` (or `ZUNO_SANDBOX_BACKEND=native`, or a trusted `\"sandbox\": {{\"backend\": \
         \"native\"}}`) selects the native backend for every Agent of this process, read-only \
         ones included, with your permission mode kept; `zuno --sandbox danger-full-access` \
         runs natively as well and additionally makes the effective permission mode \
         `allow_all`. None of these is confinement."
    )
}

/// The situation an interactive TUI start prints before asking its one question.
///
/// Made for every request the host cannot confine, read-only included: accepting
/// selects the native backend for this process, which is the one remedy that applies
/// to both. The text says what a read-only contract becomes under it.
#[must_use]
pub(crate) fn native_execution_offer(
    platform: &str,
    requested_mode: SandboxMode,
    permission_mode: &str,
) -> String {
    format!(
        "OS sandbox is not implemented for platform `{platform}`: {platform} has no confined \
         sandbox backend. The requested `{requested}` authority cannot be confined here, so \
         this session can instead run Shell natively with the Zuno process user's host \
         authority under permission mode `{permission_mode}`; that is not confinement, and a \
         read-only Agent's contract then remains a tool and permission boundary, not an OS \
         boundary. Accepting selects the native backend (`sandbox.backend: native`) for every \
         Agent this process composes. Headless runs choose this with `zuno --sandbox-backend \
         native` or `ZUNO_SANDBOX_BACKEND=native`; `zuno --sandbox danger-full-access` runs \
         natively as well and additionally makes the effective permission mode `allow_all`.",
        requested = requested_mode.as_str(),
    )
}

/// Typed reason a composition would refuse to register Shell on this host.
///
/// Produced by [`sandbox_preflight`] before a host is opened, so a surface can decide
/// what to do about an unsupported platform from data rather than from the rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsupportedPlatformRefusal {
    pub(crate) platform: String,
    pub(crate) requested_mode: SandboxMode,
}

/// What a surface does about an [`UnsupportedPlatformRefusal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnsupportedPlatformDecision {
    /// Nothing is pending; compose as usual.
    Proceed,
    /// Ask the user, once, whether this process may run natively under this request.
    OfferNativeExecution {
        platform: String,
        requested_mode: SandboxMode,
    },
    /// Refuse with the actionable text; nothing may prompt.
    Refuse { message: String },
}

/// What trusted layers already chose about native execution, read as the `Option`s
/// they are: `None` is "nobody chose", the only state an interactive start may ask
/// about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ConfiguredNativeChoices {
    pub(crate) on_unavailable: Option<ConfigSandboxUnavailableAction>,
    pub(crate) backend: Option<ConfigSandboxBackendSelection>,
}

impl ConfiguredNativeChoices {
    pub(crate) fn from_config(config: &Config) -> Self {
        let sandbox = config.sandbox.as_ref();
        Self {
            on_unavailable: sandbox.and_then(|sandbox| sandbox.on_unavailable),
            backend: sandbox.and_then(|sandbox| sandbox.backend),
        }
    }

    fn any(self) -> bool {
        self.on_unavailable.is_some() || self.backend.is_some()
    }
}

/// Decide what an unsupported platform means for the surface about to compose.
///
/// The offer is made for every request the host cannot confine, read-only included,
/// because acceptance selects the native backend and that remedy applies to both.
/// Every other guard still holds: nobody configured `sandbox.onUnavailable` or
/// `sandbox.backend` (an explicit `deny` or `auto` from any layer is honoured, and an
/// explicit `run-unconfined` or `native` that could resolve would already have done
/// so), and the surface can actually ask. Everything else refuses with the same text
/// the headless surfaces print.
#[must_use]
pub(crate) fn decide_unsupported_platform(
    refusal: Option<&UnsupportedPlatformRefusal>,
    configured: ConfiguredNativeChoices,
    interactive: bool,
) -> UnsupportedPlatformDecision {
    let Some(refusal) = refusal else {
        return UnsupportedPlatformDecision::Proceed;
    };
    let message = unsupported_platform_refusal(&refusal.platform, refusal.requested_mode);
    if configured.any() || !interactive {
        return UnsupportedPlatformDecision::Refuse { message };
    }
    UnsupportedPlatformDecision::OfferNativeExecution {
        platform: refusal.platform.clone(),
        requested_mode: refusal.requested_mode,
    }
}

/// The discovery a production composition performs, without keeping the backend.
///
/// On Linux this is the same cached bubblewrap discovery the resolver uses, so a
/// composition that follows a preflight does not probe the host twice.
pub(crate) fn system_sandbox_probe(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    zuno_sandbox::system_backend(policy.workspace(), policy.mode()).map(drop)
}

/// Whether assembling `selected_profile` would refuse Shell for want of a platform backend.
///
/// Runs ahead of [`assemble`] with the same policy, backend request and Shell
/// visibility, so the TUI can ask before raw mode and an agent switch can keep its
/// host. `probe` stands in for backend discovery so a Linux test can act as a Windows
/// host. Every other outcome is `None` and reaches assembly unchanged: Shell not
/// visible, an unbuildable policy, a different failure, an explicit `danger-full-access`
/// request, a trusted `native` backend selection (which never discovers), or a trusted
/// fallback that would resolve.
pub(crate) fn sandbox_preflight(
    directory: &Path,
    config: &Config,
    selected_profile: &AgentProfile,
    manifest: &zuno_harness::ToolManifest,
    probe: &dyn Fn(&SandboxPolicy) -> Result<(), SandboxError>,
) -> Option<UnsupportedPlatformRefusal> {
    if !shell_visible(selected_profile, selected_profile.definition(), manifest) {
        return None;
    }
    let policy = sandbox_policy(
        directory,
        config,
        selected_profile,
        selected_profile.capabilities().rules(),
    )
    .ok()?;
    let requested_mode = policy.mode();
    if requested_mode == SandboxMode::DangerFullAccess
        || sandbox_backend_selection(config) == SandboxBackendSelection::Native
    {
        return None;
    }
    let fallback_would_resolve = requested_mode == SandboxMode::WorkspaceWrite
        && sandbox_unavailable_action(config) == SandboxUnavailableAction::RunUnconfined;
    match probe(&policy) {
        Err(SandboxError::UnsupportedPlatform(platform)) if !fallback_would_resolve => {
            Some(UnsupportedPlatformRefusal {
                platform,
                requested_mode,
            })
        }
        Ok(()) | Err(_) => None,
    }
}

fn shell_visible(
    selected_profile: &AgentProfile,
    selected_agent: &zuno_catalog::agent::Agent,
    manifest: &zuno_harness::ToolManifest,
) -> bool {
    manifest.contains(BuiltinSlot::Shell)
        && selected_profile.capabilities().tool_available("shell")
        && selected_agent
            .tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|tool| tool == "shell"))
}

pub(crate) fn sandbox_policy(
    directory: &Path,
    config: &Config,
    selected_profile: &AgentProfile,
    rules: &[Rule],
) -> Result<SandboxPolicy, String> {
    let configured_mode = config.sandbox_mode();
    let mode = match selected_profile.capabilities().shell_filesystem_access() {
        ShellFilesystemAccess::ReadOnly => SandboxMode::ReadOnly,
        ShellFilesystemAccess::WorkspaceWrite => match configured_mode {
            ConfigSandboxMode::ReadOnly => SandboxMode::ReadOnly,
            ConfigSandboxMode::WorkspaceWrite => SandboxMode::WorkspaceWrite,
            ConfigSandboxMode::DangerFullAccess => SandboxMode::DangerFullAccess,
        },
    };
    let network = if mode == SandboxMode::DangerFullAccess {
        NetworkAccess::Allowed
    } else {
        match config
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.network)
            .unwrap_or(SandboxNetworkMode::Deny)
        {
            SandboxNetworkMode::Deny => NetworkAccess::Denied,
            SandboxNetworkMode::Allow => NetworkAccess::Allowed,
        }
    };
    let mut policy = SandboxPolicy::new(directory, mode, network).map_err(to_string)?;
    if let Some(sandbox) = &config.sandbox {
        if mode == SandboxMode::WorkspaceWrite {
            policy = policy
                .with_writable_roots(
                    sandbox
                        .writable_roots
                        .iter()
                        .flatten()
                        .map(|path| resolve_sandbox_path(directory, path)),
                )
                .map_err(to_string)?;
        }
        policy = policy
            .with_protected_paths(
                sandbox
                    .protected_paths
                    .iter()
                    .flatten()
                    .map(|path| resolve_sandbox_path(directory, path)),
            )
            .map_err(to_string)?;
    }
    let mode = match config.effective_permission_mode() {
        PermissionMode::Standard => "standard",
        PermissionMode::Strict => "strict",
        PermissionMode::AllowAll => "allow_all",
    };
    let policy_sha256 =
        sha256_json(&serde_json::to_value(rules).map_err(|error| error.to_string())?);
    Ok(policy.with_approval_context(mode, policy_sha256))
}

fn resolve_sandbox_path(directory: &Path, configured: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(configured);
    if path.is_absolute() {
        path
    } else {
        directory.join(path)
    }
}

fn native_tool_name(name: &str, harness_tool_names: &BTreeSet<String>) -> bool {
    zuno_tools::registry::BUILTIN_ORDER
        .iter()
        .any(|slot| slot.wire_id() == name)
        || [
            zuno_tools::JOB_CANCEL_WIRE_ID,
            zuno_tools::JOB_RECONCILE_WIRE_ID,
            zuno_tools::memory::MEMORY_TOOL_ID,
            zuno_tools::EXPERIENCE_SEARCH_WIRE_ID,
            zuno_goal::GET_GOAL_TOOL_ID,
            zuno_goal::CREATE_GOAL_TOOL_ID,
            zuno_goal::UPDATE_GOAL_TOOL_ID,
            zuno_tools::PLAN_GET_TOOL_ID,
            zuno_tools::PLAN_UPDATE_TOOL_ID,
            zuno_tools::TODO_GET_TOOL_ID,
            zuno_tools::TODO_UPDATE_TOOL_ID,
            zuno_tools::WORKFLOW_WIRE_ID,
            zuno_tools::COUNCIL_WIRE_ID,
            zuno_continuity::HISTORY_TOOL_ID,
            zuno_continuity::NOTES_TOOL_ID,
        ]
        .contains(&name)
        || harness_tool_names.contains(name)
}

/// The approval collaborator for a surface with no user attached.
///
/// Every `ask` becomes [`ToolError::Denied`] carrying the permission key and the
/// resource, so the refusal names the rule that would authorize it rather than
/// leaving the operator to guess which of the tool's arguments was gated.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HeadlessApproval;

#[async_trait]
impl PermissionAsker for HeadlessApproval {
    async fn ask(
        &self,
        _origin: PermissionOrigin<'_>,
        tool: &str,
        ask: PermissionAsk,
    ) -> Result<(), ToolError> {
        let patterns = ask.patterns.join(", ");
        if ask.manual {
            eprintln!(
                "denied `{tool}`: strict authorization requires a fresh human approval for \
                 {patterns}, and this non-interactive run has nobody to ask"
            );
        } else {
            eprintln!(
                "denied `{tool}`: permission `{}` resolves to ask for {patterns}, and this \
                 non-interactive run has nobody to ask; add `\"permission\": {{\"{}\": \
                 {{\"{patterns}\": \"allow\"}}}}` to your configuration to authorize it",
                ask.permission,
                permission_key(tool),
            );
        }
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_agent_names_reserve_every_native_and_harness_tool() {
        let harness = BTreeSet::from(["extension_tool".to_owned()]);
        for name in [
            "task",
            "job_cancel",
            "job_reconcile",
            "memory_propose",
            "goal_get",
            "goal_propose",
            "goal_update",
            "council_run",
            "history",
            "notes",
            "extension_tool",
        ] {
            assert!(native_tool_name(name, &harness), "{name}");
        }
        assert!(!native_tool_name("subagent_codex", &harness));
    }
}
