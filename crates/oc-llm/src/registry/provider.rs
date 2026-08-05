//! The contract a registered provider satisfies.
//!
//! # Why this trait is three methods
//!
//! The reference implementation's provider trait carries roughly thirty methods,
//! and the plan for this workspace names that an anti-pattern. The cost is not
//! aesthetic: every method is a thing all five provider families must answer,
//! including the ones for which the question is meaningless, and each one is a
//! place where a caller can reach past the abstraction and start behaving
//! differently per provider. A trait that wide stops being an interface and
//! becomes a union of five implementations.
//!
//! So this trait answers exactly three questions, which are the three the turn
//! loop cannot proceed without:
//!
//! 1. *Which provider is this?* — [`Provider::id`], for reporting and for
//!    [`oc_error::ProviderError::Auth`]'s `provider` field.
//! 2. *What can it do?* — [`Provider::capabilities`], so callers branch on a
//!    capability rather than on `id() == "anthropic"`.
//! 3. *Run one turn.* — [`Provider::stream`].
//!
//! Everything else a provider family needs is that family's own inherent method,
//! reachable from the crate that implements it and invisible here.
//!
//! # What is deliberately not here
//!
//! - **Model listing and metadata.** The catalog is resolved from models.dev plus
//!   config, env and auth, and belongs to `catalog.rs` (todo 26). A provider that
//!   also enumerated models would give the workspace two disagreeing answers.
//! - **Token counting and cost.** Catalog data, not provider behaviour.
//! - **Authentication flows.** `oc-auth` owns credential storage; a factory reads
//!   it at construction, and the constructed provider is already authenticated.
//! - **Retry, backoff and compaction.** `oc-error`'s [`oc_error::Recovery`]
//!   decides those from the error, once, for every provider.
//! - **Prompt caching and reasoning-effort resolution.** `effort.rs` and
//!   `cache.rs` (todo 31), applied to the request before it reaches a provider.
//! - **SSE framing.** One parser in `sse.rs` (todo 27) serves every family.

use crate::registry::spec::ApiSurface;
use oc_error::ProviderError;
use std::pin::Pin;
use std::sync::Arc;

/// A constructed, ready-to-use model provider.
///
/// Held as `Arc<dyn Provider>` so the registry can hand the same instance to
/// several concurrent turns; implementations are therefore `Send + Sync` and
/// interior-mutable where they need state.
///
/// `Debug` is a supertrait rather than a method, so it costs the trait no width.
/// It is required because this crate's job is diagnostics: a `Result` carrying a
/// provider must be printable when it turns out to be the wrong branch, and a
/// startup audit that lists what got wired is worth more than one that counts it.
/// Implementations must not render a credential — see `oc-auth`'s redaction rule.
pub trait Provider: std::fmt::Debug + Send + Sync + 'static {
    /// The registry key this instance was constructed for.
    ///
    /// Returned rather than stored by the registry because a family that serves
    /// several keys through one concrete type — an OpenAI-compatible profile, a
    /// Bedrock model routed to a different surface — knows which identity it took
    /// on, and the registry does not.
    fn id(&self) -> &str;

    /// What this provider supports.
    ///
    /// One call returning a plain value rather than five predicate methods: a
    /// caller that needs two answers should not pay two virtual calls, and a new
    /// capability should not widen the trait.
    fn capabilities(&self) -> Capabilities;

    /// Run one streaming completion.
    ///
    /// The only I/O entry point on the trait. Returning the stream rather than a
    /// future of a stream keeps the method object-safe without `async_trait`, and
    /// lets a provider surface a request-shaping failure as the stream's first
    /// item instead of a second error channel.
    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_>;
}

/// The stream a provider returns for one completion.
///
/// Boxed and pinned because the trait is object-safe; borrowed from the provider
/// because implementations hold their HTTP client and credentials.
pub type ProviderStream<'a> =
    Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, ProviderError>> + Send + 'a>>;

/// What a provider supports, as data rather than as five trait methods.
///
/// Every field is here because some todo downstream branches on it, and none is
/// here speculatively:
///
/// - `reasoning` — the five-way reasoning model (todo 28) only applies to models
///   that emit reasoning at all.
/// - `tool_calls` — the turn loop skips tool assembly entirely without it.
/// - `prompt_cache` — cache breakpoints (todos 29, 31) are a no-op elsewhere, and
///   inserting them anyway costs tokens.
/// - `attachments` — image and file parts must be dropped or rejected, not sent,
///   for a text-only model.
/// - `sampling_params` — several reasoning models *reject* `temperature` and
///   `top_p`; todo 30 strips them, and needs to know when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// The provider can emit reasoning alongside its answer.
    pub reasoning: bool,
    /// The provider accepts tool definitions and emits tool calls.
    pub tool_calls: bool,
    /// The provider honours explicit prompt-cache breakpoints.
    pub prompt_cache: bool,
    /// The provider accepts non-text input parts.
    pub attachments: bool,
    /// The provider accepts sampling parameters such as `temperature`.
    pub sampling_params: bool,
}

