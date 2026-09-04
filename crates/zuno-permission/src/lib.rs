//! Permission engine: ordered rule matching and pending approval lifecycle.
//!
//! The TypeScript oracle flattens permission configuration in source order and
//! uses `findLast` over the result. That ordering is a security property: a
//! later catch-all can deliberately override an earlier specific grant. **The
//! last matching rule wins**, so a catch-all belongs first and the narrow rules
//! that are meant to override it belong last.
//!
//! A resource is matched under every spelling that denotes it, not only the raw
//! text the caller happened to pass. [`resource`] documents the canonical
//! spelling of a shell command and of a path, and why a `deny` is allowed to
//! cover more spellings than an `allow`.

mod engine;
pub mod resource;
mod rule;
mod types;
pub mod visibility;
mod wildcard;

pub use crate::engine::PermissionEngine;
pub use crate::resource::{MatchReason, canonical_path_resource, canonical_shell_resource};
pub use crate::rule::{Decision, Denial, Matched, decide, evaluate, rules_from_config};
pub use crate::types::{
    Authorization, PermissionReply, PermissionRequest, ReplyKind, ReplyOutcome, ResolvedRequest,
    Rule, ToolCall,
};
pub use crate::wildcard::wildcard_match;
pub use zuno_config::schema::permission::PermissionAction;
