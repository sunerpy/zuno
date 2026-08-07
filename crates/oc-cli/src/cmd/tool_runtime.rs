//! The one place a command turns configuration into an executable tool set.
//!
//! Todo 44 assembled the registry and todo 56 built `run`, and the two were never
//! joined: `run` constructed its dispatcher with an empty tool vector and
//! [`oc_tool::AllowAll`], so no production path could execute a tool and no
//! production path consulted a permission rule. Both surfaces that drive a turn
//! come through here instead, because a second assembly site is how the two
//! diverge — one gaining a tool, or a permission gate, that the other lacks.
//!
//! # Why the approval collaborator refuses rather than prompts
//!
//! [`oc_permission`]'s evaluator resolves `allow` and `deny` itself and only calls
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
//! [`oc_tools::registry::BUILTIN_ORDER`] has seventeen positions. This module
//! registers the ones whose implementation needs nothing but the workspace and the
//! database. `question` and `plan_exit` need a live user to answer, `task` needs a
//! child-session host, `skill` and `lsp` have no implementation in `oc-tools` at
//! all, and `execute` is registered by the builder itself behind an experimental
//! flag. An unregistered slot is simply absent from the assembled vector, so the
//! model is never told about a tool that cannot run.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use oc_catalog::agent;
use oc_config::schema::Config;
use oc_error::ToolError;
use oc_paths::Env;
use oc_permission::Rule;
use oc_permission::visibility::permission_key;
use oc_tool::{PermissionAsk, PermissionAsker, Tool, erase};
use oc_tools::FileTools;
use oc_tools::exposure::ExposureFlags;
use oc_tools::registry::{BuiltinSlot, RegistryFlags, ResolveInput, ToolRegistryBuilder};
use oc_tools::search_common::{SearchScope, SearchTooling};
use oc_tools::websearch::gating::SearchConfig;

/// The executable tools and the ruleset that governs them, for one turn.
pub(crate) struct ToolRuntime {
    /// The model-visible, permission-filtered tools in provider order.
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    /// The merged ruleset the dispatcher re-evaluates before every call.
    pub(crate) rules: Vec<Rule>,
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
    selected_agent: &agent::Agent,
    provider_id: &str,
    model_id: &str,
) -> Result<ToolRuntime, String> {
    let dynamic = super::agent::DynamicRules::resolve(directory, worktree, env, config);
    let rules = super::agent::resolved_rules(selected_agent, config, &dynamic);

    let flags = RegistryFlags {
        exposure: ExposureFlags::from_lookup(|key| env.value(key).map(str::to_owned)),
        search: SearchConfig::from_lookup(|key| env.value(key).map(str::to_owned)),
        experimental_lsp_tool: false,
        experimental_code_mode: false,
    };

    let file_tools = FileTools::new(directory).map_err(to_string)?;
    let mut builder = ToolRegistryBuilder::new(
        directory,
        worktree.map(Path::to_path_buf),
        file_tools,
        flags,
    );
    let scope = SearchScope {
        directory: directory.to_path_buf(),
        worktree: worktree.map_or_else(|| directory.to_path_buf(), Path::to_path_buf),
    };
    let tooling = SearchTooling::with_backend(scope, oc_search::Backend::from_env());
    let shell = oc_tools::shell::ShellTool::new(directory).map_err(to_string)?;
    let todo_store = oc_db::pool::Pool::open_default().map_err(to_string)?;
    for (slot, tool) in [
        (
            BuiltinSlot::Invalid,
            erase(oc_tools::invalid::InvalidTool::new()),
        ),
        (BuiltinSlot::Shell, Arc::new(shell) as Arc<dyn Tool>),
        (
            BuiltinSlot::Glob,
            erase(oc_tools::GlobTool::new(tooling.clone())),
        ),
        (BuiltinSlot::Grep, erase(oc_tools::GrepTool::new(tooling))),
        (BuiltinSlot::Fetch, erase(oc_tools::WebFetchTool::new())),
        (
            BuiltinSlot::Todo,
            erase(oc_tools::todo::TodoWriteTool::new(Arc::new(
                oc_tools::todo::SqliteTodoStore::new(Arc::new(todo_store)),
            ))),
        ),
        (
            BuiltinSlot::Search,
            erase(oc_tools::WebSearchTool::with_config(
                SearchConfig::from_lookup(|key| env.value(key).map(str::to_owned)),
            )),
        ),
    ] {
        builder
            .register_builtin(slot, tool)
            .map_err(|error| error.to_string())?;
    }

    let registry = builder.build();
    let tools = registry.resolve(ResolveInput::new(model_id, provider_id, &rules));
    Ok(ToolRuntime { tools, rules })
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
    async fn ask(&self, tool: &str, ask: PermissionAsk) -> Result<(), ToolError> {
        let patterns = ask.patterns.join(", ");
        eprintln!(
            "denied `{tool}`: permission `{}` resolves to ask for {patterns}, and this \
             non-interactive run has nobody to ask; add `\"permission\": {{\"{}\": \
             {{\"{patterns}\": \"allow\"}}}}` to your configuration to authorize it",
            ask.permission,
            permission_key(tool),
        );
        Err(ToolError::Denied {
            tool: tool.to_owned(),
        })
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}
