//! Agent Client Protocol adapter for external editor clients.

mod permission;
mod presentation;
mod projection;
mod question;
mod replay;
mod routing;
mod transport;

pub mod conformance;

pub use permission::{AcpPermissionAsker, AcpPermissionGrants};
pub use projection::{TurnEventProjector, turn_event_update};
pub use question::AcpQuestionAsker;
pub use replay::{
    DurableReplay, REPLAY_MESSAGE_CAP, REPLAY_TRANSCRIPT_BYTE_CAP, ReplayPolicy,
    durable_plan_update, durable_updates, durable_usage_update,
};
pub use routing::{AcpSessionRoute, RoutedSession};
pub use transport::{Agent, ClientConnection, RpcError, ServeError, serve_stdio};

pub const IMPLEMENTED_METHODS: [&str; 11] = [
    "initialize",
    "session/new",
    "session/load",
    "session/set_mode",
    "session/set_config_option",
    "session/prompt",
    "session/cancel",
    "session/list",
    "session/delete",
    "session/resume",
    "session/close",
];
