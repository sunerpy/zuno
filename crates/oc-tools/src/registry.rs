//! Ordered tool assembly and the final model-visible registry projection.
//!
//! The registry keeps loading order separate from visibility. Built-ins are first,
//! followed by config-directory exports, plugin exports, and MCP tools. Model and
//! provider gates run over that assembled sequence, and permission hiding runs last.
//! That order matters: an extension can use a built-in wire id, while a blanket deny
//! must also hide tools that arrived from an extension host.

use crate::FileTools;
use crate::exposure::{ExposureFlags, exposure_predicate};
use crate::websearch::gating::{SearchConfig, web_search_enabled};
use oc_permission::Rule;
use oc_permission::visibility::retain_visible_tools;
use oc_tool::Tool;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The wire-level tool object stored by extension seams.
pub type CustomTool = Arc<dyn Tool>;

/// Built-in positions from `packages/opencode/src/tool/registry.ts:224-247`.
///
/// The enum names follow upstream's internal registry keys. [`Self::wire_id`]
/// records the provider-facing id where the two differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuiltinSlot {
    Invalid,
    Question,
    Shell,
    Read,
    Glob,
    Grep,
    Edit,
    Write,
    Task,
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
            Self::Read => "read",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Edit => "edit",
            Self::Write => "write",
            Self::Task => "task",
            Self::Fetch => "webfetch",
            Self::Todo => "todowrite",
            Self::Search => "websearch",
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
pub const BUILTIN_ORDER: [BuiltinSlot; 17] = [
    BuiltinSlot::Invalid,
    BuiltinSlot::Question,
    BuiltinSlot::Shell,
    BuiltinSlot::Read,
    BuiltinSlot::Glob,
    BuiltinSlot::Grep,
    BuiltinSlot::Edit,
    BuiltinSlot::Write,
    BuiltinSlot::Task,
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

/// Config-directory and plugin tools supplied by the wave-9 plugin host.
///
/// The two methods are separate because `registry.ts:178-199` loads config exports
/// before plugin-provided exports. A single combined callback would make that order
/// impossible to preserve once both sources are live.
pub trait CustomToolLoader: Send + Sync {
    /// Load `{tool,tools}/*.{js,ts}` from the resolved config directory chain.
    fn config_directory_tools(&self, directories: &[PathBuf]) -> Vec<CustomTool>;

    /// Load tools exposed by configured plugins.
    fn plugin_tools(&self) -> Vec<CustomTool>;
}

/// The default until the plugin host is wired.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopCustomToolLoader;

impl CustomToolLoader for NoopCustomToolLoader {
    fn config_directory_tools(&self, _directories: &[PathBuf]) -> Vec<CustomTool> {
        Vec::new()
    }

    fn plugin_tools(&self) -> Vec<CustomTool> {
        Vec::new()
    }
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
    worktree: Option<PathBuf>,
    flags: RegistryFlags,
    file_tools: FileTools,
    builtins: BTreeMap<BuiltinSlot, Arc<dyn Tool>>,
    custom_loader: Arc<dyn CustomToolLoader>,
    mcp_loader: Arc<dyn McpToolLoader>,
}

impl ToolRegistryBuilder {
    /// Start with the four file implementations that share one runtime.
    ///
    /// Requiring [`FileTools`] here makes the model-family decision reuse
    /// [`FileTools::exposed_for_model`] instead of copying its substring rule.
    #[must_use]
    pub fn new(
        directory: impl Into<PathBuf>,
        worktree: Option<PathBuf>,
        file_tools: FileTools,
        flags: RegistryFlags,
    ) -> Self {
        let mut builtins = BTreeMap::new();
        builtins.insert(BuiltinSlot::Read, Arc::clone(&file_tools.read));
        builtins.insert(BuiltinSlot::Edit, Arc::clone(&file_tools.edit));
        builtins.insert(BuiltinSlot::Write, Arc::clone(&file_tools.write));
        builtins.insert(BuiltinSlot::Patch, Arc::clone(&file_tools.apply_patch));

        Self {
            directory: directory.into(),
            worktree,
            flags,
            file_tools,
            builtins,
            custom_loader: Arc::new(NoopCustomToolLoader),
            mcp_loader: Arc::new(NoopMcpToolLoader),
        }
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

    /// Install the wave-9 custom-tool seam.
    #[must_use]
    pub fn with_custom_loader(mut self, loader: Arc<dyn CustomToolLoader>) -> Self {
        self.custom_loader = loader;
        self
    }

    /// Install the wave-8 MCP-tool seam.
    #[must_use]
    pub fn with_mcp_loader(mut self, loader: Arc<dyn McpToolLoader>) -> Self {
        self.mcp_loader = loader;
        self
    }

    /// Load every source once and freeze the oracle's source order.
    #[must_use]
    pub fn build(self) -> ToolRegistry {
        let config_directories =
            oc_paths::config_directories(&self.directory, self.worktree.as_deref());
        let mut tools = Vec::new();

        for slot in BUILTIN_ORDER {
            if slot.exposed_by_flags(&self.flags)
                && let Some(tool) = self.builtins.get(&slot)
            {
                tools.push(Arc::clone(tool));
            }
        }

        tools.extend(
            self.custom_loader
                .config_directory_tools(&config_directories),
        );
        tools.extend(self.custom_loader.plugin_tools());
        tools.extend(self.mcp_loader.tools());

        ToolRegistry {
            tools,
            file_tools: self.file_tools,
            flags: self.flags,
            config_directories,
        }
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
    tools: Vec<Arc<dyn Tool>>,
    file_tools: FileTools,
    flags: RegistryFlags,
    config_directories: Vec<PathBuf>,
}

impl ToolRegistry {
    /// Every loaded tool after process-wide exposure flags, before turn filters.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// The config directory chain handed to the custom-tool loader.
    #[must_use]
    pub fn config_directories(&self) -> &[PathBuf] {
        &self.config_directories
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
            .tools
            .iter()
            .filter(|tool| {
                let id = tool.id();
                if id == crate::websearch::ID
                    && !web_search_enabled(input.provider_id, &self.flags.search)
                {
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
}

/// Apply `registry.ts:183-190`'s config-export naming rule.
///
/// A default export takes the source file's basename. Every named export is
/// namespaced as `{basename}_{export}`. `None` means the path has no basename.
#[must_use]
pub fn config_tool_id(path: &Path, export_id: &str) -> Option<String> {
    let namespace = path.file_stem()?.to_string_lossy();
    if export_id == "default" {
        Some(namespace.into_owned())
    } else {
        Some(format!("{namespace}_{export_id}"))
    }
}
