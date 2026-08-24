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
//!    [`zuno_error::ProviderError::Auth`]'s `provider` field.
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
//! - **Authentication flows.** `zuno-auth` owns credential storage; a factory reads
//!   it at construction, and the constructed provider is already authenticated.
//! - **Retry, backoff and compaction.** `zuno-error`'s [`zuno_error::Recovery`]
//!   decides those from the error, once, for every provider.
//! - **Prompt caching and reasoning-effort resolution.** `effort.rs` and
//!   `cache.rs` (todo 31), applied to the request before it reaches a provider.
//! - **SSE framing.** One parser in `sse.rs` (todo 27) serves every family.

pub use crate::event::{FinishReason, Message, RequestContentBlock, Role, StreamEvent};
use crate::registry::spec::ApiSurface;
use serde_json::Value;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use zuno_error::ProviderError;

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
/// Implementations must not render a credential — see `zuno-auth`'s redaction rule.
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
/// This is deliberately the provider-safe shape: a model id, the SDK surface to
/// invoke, and outbound messages whose block type cannot represent history-only
/// or unsigned reasoning. Transcript storage remains in `event.rs`; conversion at
/// that boundary preserves signed and encrypted reasoning while filtering blocks
/// that must not be replayed.
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
    /// The durable conversation and static instruction messages, in order.
    pub messages: Vec<Message>,
    /// Volatile non-user policy for this request, such as active Goal state or memory.
    ///
    /// Providers map each item to their native developer/system context without
    /// inserting it into replayable user history or the cacheable static prefix.
    pub developer_context: Vec<String>,
    /// The tools this request offers the model, already frozen for the turn.
    ///
    /// Carried here rather than on the provider because the set is a property of
    /// the *request*: it is the snapshot the turn loop locked, and dispatch is
    /// checked against exactly what the model was shown. A provider that held its
    /// own tool list could answer with a call the loop would then refuse.
    pub tools: Vec<ToolSchema>,
    /// Per-request provider parameters contributed after model resolution.
    ///
    /// Provider implementations consume these after building their normal body,
    /// so a plugin mutation applies to this request only and cannot leak into the
    /// provider instance shared by later turns.
    pub parameters: serde_json::Map<String, serde_json::Value>,
    /// Per-request HTTP headers contributed after model resolution.
    pub headers: BTreeMap<String, String>,
}

/// One tool as the model is told about it, before any provider's wire shape.
///
/// Provider-neutral on purpose: `zuno-llm` does not depend on `zuno-tool`, and each
/// family nests these three fields differently — OpenAI under
/// `function`, Anthropic and Gemini at the top level. Translating in the provider
/// keeps this spine free of any one vendor's envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchema {
    /// The name the model calls.
    pub name: String,
    /// The description the model reads.
    pub description: String,
    /// The JSON Schema for the arguments.
    pub parameters: Value,
}

/// A provider-bound tool call whose arguments are not a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "tool call `{call_id}` for `{tool}` in message {message_index} block {block_index} has non-object arguments"
)]
pub struct InvalidToolArguments {
    pub message_index: usize,
    pub block_index: usize,
    pub call_id: String,
    pub tool: String,
}

