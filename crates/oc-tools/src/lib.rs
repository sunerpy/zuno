//! Built-in tool implementations: file, shell, search, web, and task tools.
//!
//! # The web tools
//!
//! [`webfetch`] retrieves a URL; [`websearch`] queries a search backend. Both are
//! bounded in time, response size and redirect hops, poll the turn's interrupt while
//! a body streams, and treat everything they retrieve as data rather than
//! instruction. See [`webfetch::bounds`] for the values and where each came from.

pub mod webfetch;
pub mod websearch;

pub use crate::webfetch::WebFetchTool;
pub use crate::webfetch::bounds::WebError;
pub use crate::websearch::WebSearchTool;
pub use crate::websearch::gating::{Provider, SearchConfig, select_provider, web_search_enabled};