impl Capabilities {
    /// The capability set of a plain text-completion model.
    ///
    /// A useful floor for a fake in a test and for a compatible endpoint whose
    /// real capabilities are unknown until the catalog says otherwise.
    #[must_use]
    pub const fn text_only() -> Self {
        Self {
            reasoning: false,
            tool_calls: false,
            prompt_cache: false,
            attachments: false,
            sampling_params: true,
        }
    }
}

/// One completion request, as the registry's contract sees it.
///
/// # Scope
///
/// This is deliberately the *narrow* shape: a model id, the surface to invoke it
/// on, and the turn's text. The full message model — content parts, tool results,
/// attachments — belongs to the session layer, and the full stream event
/// vocabulary belongs to `event.rs` and `stream.rs` (todo 28), which also adds
/// `RetryRollback` and the five-way reasoning model.
///
/// They are not redefined here, and this type is not a placeholder for them: it
/// is what `Provider::stream` needs to have a real signature today, and todo 28
/// widens it additively. A registry whose only trait method took no arguments and
/// returned nothing would compile and prove nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    /// The model to invoke, named as the provider's own catalog names it.
    pub model_id: String,
    /// Which of the provider SDK's surfaces to call.
    ///
    /// Carried per request, not per provider, because the oracle chooses it per
    /// *model*: Bedrock Mantle routes two specific model ids to `chat` and
    /// everything else to `responses`, and Copilot picks by the model's declared
    /// endpoint and then by a `gpt-N` version check. See
    /// [`ApiSurface`](crate::registry::ApiSurface).
    pub surface: ApiSurface,
    /// The turn, in order.
    pub messages: Vec<Message>,
}

impl CompletionRequest {
    /// A request for `model_id` carrying `messages` on the provider's default
    /// surface.
    #[must_use]
    pub fn new(model_id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model_id: model_id.into(),
            surface: ApiSurface::Default,
            messages,
        }
    }

    /// Pin this request to a specific SDK surface.
    #[must_use]
    pub fn on_surface(mut self, surface: ApiSurface) -> Self {
        self.surface = surface;
        self
    }
}

/// One message in a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

impl Message {
    /// A message from `role` carrying `text`.
    #[must_use]
    pub fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
        }
    }
}

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// One event from a provider's stream.
///
/// The intersection of what every bundled SDK family emits, and no more. Todo 28
/// owns the full vocabulary; this exists so a provider's stream item is a typed
/// value rather than an untyped blob, and so a fake provider in a test can
/// actually stream something.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A fragment of the answer.
    TextDelta(String),
    /// A fragment of the model's reasoning.
    ReasoningDelta(String),
    /// Token accounting, reported once the provider knows it.
    Usage { input: u64, output: u64 },
    /// The turn ended.
    Finish(FinishReason),
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished its answer.
    Stop,
    /// The model hit its output limit mid-answer.
    Length,
    /// The model wants tools run before continuing.
    ToolCalls,
    /// The model declined to answer.
    Refusal,
}

/// Whether a credential exists for a provider key.
///
/// `oc-llm` does not depend on `oc-auth`, and this one-method trait is why: the
/// registry needs to know *whether* a provider is credentialed in order to report
/// the credentialed-but-unwired case, and nothing more. The credential store
/// implements this from the other side, so the spine stays free of storage,
/// refresh and file-permission concerns.
///
/// It is the same inversion the registry itself performs, applied to the one
/// dependency the diagnostic would otherwise force in.
pub trait CredentialPresence {
    /// True when a usable credential is stored for `provider`.
    ///
    /// Presence only. Neither the value nor its shape crosses this boundary.
    fn has_credential(&self, provider: &str) -> bool;
}

impl<T: CredentialPresence + ?Sized> CredentialPresence for &T {
    fn has_credential(&self, provider: &str) -> bool {
        (**self).has_credential(provider)
    }
}

impl<T: CredentialPresence + ?Sized> CredentialPresence for Arc<T> {
    fn has_credential(&self, provider: &str) -> bool {
        (**self).has_credential(provider)
    }
}
