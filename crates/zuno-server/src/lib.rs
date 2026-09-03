//! Secure HTTP assembly and bounded event delivery for every network surface.
//!
//! Route-owning crates build an [`axum::Router`] and hand it to
//! [`ServerBuilder::with_routes`]. The builder merges every route before applying
//! authentication and directory middleware, because `axum::Router::layer` affects
//! only routes that already exist. This keeps later route additions from
//! accidentally escaping the password gate.
//!
//! [`api`] owns the native `/api/*` operations. Extension clients use the same
//! native surface rather than an emulated OpenCode route set.
//!
//! Engine transitions enter [`EventFanout`] through
//! [`EventFanout::forward_engine_events`]. Every connection receives its own fixed
//! queue; a stalled connection loses new events and receives an explicit
//! [`Delivery::Lagged`] count instead of making the process grow without bound.

pub mod api;
mod auth;
mod browser_auth;
mod directory;
mod discovery;
mod event;
mod events;
mod request_broker;
mod server;

pub use auth::AuthConfig;
pub use directory::RequestDirectory;
pub use discovery::local_server_urls;
pub use event::{DEFAULT_EVENT_SUBSCRIBER_CAPACITY, Delivery, EventFanout, EventSubscription};
pub use events::{
    EventCursor, EventService, EventStreamError, NewEvent, StreamEvent, events_router,
};
pub use request_broker::{
    PermissionRequest, PermissionResolution, QuestionAnswers, QuestionDecision, QuestionRequest,
    QuestionResolution, QuestionToolCall, RequestBroker, RequestSource,
};
pub use server::{
    BoundServer, ServerBuilder, ServerConfig, ServerError, ServerServices, SessionCompactExecution,
    SessionModelSelection, SessionMutationExecutor, SessionMutationFuture, SessionPromptExecution,
    SessionReportExecution,
};
