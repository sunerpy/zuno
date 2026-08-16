//! The genuinely-OpenAI-compatible provider profile.
//!
//! One configurable profile serves every vendor that really speaks
//! chat-completions on the wire — OpenRouter, xAI, Mistral, Groq, DeepInfra,
//! Cerebras, Cohere, TogetherAI, Perplexity, Vercel, Alibaba, GitLab, Venice,
//! DeepSeek, Cloudflare Workers AI and its AI Gateway — plus the two that are
//! compatible with a rule: **Azure**, whose surface comes from an availability
//! walk gated by one provider option, and **GitHub Copilot**, whose surface is
//! chosen per model id. See [`family::CLAIMED`] for the exact list.
//!
//! # What this crate refuses, and why that matters
//!
//! Anthropic, Bedrock and Google/Vertex are **not** here. Their wire protocols
//! genuinely differ — typed SSE events, binary EventStream framing over
//! SigV4-signed requests, and a `contents`/`parts` request shape returning
//! `candidates` — so none of them produces a `choices[].delta` this profile could
//! read. Both reference implementations treat OpenAI-compatible as the fallthrough
//! default, which turns a misconfiguration into a deserialization error naming
//! JSON instead of naming the crate the user should have configured. Here an
//! unclaimed id is refused at construction with a message that names the
//! destination. See [`family`].
//!
//! # Layout
//!
//! | module | responsibility |
//! |---|---|
//! | [`family`] | which ids this profile claims, and who carries the rest |
//! | [`surface`] | the Azure walk and the Copilot per-model rule, in one file |
//! | [`quirks`] | the quirk table, keyed by capability where possible |
//! | [`wire`] | the chunk shape, derived from the recorded corpus |
//! | [`request`] | body assembly, including the three ordering and gating rules |
//! | [`stream`] | chunk-to-[`StreamEvent`](zuno_llm::registry::StreamEvent) translation |
//! | [`transport`] | the one seam to the network, so tests replay bytes |
//! | [`provider`] | the [`Provider`](zuno_llm::registry::Provider) implementation |
//!
//! # What this crate deliberately does not do
//!
//! - **It does not parse SSE.** [`zuno_llm::sse::SseParser`] owns framing and the
//!   UTF-8 boundary state that keeps a code point split across two network chunks
//!   intact. There is no `from_utf8_lossy` anywhere in this crate.
//! - **It does not classify errors from rendered text.** Recovery comes from the
//!   HTTP status and from structured body fields. Both reference implementations
//!   shipped `is_retryable(&message.to_lowercase())`; `zuno-error` exists so this one
//!   does not need to.
//! - **It does not resolve reasoning effort or prompt-cache policy.** Those are
//!   `zuno-llm`'s `effort` and `cache` modules, applied to the request before it
//!   arrives, and they reach the wire through
//!   [`EXTRA_BODY_OPTION`](provider::EXTRA_BODY_OPTION).
//! - **It does not hand-write JSON schemas.** Tool definitions are passed through
//!   from `zuno-tool`.
//!
//! # Example
//!
//! ```
//! use zuno_llm::registry::{ApiSurface, Spec};
//! use zuno_provider_compatible::{CompatibleProvider, ReqwestTransport, Transport};
//! use std::sync::Arc;
//!
//! let transport: Arc<dyn Transport> = Arc::new(ReqwestTransport::new("groq"));
//! let provider = CompatibleProvider::new(
//!     Spec::new("groq").with_base_url("https://api.groq.com/openai/v1"),
//!     transport,
//!     Some("token".to_owned()),
//! )
//! .expect("groq is a claimed provider id");
//!
//! assert_eq!(
//!     provider.endpoint("llama-3.3-70b", ApiSurface::Default),
//!     "https://api.groq.com/openai/v1/chat/completions"
//! );
//! ```
//!
//! Refusing a provider this profile cannot speak for:
//!
//! ```
//! use zuno_llm::registry::Spec;
//! use zuno_provider_compatible::family;
//!
//! let error = family::resolve(&Spec::new("amazon-bedrock")).expect_err("wrong family");
//! assert!(error.to_string().contains("unsupported"));
//! assert!(error.to_string().contains("zuno-provider-bedrock"));
//! ```

pub mod family;
pub mod provider;
pub mod quirks;
pub mod request;
pub mod stream;
pub mod surface;
pub mod transport;
pub mod wire;

pub use crate::family::{Family, Profile, SurfaceRule, UnsupportedProvider};
pub use crate::provider::{CompatibleProvider, compatible_default_capabilities, factory};
pub use crate::quirks::Quirks;
pub use crate::request::{RequestBody, Sampling};
pub use crate::stream::{ChunkTranslator, ResponsesTranslator, SurfaceTranslator};
pub use crate::surface::{SurfaceSupport, azure_surface, copilot_surface, endpoint_path};
pub use crate::transport::{ChunkStream, HttpRequest, ReqwestTransport, Transport};
pub use crate::wire::ChatChunk;
