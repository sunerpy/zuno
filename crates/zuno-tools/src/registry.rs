//! Ordered tool assembly and the final model-visible registry projection.
//!
//! The registry keeps loading order separate from visibility. Built-ins are first,
//! followed by native harness contributions and MCP tools. A later source replaces
//! an earlier same-named tool in place without sending duplicate names to a
//! provider. Model and provider gates run over that assembled sequence, and
//! permission hiding runs last.

use crate::FileTools;
use crate::batch::ExecuteTool;
use crate::exposure::{ExposureFlags, exposure_predicate};
use crate::websearch::gating::{SearchConfig, web_search_enabled};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use zuno_permission::Rule;
use zuno_permission::visibility::{permission_key, retain_visible_tools};
use zuno_tool::{PermissionAsk, Tool, ToolContext, ToolOutput, ToolOutputStore, erase};

/// The wire-level tool object stored by extension seams.
pub type CustomTool = Arc<dyn Tool>;

/// A tool's assembly source, in increasing upstream precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Builtin,
    Harness,
    Mcp,
}

/// Tool sources in assembly order, from lowest to highest precedence.
///
/// A later source replaces an earlier same-named tool in place.
pub const TOOL_SOURCE_PRECEDENCE: [ToolSource; 3] =
    [ToolSource::Builtin, ToolSource::Harness, ToolSource::Mcp];

impl std::fmt::Display for ToolSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Builtin => "built-in",
            Self::Harness => "harness",
            Self::Mcp => "MCP",
        })
    }
}

/// One observable same-name replacement made during registry assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSuppressionDiagnostic {
    pub tool: String,
    pub suppressed_source: ToolSource,
    pub winning_source: ToolSource,
}

impl std::fmt::Display for ToolSuppressionDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tool `{}` from {} suppressed by same-named tool from {}",
            self.tool, self.suppressed_source, self.winning_source
        )
    }
}

/// Built-in positions from `packages/opencode/src/tool/registry.ts:224-247`.
///
/// The enum names follow upstream's internal registry keys. [`Self::wire_id`]
/// records the provider-facing id where the two differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuiltinSlot {
    Invalid,
    Question,
    Shell,
    Background,
    Read,
    Glob,
    Grep,
    Edit,
    Write,
    Task,
    Job,
    Fetch,
    Todo,
    Search,
    Skill,
    Patch,
    Execute,
    Lsp,
    Plan,
}

impl BuiltinSlot {
    /// The id the model calls for this position.
    #[must_use]
    pub const fn wire_id(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Question => "question",
            Self::Shell => "bash",
            Self::Background => "bg",
            Self::Read => "read",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Task => "task",
            Self::Job => "job",
            Self::Fetch => "webfetch",
            Self::Todo => "todowrite",
            Self::Search => "web_search",
            Self::Skill => "skill",
            Self::Patch => "apply_patch",
            Self::Execute => "execute",
            Self::Lsp => "lsp",
            Self::Plan => "plan_exit",
        }
    }

    fn exposed_by_flags(self, flags: &RegistryFlags) -> bool {
        if let Some(predicate) = exposure_predicate(self.wire_id())
            && !predicate(&flags.exposure)
        {
            return false;
        }

        match self {
            Self::Execute => flags.experimental_code_mode,
            Self::Lsp => flags.experimental_lsp_tool,
            _ => true,
        }
    }
}

/// The exact built-in order used before custom and MCP tools are appended.
pub const BUILTIN_ORDER: [BuiltinSlot; 19] = [
    BuiltinSlot::Invalid,
    BuiltinSlot::Question,
    BuiltinSlot::Shell,
    BuiltinSlot::Background,
    BuiltinSlot::Read,
    BuiltinSlot::Glob,
    BuiltinSlot::Grep,
    BuiltinSlot::Edit,
    BuiltinSlot::Write,
    BuiltinSlot::Task,
    BuiltinSlot::Job,
    BuiltinSlot::Fetch,
    BuiltinSlot::Todo,
    BuiltinSlot::Search,
    BuiltinSlot::Skill,
    BuiltinSlot::Patch,
    BuiltinSlot::Execute,
    BuiltinSlot::Lsp,
    BuiltinSlot::Plan,
];

/// Process-wide flags consulted while the registry is assembled and resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryFlags {
    /// Client and plan-mode gates shared with the conditional built-ins.
    pub exposure: ExposureFlags,
    /// Provider flags shared with `websearch` execution.
    pub search: SearchConfig,
    /// Whether the experimental LSP tool is registered.
    pub experimental_lsp_tool: bool,
    /// Whether an available code-mode tool is registered.
    pub experimental_code_mode: bool,
}

