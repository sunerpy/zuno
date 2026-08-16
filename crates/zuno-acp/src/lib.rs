//! Agent Client Protocol adapter for external editor clients.

mod permission;
mod projection;
mod transport;

pub mod conformance;

pub use permission::AcpPermissionAsker;
pub use projection::turn_event_update;
pub use transport::{Agent, ClientConnection, RpcError, ServeError, serve_stdio};

pub const IMPLEMENTED_METHODS: [&str; 13] = [
    "initialize",
    "authenticate",
    "session/new",
    "session/load",
    "session/list",
    "session/resume",
    "session/close",
    "session/fork",
    "session/set_config_option",
    "session/set_mode",
    "session/set_model",
    "session/prompt",
    "session/cancel",
];
