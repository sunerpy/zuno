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
use zuno_config::schema::Config;
use zuno_error::ToolError;
use zuno_orchestration::CapabilitySnapshot;
use zuno_paths::Env;
use zuno_permission::Rule;
use zuno_permission::visibility::permission_key;
use zuno_tool::{PermissionAsk, PermissionAsker, PermissionOrigin, Tool, ToolUiIntent, erase};
use zuno_tools::FileTools;
use zuno_tools::exposure::ExposureFlags;
use zuno_tools::question::{QuestionAsker, QuestionTool};
use zuno_tools::registry::{
    BuiltinSlot, McpToolLoader, RegistryFlags, ResolveInput, ToolRegistryBuilder,
};
use zuno_tools::search_common::{SearchScope, SearchTooling};
use zuno_tools::websearch::gating::{SearchConfig, require_provider};

/// The executable tools and the ruleset that governs them, for one turn.
pub(crate) struct ToolRuntime {
    /// The model-visible, permission-filtered tools in provider order.
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// The merged ruleset the dispatcher re-evaluates before every call.
    pub(crate) rules: Vec<Rule>,
    /// Same-name replacements made while assembling, for the host to surface.
    ///
    /// Carried out of assembly rather than printed inside it: a shadowed built-in is
    /// a defect a user must see, but which surface can say so — stderr, a transcript
    /// line — is the host's decision, not the registry's.
    pub(crate) suppressions: Vec<String>,
}

pub(crate) struct ToolSelection<'a> {
    pub(crate) provider_id: &'a str,
    pub(crate) model_id: &'a str,
    pub(crate) manifest: Arc<zuno_harness::ToolManifest>,
    pub(crate) contributions: Arc<zuno_harness::ToolContributions>,
    pub(crate) question: Option<Arc<dyn QuestionAsker>>,
    pub(crate) background_executions: Arc<zuno_pty::BackgroundExecutionService>,
    pub(crate) todo_store: Arc<zuno_db::pool::Pool>,
    pub(crate) goal_store: Arc<zuno_goal::GoalStore>,
    pub(crate) mcp_loader: Option<Arc<dyn McpToolLoader>>,
    pub(crate) skills: Arc<zuno_catalog::skill::Skills>,
    pub(crate) capability: Arc<CapabilitySnapshot>,
    pub(crate) delegation: Delegation,
    pub(crate) product_agents: Arc<dyn zuno_tools::product_agent::ProductAgentHost>,
    pub(crate) workflows: Arc<dyn zuno_tools::workflow::WorkflowHost>,
    pub(crate) councils: Arc<dyn zuno_tools::council::CouncilHost>,
    pub(crate) job_controller: Arc<dyn zuno_tools::job_cancel::JobController>,
    pub(crate) memory: Option<Arc<dyn Tool>>,
}

/// The collaborators `task` needs, which only a surface that can drive a turn has.
///
/// **Deliberately not `Option`.** `task` went unregistered for exactly as long as it
/// took nobody to pass a host, and an optional field turns that back into a runtime
/// choice a test can only catch by looking. Required, the compiler catches it: the
/// sole caller of [`assemble`] is [`super::turn::TurnHost::open_with_runtime_and_mcp`],
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
    let rules = selected_profile.capabilities().rules().to_vec();
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

    let file_tools = FileTools::new(directory).map_err(to_string)?;
    let mut builder = ToolRegistryBuilder::new(directory, file_tools, flags)
        .with_builtin_slots(selection.manifest.slots().iter().copied())
        .with_harness_tools(selection.contributions.tools().iter().cloned());
    let scope = SearchScope {
        directory: directory.to_path_buf(),
        worktree: worktree.map_or_else(|| directory.to_path_buf(), Path::to_path_buf),
    };
    let tooling = SearchTooling::discover(scope).map_err(to_string)?;
    let shell =
        zuno_tools::shell::ShellTool::with_configured_shell(directory, config.shell.as_deref())
            .map_err(to_string)?
            .with_background_executions(Arc::clone(&selection.background_executions));
    if selection.manifest.contains(BuiltinSlot::Question)
        && let Some(asker) = selection.question
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
        .with_vision_available(vision_available);
    for (agent, model) in agent_models {
        task = task.with_agent_override(agent, model);
    }
    if selection.manifest.contains(BuiltinSlot::Task) {
        builder
            .register_builtin(BuiltinSlot::Task, erase(task.clone()))
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
    }
    for (slot, tool) in [
        (
            BuiltinSlot::Invalid,
            erase(zuno_tools::invalid::InvalidTool::new()),
        ),
        (BuiltinSlot::Shell, Arc::new(shell) as Arc<dyn Tool>),
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
        (BuiltinSlot::Fetch, erase(zuno_tools::WebFetchTool::new())),
        (
            BuiltinSlot::Search,
            erase(zuno_tools::WebSearchTool::with_config(search)),
        ),
        (
            BuiltinSlot::Skill,
            erase(zuno_tools::SkillTool::new(Arc::clone(&selection.skills))),
        ),
    ] {
        if selection.manifest.contains(slot) {
            builder
                .register_builtin(slot, tool)
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(loader) = selection.mcp_loader {
        builder = builder.with_mcp_loader(loader);
    }

    if let Some(memory) = selection.memory {
        builder.register_configured_builtin(memory);
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
    for tool in zuno_tools::work_state_tools(Arc::clone(&selection.todo_store)) {
        builder.register_configured_builtin(tool);
    }

    let registry = builder.build();
    let suppressions = registry
        .diagnostics()
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut tools = registry.resolve(ResolveInput::new(
        selection.model_id,
        selection.provider_id,
        &rules,
    ));
    if let Some(allowlist) = &selected_agent.tools {
        let allowlist = allowlist
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        tools.retain(|tool| allowlist.contains(tool.id()));
    }
    if !selected_profile.capabilities().can_delegate() {
        tools.retain(|tool| tool.ui_intent() != ToolUiIntent::Subagent);
    }
    Ok(ToolRuntime {
        tools,
        rules,
        suppressions,
    })
}

fn native_tool_name(name: &str, harness_tool_names: &BTreeSet<String>) -> bool {
    zuno_tools::registry::BUILTIN_ORDER
        .iter()
        .any(|slot| slot.wire_id() == name)
        || [
            zuno_tools::JOB_CANCEL_WIRE_ID,
            zuno_tools::memory::MEMORY_TOOL_ID,
            zuno_goal::GET_GOAL_TOOL_ID,
            zuno_goal::CREATE_GOAL_TOOL_ID,
            zuno_goal::UPDATE_GOAL_TOOL_ID,
            zuno_tools::PLAN_GET_TOOL_ID,
            zuno_tools::PLAN_UPDATE_TOOL_ID,
            zuno_tools::TODO_GET_TOOL_ID,
            zuno_tools::TODO_UPDATE_TOOL_ID,
            zuno_tools::WORKFLOW_WIRE_ID,
            zuno_tools::COUNCIL_WIRE_ID,
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
            "memory_propose",
            "goal_get",
            "goal_propose",
            "goal_update",
            "council_run",
            "extension_tool",
        ] {
            assert!(native_tool_name(name, &harness), "{name}");
        }
        assert!(!native_tool_name("subagent_codex", &harness));
    }
}
