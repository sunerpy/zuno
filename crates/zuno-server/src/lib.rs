//! Secure HTTP assembly and bounded event delivery for every network surface.
//!
//! Route-owning crates build an [`axum::Router`] and hand it to
//! [`ServerBuilder::with_routes`]. The builder merges every route before applying
//! authentication and directory middleware, because `axum::Router::layer` affects
//! only routes that already exist. This keeps later route additions from
//! accidentally escaping the password gate.
//!
//! Two surfaces are served side by side. [`api`] owns the prefixed `/api/*`
//! operations; [`compat_v1`] owns the unprefixed paths the published SDK — and so
//! every resident plugin — actually requests. The v1 surface is deliberately a
//! measured minimum rather than a full port, and it accounts for its own gaps: an
//! unmeasured v1 path answers 404 with instructions instead of leaving a plugin to
//! hang. See that module's docs for why the accounting is scoped to a prefix set
//! rather than installed as a router fallback.
//!
//! Engine transitions enter [`EventFanout`] through
//! [`EventFanout::forward_engine_events`]. Every connection receives its own fixed
//! queue; a stalled connection loses new events and receives an explicit
//! [`Delivery::Lagged`] count instead of making the process grow without bound.

pub mod api;
mod auth;
pub mod compat_v1;
mod directory;
mod discovery;
mod event;
mod events;
mod request_broker;
mod server;

pub use auth::AuthConfig;
pub use compat_v1::{
    CompatV1State, ProviderOAuthAuthorization, ProviderOAuthAuthorizeRequest, ProviderOAuthBackend,
    ProviderOAuthCallbackRequest, ProviderOAuthCompletion, ProviderOAuthFuture, Toast,
    ToastForwarder, UnknownRoutes, V1_PREFIXES, V1_SURFACE, V1Backing, V1Coverage, V1Route,
    compat_v1_router, v1_coverage,
};
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
};
