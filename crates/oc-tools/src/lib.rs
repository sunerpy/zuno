//! Built-in tool implementations: file, shell, search, web, and task tools.
//!
//! # Search
//!
//! [`glob`] matches paths and [`grep`] searches contents, both over
//! [`oc_search`]'s embedded engine. Nothing here downloads a ripgrep binary, which
//! the oracle does on first search; a system `rg` is reachable only through
//! [`oc_search::Backend::from_env`] and is never selected implicitly.

pub mod glob;
pub mod grep;
pub mod search_common;

pub use crate::glob::{GlobParams, GlobTool};
pub use crate::grep::{GrepParams, GrepTool};
pub use crate::search_common::{RESULT_LIMIT, SearchScope, SearchTooling};
