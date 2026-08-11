//! What the composition root hands a factory.
//!
//! # Why a bare provider name is not enough
//!
//! Three of the oracle's bundled providers need construction parameters that no
//! amount of "look it up from the key" can supply, and all three pick a different
//! *API surface* on the same SDK:
//!
//! - **Azure** (`provider.ts:154-160`) walks `chat` → `responses` → `messages` →
//!   `languageModel`, choosing by a `useChat` flag, and its endpoint is assembled
//!   from a resource name that comes from provider options, then env, then auth.
//! - **Bedrock Mantle** (`:162-166`) routes two specific model ids
//!   (`openai.gpt-oss-safeguard-20b`, `-120b`) to `chat` and everything else to
//!   `responses`, on top of a region.
//! - **GitHub Copilot** (`:225-239`) prefers the model's own declared endpoint,
//!   falls back to a `gpt-N` version check (`N >= 5` and not `gpt-5-mini` →
//!   `responses`), and otherwise uses `chat`.
//!
//! A registry keyed only by name cannot express any of them, and todos 30, 94, 95
//! and 96 would each have to invent a private side channel. So the factory
//! signature takes a [`Spec`], and the reference implementation's parameterized
//! factory (`.omo/refs/jcode/src/cli/startup.rs:160-175`, where OpenRouter serves
//! four identities through one concrete type) is generalized to every provider
//! rather than special-cased for one.
//!
//! # Why a struct of options rather than an enum of variants
//!
//! The reference uses an enum, one variant per identity. That works when the
//! variants are known and few. Here five provider families are still unwritten,
//! and an enum would force each of them to add a variant to a shared type in
//! `oc-llm` — reintroducing, in the type system, exactly the coupling this
//! registry deletes from the dependency graph. A struct of optional parameters
//! grows without any provider family editing the spine.

use std::collections::BTreeMap;

/// Which of a provider SDK's model surfaces to invoke.
///
/// These four are the ones the oracle's Azure selector walks in order, and the
/// same four cover Bedrock Mantle's and Copilot's routing. Naming them as a type
/// keeps the choice out of the string-matching that produced it in the oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApiSurface {
    /// The SDK's own default — `languageModel(id)`.
    #[default]
    Default,
    /// The chat-completions surface — `chat(id)`.
    Chat,
    /// The responses surface — `responses(id)`.
    Responses,
    /// The messages surface — `messages(id)`.
    Messages,
}

/// The parameters a factory needs to construct its provider.
///
/// Fields are `Option` because most providers need none of them; a factory reads
/// the ones its family requires and ignores the rest. The registry never inspects
/// a spec — it forwards it — so adding a field here does not change any control
/// flow in this crate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spec {
    /// The provider identity being instantiated.
    ///
    /// Present so a family serving several identities through one concrete type
    /// knows which profile it is taking on, and so a factory's error can name it
    /// without the registry having to stamp it. This is deliberately separate
    /// from [`factory`](Self::factory): OpenRouter, Groq and Azure all use the
    /// `openai-compatible` factory while retaining distinct behavior.
    pub provider: String,

    /// The registry factory that constructs this identity.
    ///
    /// Defaults to [`provider`](Self::provider), preserving the one-key case for
    /// dedicated families. Production model selection overrides it only when
    /// catalog transport metadata selects a shared wire-family factory.
    factory: String,

    /// Which SDK surface to construct against, when the choice is fixed at
    /// construction time rather than per request.
    ///
    /// Per-request routing — Mantle's model-id check, Copilot's version check —
    /// travels on
    /// [`CompletionRequest::surface`](crate::registry::CompletionRequest::surface)
    /// instead.
    pub surface: ApiSurface,

    /// Base endpoint override.
    ///
    /// Azure's assembled resource endpoint, a Vertex Regional Endpoint Platform
    /// domain, or an OpenAI-compatible profile's own URL.
    pub base_url: Option<String>,

    /// API version, for providers that pin one in the request line.
    ///
    /// Azure's `api-version` is the case that forces this to exist.
    pub api_version: Option<String>,

    /// Cloud region. Bedrock signs with it; Vertex routes with it.
    pub region: Option<String>,

    /// Cloud project. Vertex needs one to build a publisher path.
    pub project: Option<String>,

    /// Headers every request from this provider must carry.
    ///
    /// Anthropic's `anthropic-beta` opt-ins and Copilot's editor-identification
    /// headers are the evidenced cases. Ordered so a spec renders and compares
    /// deterministically, which a prompt-cache stability test (todo 31) will
    /// depend on.
    pub headers: BTreeMap<String, String>,

    /// Whatever else the user's `provider.*.options` carried.
    ///
    /// Dynamically typed on purpose: this mirrors the oracle's own
    /// `Record<string, any>` config surface, so a provider-specific option a user
    /// sets today does not require a field here first. It is a *config* bag, not
    /// an error channel — nothing in the workspace makes a recovery decision from
    /// it.
    pub options: BTreeMap<String, serde_json::Value>,
}

impl Spec {
    /// A spec for `provider` with no parameters.
    ///
    /// The common case: a provider that reads its own credential and needs
    /// nothing from the composition root.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        let provider = provider.into();
        Self {
            factory: provider.clone(),
            provider,
            ..Self::default()
        }
    }

    /// Select a registry factory without changing the provider identity.
    #[must_use]
    pub fn with_factory(mut self, factory: impl Into<String>) -> Self {
        self.factory = factory.into();
        self
    }

    /// The registry key used to construct this provider identity.
    #[must_use]
    pub fn factory(&self) -> &str {
        &self.factory
    }

    /// Fix the SDK surface to construct against.
    #[must_use]
    pub fn with_surface(mut self, surface: ApiSurface) -> Self {
        self.surface = surface;
        self
    }

    /// Override the base endpoint.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Pin the API version.
    #[must_use]
    pub fn with_api_version(mut self, api_version: impl Into<String>) -> Self {
        self.api_version = Some(api_version.into());
        self
    }

    /// Set the cloud region.
    #[must_use]
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Set the cloud project.
    #[must_use]
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Add a header every request must carry.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Carry one entry of the user's `provider.*.options` bag.
    #[must_use]
    pub fn with_option(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.options.insert(key.into(), value);
        self
    }
}