/// MCP tools supplied by the wave-8 MCP host.
pub trait McpToolLoader: Send + Sync {
    /// Return the connected servers' tools in host-defined order.
    fn tools(&self) -> Vec<CustomTool>;
}

/// The default until MCP tool discovery is wired.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopMcpToolLoader;

impl McpToolLoader for NoopMcpToolLoader {
    fn tools(&self) -> Vec<CustomTool> {
        Vec::new()
    }
}

/// A registry construction error that points at the mismatched built-in slot.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    /// One slot may have only one implementation.
    #[error("built-in slot {slot:?} was registered more than once")]
    DuplicateBuiltin { slot: BuiltinSlot },
    /// Slot validation prevents an internal key/wire-id mix-up from shipping.
    #[error("built-in slot {slot:?} expects tool id {expected}, got {actual}")]
    WrongBuiltinId {
        slot: BuiltinSlot,
        expected: &'static str,
        actual: String,
    },
}

/// Incremental assembly for hosts whose concrete tools live in different crates.
pub struct ToolRegistryBuilder {
    directory: PathBuf,
    flags: RegistryFlags,
    file_tools: FileTools,
    enabled_builtins: Option<HashSet<BuiltinSlot>>,
    builtins: BTreeMap<BuiltinSlot, Arc<dyn Tool>>,
    configured_builtins: Vec<Arc<dyn Tool>>,
    harness_tools: Vec<CustomTool>,
    mcp_loader: Arc<dyn McpToolLoader>,
}

impl ToolRegistryBuilder {
    /// Start with the four file implementations that share one runtime.
    ///
    /// Requiring [`FileTools`] here makes the model-family decision reuse
    /// [`FileTools::exposed_for_model`] instead of copying its substring rule.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>, file_tools: FileTools, flags: RegistryFlags) -> Self {
        let mut builtins = BTreeMap::new();
        builtins.insert(BuiltinSlot::Read, Arc::clone(&file_tools.read));
        builtins.insert(BuiltinSlot::Edit, Arc::clone(&file_tools.edit));
        builtins.insert(BuiltinSlot::Write, Arc::clone(&file_tools.write));
        builtins.insert(BuiltinSlot::Patch, Arc::clone(&file_tools.apply_patch));

        Self {
            directory: directory.into(),
            flags,
            file_tools,
            enabled_builtins: None,
            builtins,
            configured_builtins: Vec::new(),
            harness_tools: Vec::new(),
            mcp_loader: Arc::new(NoopMcpToolLoader),
        }
    }

    /// Restrict built-ins to the ordered slots selected by a harness profile.
    #[must_use]
    pub fn with_builtin_slots(mut self, slots: impl IntoIterator<Item = BuiltinSlot>) -> Self {
        self.enabled_builtins = Some(slots.into_iter().collect());
        self
    }

    /// Register one implementation at its upstream position.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::WrongBuiltinId`] when the tool's wire id does not
    /// belong to `slot`, or [`RegistryError::DuplicateBuiltin`] when occupied.
    pub fn register_builtin(
        &mut self,
        slot: BuiltinSlot,
        tool: Arc<dyn Tool>,
    ) -> Result<&mut Self, RegistryError> {
        let expected = slot.wire_id();
        if tool.id() != expected {
            return Err(RegistryError::WrongBuiltinId {
                slot,
                expected,
                actual: tool.id().to_owned(),
            });
        }
        if self.builtins.contains_key(&slot) {
            return Err(RegistryError::DuplicateBuiltin { slot });
        }
        self.builtins.insert(slot, tool);
        Ok(self)
    }

    /// Register a configured built-in that has no fixed upstream slot.
    ///
    /// These tools are assembled after the slotted built-ins and before harness
    /// and MCP contributions.
    pub fn register_configured_builtin(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        self.configured_builtins.push(tool);
        self
    }

    /// Install native tools contributed by the active harness profile.
    #[must_use]
    pub fn with_harness_tools(mut self, tools: impl IntoIterator<Item = CustomTool>) -> Self {
        self.harness_tools = tools.into_iter().collect();
        self
    }

    /// Install the wave-8 MCP-tool seam.
    #[must_use]
    pub fn with_mcp_loader(mut self, loader: Arc<dyn McpToolLoader>) -> Self {
        self.mcp_loader = loader;
        self
    }

    /// Load every source once and freeze the native source order.
    #[must_use]
    pub fn build(self) -> ToolRegistry {
        let output_store = ToolOutputStore::new(
            self.directory
                .join(zuno_paths::PROJECT_DIRECTORY)
                .join(zuno_paths::TOOL_OUTPUT_DIRECTORY),
        );
        let mut diagnostics = Vec::new();
        let core = Arc::new_cyclic(|weak| {
            let mut sourced_tools = Vec::new();
            for source in TOOL_SOURCE_PRECEDENCE {
                match source {
                    ToolSource::Builtin => {
                        for slot in BUILTIN_ORDER {
                            if self
                                .enabled_builtins
                                .as_ref()
                                .is_some_and(|enabled| !enabled.contains(&slot))
                            {
                                continue;
                            }
                            if !slot.exposed_by_flags(&self.flags) {
                                continue;
                            }
                            if let Some(tool) = self.builtins.get(&slot) {
                                insert_tool(
                                    &mut sourced_tools,
                                    &mut diagnostics,
                                    Arc::clone(tool),
                                    source,
                                );
                            } else if slot == BuiltinSlot::Execute {
                                insert_tool(
                                    &mut sourced_tools,
                                    &mut diagnostics,
                                    erase(ExecuteTool::new(
                                        RegistryHandle::new(weak.clone()),
                                        output_store.clone(),
                                    )),
                                    source,
                                );
                            }
                        }
                        insert_tools(
                            &mut sourced_tools,
                            &mut diagnostics,
                            self.configured_builtins.iter().cloned(),
                            source,
                        );
                    }
                    ToolSource::Harness => insert_tools(
                        &mut sourced_tools,
                        &mut diagnostics,
                        self.harness_tools.iter().cloned(),
                        source,
                    ),
                    ToolSource::Mcp => insert_tools(
                        &mut sourced_tools,
                        &mut diagnostics,
                        self.mcp_loader.tools(),
                        source,
                    ),
                }
            }
            let tools = sourced_tools
                .into_iter()
                .map(|(tool, _source)| tool)
                .collect();
            RegistryCore { tools }
        });

        ToolRegistry {
            core,
            file_tools: self.file_tools,
            flags: self.flags,
            diagnostics,
        }
    }
}

