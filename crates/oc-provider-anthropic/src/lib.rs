//! Anthropic Messages wire protocol: streaming, tool use, reasoning, and cache control.
//!
//! The crate deliberately separates three jobs:
//!
//! - [`request`] translates provider-neutral messages into Anthropic Messages
//!   request JSON and places cache breakpoints at stable prefix boundaries.
//! - [`stream::AnthropicDecoder`] feeds raw bytes through [`oc_llm::sse`] and
//!   translates Anthropic events into provider-neutral [`oc_llm::event::StreamEvent`]s.
//! - [`AnthropicProvider`] owns authentication and the HTTP transport.
//!
//! No network chunk is decoded here. Every chunk goes directly to the shared SSE
//! parser, which retains incomplete UTF-8 code points between chunks.

mod error;
mod provider;
pub mod request;
pub mod stream;

pub use crate::error::{AnthropicErrorBody, map_http_error, retry_after};
pub use crate::provider::{AnthropicAuth, AnthropicConfig, AnthropicProvider, factory};
pub use crate::request::build_request_body;
pub use crate::stream::AnthropicDecoder;
