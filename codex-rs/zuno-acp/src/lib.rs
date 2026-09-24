#![recursion_limit = "256"]

//! Native Agent Client Protocol (ACP) adapter for Zuno.
//!
//! The adapter is intentionally a protocol edge. Codex App Server remains the
//! only execution and persistence authority; ACP sessions are projections of
//! App Server threads, not a second agent loop.

mod adapter;
mod transport;

pub use adapter::AcpBridgeError;
pub use adapter::CodexAcpAgent;
pub use adapter::serve_app_server_stdio;
pub use adapter::serve_in_process_stdio;
pub use codex_app_server_client::InProcessClientStartArgs;
pub use transport::Agent;
pub use transport::ClientConnection;
pub use transport::RequestId;
pub use transport::RpcError;
pub use transport::ServeError;
pub use transport::serve_stdio;

/// Stable ACP v1 methods implemented by the adapter.
pub const IMPLEMENTED_METHODS: &[&str] = &[
    "initialize",
    "session/new",
    "session/load",
    "session/resume",
    "session/fork",
    "session/list",
    "session/prompt",
    "session/steer",
    "session/set_config_option",
    "session/set_mode",
    "session/set_model",
    "session/cancel",
    "session/close",
    "session/delete",
];