impl CompletionRequest {
    /// A request for `model_id` carrying `messages` on the provider's default
    /// surface, offering no tools.
    #[must_use]
    pub fn new(model_id: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model_id: model_id.into(),
            surface: ApiSurface::Default,
            messages,
            developer_context: Vec::new(),
            tools: Vec::new(),
            parameters: serde_json::Map::new(),
            headers: BTreeMap::new(),
        }
    }

    /// Attach independent volatile developer-context items.
    #[must_use]
    pub fn with_developer_context(mut self, developer_context: Vec<String>) -> Self {
        self.developer_context = developer_context;
        self
    }

    /// Offer `tools` to the model on this request.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    /// Pin this request to a specific SDK surface.
    #[must_use]
    pub fn on_surface(mut self, surface: ApiSurface) -> Self {
        self.surface = surface;
        self
    }

    /// Reject a request that would serialize a scalar tool argument value.
    ///
    /// Provider protocols disagree on many details, but they all require tool
    /// arguments to be an object. Keeping this validation on the provider-neutral
    /// request makes malformed persisted history fail locally instead of becoming
    /// a remote HTTP 400 whose field path is difficult to diagnose.
    pub fn validate_tool_arguments(&self) -> Result<(), InvalidToolArguments> {
        for (message_index, message) in self.messages.iter().enumerate() {
            for (block_index, block) in message.content.iter().enumerate() {
                if let RequestContentBlock::ToolUse {
                    id, name, input, ..
                } = block
                    && !input.is_object()
                {
                    return Err(InvalidToolArguments {
                        message_index,
                        block_index,
                        call_id: id.clone(),
                        tool: name.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Overlay request-local parameters onto an object-shaped provider body.
    ///
    /// Parameters arrive in SDK provider-option vocabulary — that is what
    /// [`resolve_effort`](crate::effort::resolve_effort) and a model catalog's
    /// declared variants both speak — so they are lowered to `sending_to` before
    /// they are merged. This is the single choke point through which request-local
    /// parameters reach *any* provider body, so a provider that skips it receives
    /// no parameters at all rather than shipping an option name the endpoint does
    /// not read.
    ///
    /// `sending_to` is a required argument rather than a read of
    /// [`surface`](Self::surface) because those two are not the same thing: a
    /// request may carry [`ApiSurface::Default`] while the provider's own rules
    /// resolve that to `/responses`, and one option name lowers differently on
    /// each. Passing it in forces every adapter to name the surface it is actually
    /// posting to, and a new adapter cannot compile without answering the question.
    ///
    /// The merge is recursive: an object-valued parameter is merged into an
    /// object already at that key instead of replacing it. Without that, Gemini's
    /// `generationConfig.thinkingConfig` would arrive by evicting the sampling
    /// fields the provider had already written to `generationConfig`.
    pub fn apply_parameters(&self, body: &mut serde_json::Value, sending_to: ApiSurface) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        let wire = crate::effort::lower_to_wire(&self.parameters, sending_to);
        merge_into(object, &wire);
    }
}

fn merge_into(
    target: &mut serde_json::Map<String, Value>,
    update: &serde_json::Map<String, Value>,
) {
    for (name, value) in update {
        match (target.get_mut(name), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                merge_into(existing, incoming);
            }
            _ => {
                target.insert(name.clone(), value.clone());
            }
        }
    }
}

/// Whether a credential exists for a provider key.
///
/// `zuno-llm` does not depend on `zuno-auth`, and this one-method trait is why: the
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_bound_tool_arguments_must_be_json_objects() {
        for input in [json!("malformed string"), json!(["array"]), Value::Null] {
            let request = CompletionRequest::new(
                "gpt-test",
                vec![Message::from_content(
                    Role::Assistant,
                    vec![RequestContentBlock::ToolUse {
                        id: "call_bad".to_owned(),
                        name: "write".to_owned(),
                        input,
                        thought_signature: None,
                    }],
                )],
            );
            let error = request
                .validate_tool_arguments()
                .expect_err("non-object tool arguments must be rejected locally");
            assert_eq!(error.message_index, 0);
            assert_eq!(error.block_index, 0);
            assert_eq!(error.call_id, "call_bad");
            assert_eq!(error.tool, "write");
        }

        let valid = CompletionRequest::new(
            "gpt-test",
            vec![Message::from_content(
                Role::Assistant,
                vec![RequestContentBlock::ToolUse {
                    id: "call_ok".to_owned(),
                    name: "write".to_owned(),
                    input: json!({"filePath":"README.md","content":"ok"}),
                    thought_signature: None,
                }],
            )],
        );
        valid
            .validate_tool_arguments()
            .expect("object tool arguments remain valid");
    }
}
