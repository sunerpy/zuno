//! Zuno-native extension packages with two explicit lifetimes.
//!
//! A process package is defined through model tools and exists only in the
//! [`ExtensionRegistry`] owned by that process. A static package is read from
//! `extensions/<id>/extension.json` below a Zuno configuration directory on each
//! composition. Both forms use [`Package`], the same validation, and the same
//! contribution merger. Only static packages may declare executable runtimes;
//! those mount as transactional profile effects and expose native tool proxies.

mod host;
mod install;
mod manifest;
mod registry;
mod resolve;
mod static_loading;
mod tools;

pub use host::{
    PLUGIN_PROTOCOL_VERSION, PLUGIN_WIT, PluginHostError, RuntimeSurface, RuntimeSurfaceError,
    runtime_surface,
};
pub use install::{InstallError, InstallMode, InstalledPackage, install_local, remove_installed};
pub use manifest::{
    API_VERSION, DEFAULT_TIMEOUT_MS, DEFAULT_WASI_FUEL, DEFAULT_WASI_MEMORY_MIB, Package,
    PluginCapability, PluginRuntime, PluginToolConcurrency, PluginToolDefinition, PluginToolEffect,
    PluginToolReplay, PluginToolUiIntent, SkillDefinition, WorkflowDefinition,
};
pub use registry::{
    CompositionLease, DynamicState, ExtensionRegistry, ExtensionTransaction, PackageStatus,
    PreparedTransition, RegistryError, Scope, StageOutcome,
};
pub use resolve::{
    PackageOrigin, ResolvedExtensions, ResolvedPackage, resolve_active, resolve_desired,
};
pub use static_loading::{STATIC_DIRECTORY, STATIC_MANIFEST, StaticPackage, discover_static};
pub use tools::lifecycle_tools;