fn insert_tools(
    tools: &mut Vec<(Arc<dyn Tool>, ToolSource)>,
    diagnostics: &mut Vec<ToolSuppressionDiagnostic>,
    incoming: impl IntoIterator<Item = Arc<dyn Tool>>,
    source: ToolSource,
) {
    for tool in incoming {
        insert_tool(tools, diagnostics, tool, source);
    }
}

fn insert_tool(
    tools: &mut Vec<(Arc<dyn Tool>, ToolSource)>,
    diagnostics: &mut Vec<ToolSuppressionDiagnostic>,
    tool: Arc<dyn Tool>,
    source: ToolSource,
) {
    if let Some((existing, existing_source)) = tools
        .iter_mut()
        .find(|(existing, _source)| existing.id() == tool.id())
    {
        let diagnostic = ToolSuppressionDiagnostic {
            tool: tool.id().to_owned(),
            suppressed_source: *existing_source,
            winning_source: source,
        };
        // A shadowed built-in was a real defect that a quiet log hid, so this stays
        // observable two ways: durably in the log, and as structured data the host is
        // expected to surface. What it must not do is write to the terminal itself —
        // a library that owns stdout cannot be hosted by a TUI, and this printed onto
        // the primary screen, where the lines lay hidden behind the alternate screen
        // until the user exited and inherited them without context.
        tracing::warn!(
            tool = %diagnostic.tool,
            suppressed_source = %diagnostic.suppressed_source,
            winning_source = %diagnostic.winning_source,
            "tool suppressed by a same-named tool from a later source"
        );
        diagnostics.push(diagnostic);
        *existing = tool;
        *existing_source = source;
    } else {
        tools.push((tool, source));
    }
}

/// Per-turn inputs that change the model-visible projection.
#[derive(Debug, Clone, Copy)]
pub struct ResolveInput<'a> {
    /// The model id, without the provider prefix.
    pub model_id: &'a str,
    /// The model provider id, not the web-search backend.
    pub provider_id: &'a str,
    /// Agent and session rules after their precedence merge.
    pub permissions: &'a [Rule],
    /// The resolved code-mode catalog description, when one exists.
    pub code_mode_description: Option<&'a str>,
}

impl<'a> ResolveInput<'a> {
    /// A turn with no usable code-mode catalog.
    #[must_use]
    pub const fn new(model_id: &'a str, provider_id: &'a str, permissions: &'a [Rule]) -> Self {
        Self {
            model_id,
            provider_id,
            permissions,
            code_mode_description: None,
        }
    }

    /// Record that code mode resolved a non-empty catalog description.
    #[must_use]
    pub const fn with_code_mode_description(mut self, description: &'a str) -> Self {
        self.code_mode_description = Some(description);
        self
    }
}

/// The assembled registry, before and after per-turn filtering.
pub struct ToolRegistry {
    core: Arc<RegistryCore>,
    file_tools: FileTools,
    flags: RegistryFlags,
    diagnostics: Vec<ToolSuppressionDiagnostic>,
}

