//! Secure HTTP assembly and bounded event delivery for every network surface.
//!
//! Route-owning crates build an [`axum::Router`] and hand it to
//! [`ServerBuilder::with_routes`]. The builder merges every route before applying
//! authentication and directory middleware, because `axum::Router::layer` affects
//! only routes that already exist. This keeps later route additions from
//! accidentally escaping the password gate.
//!
//! Engine transitions enter [`EventFanout`] through
//! [`EventFanout::forward_engine_events`]. Every connection receives its own fixed
//! queue; a stalled connection loses new events and receives an explicit
//! [`Delivery::Lagged`] count instead of making the process grow without bound.

mod auth;
mod directory;
mod event;
mod server;

pub use auth::AuthConfig;
pub use directory::RequestDirectory;
pub use event::{DEFAULT_EVENT_SUBSCRIBER_CAPACITY, Delivery, EventFanout, EventSubscription};
pub use server::{BoundServer, ServerBuilder, ServerConfig, ServerError, ServerServices};
