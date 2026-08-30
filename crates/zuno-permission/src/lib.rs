//! Permission engine: ordered rule matching and pending approval lifecycle.
//!
//! The TypeScript oracle flattens permission configuration in source order and
//! uses `findLast` over the result. That ordering is a security property: a
//! later catch-all can deliberately override an earlier specific grant.

mod engine;
mod rule;
mod types;
pub mod visibility;
mod wildcard;

pub use crate::engine::PermissionEngine;
pub use crate::rule::{evaluate, rules_from_config};
pub use crate::types::{
    Authorization, PermissionReply, PermissionRequest, ReplyKind, ReplyOutcome, ResolvedRequest,
    Rule, ToolCall,
};
pub use crate::wildcard::wildcard_match;
pub use zuno_config::schema::permission::PermissionAction;
