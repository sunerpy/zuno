//! Configuration schema, discovery, merge order, variable substitution, and legacy rejection.

pub mod discovery;
pub mod instructions;
pub mod legacy;
pub mod schema;
pub mod variable;

pub use crate::instructions::{
    InstructionOptions, InstructionPath, InstructionText, InstructionWarning, Instructions,
    LoadedInstructions, Origin, UpwardClaims, WarningKind,
};
pub use crate::legacy::{DeprecatedForm, Deprecation};
pub use crate::schema::{
    Config, KNOWN_TOP_LEVEL_KEYS, LEGACY_TUI_KEYS, LegacyTuiKey, WebSearchBackend, WebSearchConfig,
};
pub use crate::variable::{Missing, Source, Substitution};
