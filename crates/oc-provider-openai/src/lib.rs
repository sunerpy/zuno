//! OpenAI Chat Completions and Responses wire protocols.

mod error;
mod provider;
pub mod request;
pub mod stream;

pub use crate::error::{OpenAiErrorBody, map_http_error, retry_after};
pub use crate::provider::{OpenAiConfig, OpenAiProvider};
pub use crate::request::{Sampling, build_request_body, is_reasoning_model, resolve_surface};
pub use crate::stream::OpenAiDecoder;
