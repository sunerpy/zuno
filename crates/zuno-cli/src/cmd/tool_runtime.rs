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
//! [`zuno_tools::registry::BUILTIN_ORDER`] has seventeen positions. This module
//! registers the ones whose implementation needs nothing but the workspace and the
//! database. `question` and `plan_exit` need a live user to answer, `task` needs a
//! child-session host, `skill` and `lsp` have no implementation in `zuno-tools` at
//! all, and `execute` is registered by the builder itself behind an experimental
//! flag. An unregistered slot is simply absent from the assembled vector, so the
//! model is never told about a tool that cannot run.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use zuno_catalog::agent;
use zuno_config::schema::Config;
use zuno_error::ToolError;
use zuno_memory::ScopeLimits;
use zuno_paths::Env;
use zuno_permission::Rule;
use zuno_permission::visibility::permission_key;
use zuno_tool::{PermissionAsk, PermissionAsker, Tool, erase};
use zuno_tools::exposure::ExposureFlags;
use zuno_tools::question::{QuestionAsker, QuestionTool};
use zuno_tools::registry::{
    BuiltinSlot, CustomTool, CustomToolLoader, McpToolLoader, RegistryFlags, ResolveInput,
    ToolRegistryBuilder,
};
use zuno_tools::search_common::{SearchScope, SearchTooling};
use zuno_tools::websearch::gating::SearchConfig;
use zuno_tools::{FileTools, MemoryTool, ScopePaths};

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
    pub(crate) question: Option<Arc<dyn QuestionAsker>>,
    pub(crate) plugin_tools: &'a [Arc<dyn Tool>],
    pub(crate) plugins: Option<Arc<super::plugin_runtime::PluginRuntime>>,
    pub(crate) todo_store: Arc<zuno_db::pool::Pool>,
    pub(crate) goal_store: Arc<zuno_goal::GoalStore>,
    pub(crate) mcp_loader: Option<Arc<dyn McpToolLoader>>,
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
    selection: ToolSelection<'_>,
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
    let tooling = SearchTooling::with_backend(scope, zuno_search::Backend::from_env());
    let mut shell = zuno_tools::shell::ShellTool::new(directory).map_err(to_string)?;
    if let Some(plugins) = selection.plugins.as_ref() {
        let hook: Arc<dyn zuno_tools::shell::ShellEnvHook> = plugins.clone();
        shell = shell.with_env_hook(hook);
    }
    if let Some(asker) = selection.question {
        builder
            .register_builtin(BuiltinSlot::Question, erase(QuestionTool::new(asker)))
            .map_err(|error| error.to_string())?;
    }
    for (slot, tool) in [
        (
            BuiltinSlot::Invalid,
            erase(zuno_tools::invalid::InvalidTool::new()),
        ),
        (BuiltinSlot::Shell, Arc::new(shell) as Arc<dyn Tool>),
        (
            BuiltinSlot::Glob,
            erase(zuno_tools::GlobTool::new(tooling.clone())),
        ),
        (BuiltinSlot::Grep, erase(zuno_tools::GrepTool::new(tooling))),
        (BuiltinSlot::Fetch, erase(zuno_tools::WebFetchTool::new())),
        (
            BuiltinSlot::Todo,
            erase(zuno_tools::todo::TodoWriteTool::new(Arc::new(
                zuno_tools::todo::SqliteTodoStore::new(Arc::clone(&selection.todo_store)),
            ))),
        ),
        (
            BuiltinSlot::Search,
            erase(zuno_tools::WebSearchTool::with_config(
                SearchConfig::from_lookup(|key| env.value(key).map(str::to_owned)),
            )),
        ),
    ] {
        builder
            .register_builtin(slot, tool)
            .map_err(|error| error.to_string())?;
    }
    builder = builder.with_custom_loader(Arc::new(ProductionCustomTools {
        plugin_tools: selection.plugin_tools.to_vec(),
    }));
    if let Some(loader) = selection.mcp_loader {
        builder = builder.with_mcp_loader(loader);
    }

    let memory_root = worktree.unwrap_or(directory);
    if let Some(memory) = configured_memory_tool(memory_root, config) {
        builder.register_configured_builtin(memory);
    }
    for tool in zuno_goal::goal_tools(Arc::clone(&selection.goal_store)) {
        builder.register_configured_builtin(tool);
    }

    let registry = builder.build();
    let suppressions = registry
        .diagnostics()
        .iter()
        .map(ToString::to_string)
        .collect();
    let tools = registry.resolve(ResolveInput::new(
        selection.model_id,
        selection.provider_id,
        &rules,
    ));
    Ok(ToolRuntime {
        tools,
        rules,
        suppressions,
    })
}

struct ProductionCustomTools {
    plugin_tools: Vec<CustomTool>,
}

impl CustomToolLoader for ProductionCustomTools {
    fn config_directory_tools(&self, _directories: &[std::path::PathBuf]) -> Vec<CustomTool> {
        Vec::new()
    }

    fn plugin_tools(&self) -> Vec<CustomTool> {
        self.plugin_tools.clone()
    }
}

fn configured_memory_tool(root: &Path, config: &Config) -> Option<Arc<dyn Tool>> {
    let memory = config.resolved_memory();
    let limits = ScopeLimits::new(memory.global_char_limit, memory.project_char_limit);
    MemoryTool::configured(memory.tool, ScopePaths::discover(root), limits).map(erase)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::from_json_str(Path::new("opencode.json"), text).expect("memory config")
    }

    #[test]
    fn memory_tool_is_present_by_default_and_absent_under_master_switch() {
        let root = Path::new("/tmp/memory-tool-gate");
        let enabled = configured_memory_tool(root, &Config::default());
        let disabled = configured_memory_tool(root, &config(r#"{"memory":false}"#));

        assert_eq!(enabled.as_ref().map(|tool| tool.id()), Some("memory"));
        assert!(disabled.is_none());
    }

    #[test]
    fn component_tool_switch_can_disable_only_model_facing_access() {
        let memory = configured_memory_tool(
            Path::new("/tmp/memory-tool-component-gate"),
            &config(r#"{"memory":{"tool":false}}"#),
        );

        assert!(memory.is_none());
    }
}
