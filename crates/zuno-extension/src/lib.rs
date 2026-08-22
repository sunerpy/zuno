//! Zuno-native extension packages with two explicit lifetimes.
//!
//! A process package is defined through model tools and exists only in the
//! [`ExtensionRegistry`] owned by that process. A static package is read from
//! `extensions/<id>/extension.json` below a Zuno configuration directory on each
//! composition. Both forms use [`Package`], the same validation, and the same
//! contribution merger.

mod manifest;
mod registry;
mod resolve;
mod static_loading;
mod tools;

pub use manifest::{API_VERSION, Package, SkillDefinition, WorkflowDefinition};
pub use registry::{DynamicState, ExtensionRegistry, PackageStatus, Scope};
pub use resolve::{PackageOrigin, ResolvedExtensions, ResolvedPackage, resolve_active};
pub use static_loading::{STATIC_DIRECTORY, STATIC_MANIFEST, StaticPackage, discover_static};
pub use tools::lifecycle_tools;
