//! Configuration schema, discovery, merge order, and variable substitution.

pub mod discovery;
pub mod instructions;
pub mod json_schema;
pub mod schema;
pub mod variable;

pub use crate::instructions::{
    InstructionOptions, InstructionPath, InstructionText, InstructionWarning, Instructions,
    LoadedInstructions, Origin, UpwardClaims, WarningKind,
};
pub use crate::schema::sandbox::{
    SandboxConfig, SandboxMode, SandboxNetworkMode, SandboxUnavailableAction,
};
pub use crate::schema::{Config, KNOWN_TOP_LEVEL_KEYS, WebSearchBackend, WebSearchConfig};
pub use crate::variable::{Missing, Source, Substitution};
