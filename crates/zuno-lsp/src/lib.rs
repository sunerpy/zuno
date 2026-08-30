//! Language server client pool used for diagnostics and symbol lookup.

pub mod client;
pub mod manager;
pub mod registry;
pub mod tool;

pub use client::{Client, ClientError, Diagnostic, Position, Range};
pub use manager::{Manager, ManagerError, ManagerEvent, RestartPolicy, ServerState, ServerStatus};
pub use registry::{
    InstallKind, InstallRequest, NoopInstaller, RegistryError, RootPolicy, ServerInstaller,
    ServerRegistry, ServerSpec,
};
pub use tool::{LspOperation, LspParams, LspTool};