impl ToolRegistry {
    /// Every loaded tool after process-wide exposure flags, before turn filters.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.core.tools
    }

    /// Same-name replacements performed while all registry sources were assembled.
    #[must_use]
    pub fn diagnostics(&self) -> &[ToolSuppressionDiagnostic] {
        &self.diagnostics
    }

    /// Resolve the model/provider projection, then hide fully denied tools last.
    ///
    /// `registry.ts:286-304` filters by wire id across built-ins and extensions.
    /// The execute-description check is intentionally independent from the code-mode
    /// registration flag: once todo 70 supplies an implementation, both gates must
    /// pass. Until then the second gate is inert because no execute tool is loaded.
    #[must_use]
    pub fn resolve(&self, input: ResolveInput<'_>) -> Vec<Arc<dyn Tool>> {
        let exposed_file_ids: BTreeSet<String> = self
            .file_tools
            .exposed_for_model(input.model_id)
            .into_iter()
            .map(|tool| tool.id().to_owned())
            .collect();
        let model_conditional_file_ids = [
            self.file_tools.edit.id(),
            self.file_tools.write.id(),
            self.file_tools.apply_patch.id(),
        ];

        let mut visible: Vec<Arc<dyn Tool>> = self
            .core
            .tools
            .iter()
            .filter(|tool| {
                let id = tool.id();
                if id == crate::websearch::ID && !web_search_enabled(&self.flags.search) {
                    return false;
                }
                if model_conditional_file_ids.contains(&id) && !exposed_file_ids.contains(id) {
                    return false;
                }
                id != "execute" || input.code_mode_description.is_some()
            })
            .cloned()
            .collect();

        // Oracle: `permission/index.ts:204-219`. This is deliberately the final
        // transformation so the same deny also covers plugin and MCP tools.
        retain_visible_tools(&mut visible, input.permissions, |tool| tool.id());
        visible
    }

    /// The resolved wire ids in provider order.
    #[must_use]
    pub fn resolved_ids(&self, input: ResolveInput<'_>) -> Vec<String> {
        self.resolve(input)
            .into_iter()
            .map(|tool| tool.id().to_owned())
            .collect()
    }

    /// Execute a tool through the registry's shared lookup and permission boundary.
    pub async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, zuno_error::ToolError> {
        self.core.execute(name, arguments, ctx).await
    }
}

struct RegistryCore {
    tools: Vec<Arc<dyn Tool>>,
}

impl RegistryCore {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, zuno_error::ToolError> {
        let resolved = canonical_tool_name(name);
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.id() == resolved)
            .cloned()
            .ok_or_else(|| zuno_error::ToolError::NotFound {
                tool: resolved.to_owned(),
            })?;
        ctx.ask(resolved, PermissionAsk::new(permission_key(resolved), "*"))
            .await?;
        tool.invoke(arguments, ctx).await
    }
}

#[derive(Clone)]
pub(crate) struct RegistryHandle {
    core: Weak<RegistryCore>,
}

impl RegistryHandle {
    fn new(core: Weak<RegistryCore>) -> Self {
        Self { core }
    }

    pub(crate) async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolOutput, zuno_error::ToolError> {
        let core = self
            .core
            .upgrade()
            .ok_or_else(|| zuno_error::ToolError::Failed {
                tool: name.to_owned(),
                source: Box::new(RegistryUnavailable),
            })?;
        core.execute(name, arguments, ctx).await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("tool registry is no longer available")]
struct RegistryUnavailable;

/// Normalize names before registry lookup, including provider namespacing.
#[must_use]
pub(crate) fn canonical_tool_name(name: &str) -> &str {
    let name = name.strip_prefix("functions.").unwrap_or(name);
    match name {
        "communicate" | "task_runner" | "subagent" | "Agent" | "Task" => "task",
        "launch" => "open",
        "shell" | "shell_exec" | "Bash" => "bash",
        "read_file" | "file_read" | "Read" => "read",
        "write_file" | "file_write" | "Write" => "write",
        "edit_file" | "file_edit" | "Edit" => "edit",
        "file_grep" | "Grep" => "grep",
        "skill_manage" | "Skill" => "skill",
        "discover_tools" => "integration_tools",
        "todoread" | "todo_read" | "todo_write" | "todos" | "todo" | "TodoWrite" => "todowrite",
        "WebFetch" => "webfetch",
        "WebSearch" => "web_search",
        "ApplyPatch" => "apply_patch",
        "Question" => "question",
        "PlanExit" => "plan_exit",
        "Lsp" => "lsp",
        "Execute" => "execute",
        "ScheduleWakeup" => "schedule",
        other => other,
    }
}
