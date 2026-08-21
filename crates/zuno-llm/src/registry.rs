//! The provider factory registry: how a provider implementation reaches the spine
//! without the spine naming it.
//!
//! # The problem this solves
//!
//! `zuno-llm` is the spine. `zuno-engine`, `zuno-server`, `zuno-tui` and `zuno-cli` all sit
//! on top of it, and the five provider families sit beside it — Anthropic, the
//! OpenAI family, genuinely-compatible endpoints, Bedrock, and Google/Vertex.
//! Those five are separate crates because their wire protocols genuinely differ:
//! Bedrock needs SigV4 signing and EventStream framing, Google has its own request
//! shape, Vertex adds GCP authentication on top.
//!
//! The obvious wiring — `zuno-llm` depends on all five and dispatches — costs the
//! workspace every rebuild. `zuno-llm` is upstream of everything, so a one-line edit
//! to Bedrock's signer would recompile the spine and, through it, the engine, the
//! server, the renderer and the CLI. The dependency edge, not the code, is what
//! makes a provider edit expensive.
//!
//! So the edge is inverted. `zuno-llm` declares [`Provider`] and holds a map of
//! factories; each provider crate depends on `zuno-llm` and implements the trait;
//! `zuno-cli` — the one place that already depends on everything, because it is the
//! binary — owns both edges and does the wiring. A Bedrock edit now rebuilds
//! `zuno-provider-bedrock` and `zuno-cli`. Nothing else.
//!
//! `tests/registry_dependency_direction.rs` asserts the absent edge mechanically,
//! across the whole first-party transitive closure, so it cannot be re-added
//! quietly — not even through an intermediate crate. The technique of deleting a
//! reverse edge and leaving a note saying why comes from
//! `.omo/refs/jcode/crates/jcode-app-core/Cargo.toml:64-67`.
//!
//! # Wired in exactly one place
//!
//! Every registration lives in a single function in `zuno-cli`, whose signature is
//! [`Composition`]. One function means one place to read to learn what this build
//! can talk to, and one place a provider todo edits. The reference implementation
//! keeps the same discipline and says so in a comment
//! (`.omo/refs/jcode/src/cli/startup.rs:135-139`).
//!
//! # Where this improves on the reference
//!
//! The reference registry returns `Option<Arc<dyn Provider>>`, and `None` means
//! *either* "no factory registered" or "the factory declined"
//! (`.omo/refs/jcode/crates/jcode-base/src/provider/external.rs:219-232`). Its
//! caller then logs one warning for both — "the composition root must call
//! `register_external_provider()`" — so a user with no GitHub token is told the
//! program is miswired. Here the two are different [`RegistryError`] variants with
//! different messages, which is the entire reason for having two registration
//! forms.
//!
//! # Example
//!
//! ```
//! use zuno_llm::registry::{
//!     Capabilities, CompletionRequest, Declined, Provider, ProviderRegistry,
//!     ProviderStream, Spec, Unavailable,
//! };
//! use std::sync::Arc;
//!
//! #[derive(Debug)]
//! struct Fake(&'static str);
//!
//! impl Provider for Fake {
//!     fn id(&self) -> &str {
//!         self.0
//!     }
//!     fn capabilities(&self) -> Capabilities {
//!         Capabilities::text_only()
//!     }
//!     fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
//!         Box::pin(futures::stream::empty())
//!     }
//! }
//!
//! // The composition root: the one function that names provider implementations.
//! let mut registry = ProviderRegistry::new();
//! registry.register("anthropic", |_spec| Arc::new(Fake("anthropic")));
//! registry.register_fallible("github-copilot", |_spec| {
//!     Err(Declined::Unavailable(Unavailable::MissingCredential))
//! });
//!
//! // Wired and constructible.
//! assert_eq!(registry.resolve_key("anthropic")?.id(), "anthropic");
//!
//! // Wired, but not usable here. A user-facing state.
//! let unavailable = registry.resolve_key("github-copilot").unwrap_err();
//! assert!(!unavailable.is_wiring_bug());
//! assert!(unavailable.to_string().contains("unavailable"));
//!
//! // Never wired. A bug in this workspace, and the message says whose.
//! let missing = registry.resolve(Spec::new("bedrock")).unwrap_err();
//! assert!(missing.is_wiring_bug());
//! assert!(missing.to_string().contains("composition root"));
//! # Ok::<(), zuno_llm::registry::RegistryError>(())
//! ```

mod error;
mod provider;
mod spec;

pub use crate::registry::error::{Declined, FactoryOutcome, RegistryError, Unavailable};
pub use crate::registry::provider::{
    Capabilities, CompletionRequest, CredentialPresence, FinishReason, Message, Provider,
    ProviderStream, Role, StreamEvent, ToolSchema,
};
pub use crate::registry::spec::{ApiSurface, Spec, generation};

use std::collections::HashMap;
use std::sync::Arc;

/// A constructor for one provider, parameterized by its [`Spec`].
///
/// `Arc` rather than `Box` so the registry can be cloned and handed to concurrent
/// tasks without re-running the composition root. `Fn` rather than `FnOnce`
/// because a key may be resolved many times — once per model, or again after a
/// credential refresh.
pub type Factory = Arc<dyn Fn(Spec) -> FactoryOutcome + Send + Sync>;

/// The signature of the composition root.
///
/// `zuno-cli` implements this exactly once. Todos 29, 30, 94, 95 and 96 each add one
/// `register*` call inside that single implementation and change nothing else in
/// the workspace; naming the signature here is what lets each of them know what
/// they must satisfy before their crate exists.
pub type Composition = fn() -> ProviderRegistry;

/// The map from provider key to factory.
///
/// Owned rather than global. The reference implementation uses a process-wide
/// `OnceLock<RwLock<HashMap<..>>>` and documents that re-registering a key
/// replaces the previous factory "useful for tests"
/// (`.omo/refs/jcode/crates/jcode-base/src/provider/external.rs:184-195`) — which
/// is an admission that a global registry and a parallel test suite fight each
/// other. This workspace runs its tests in parallel, so an owned value that the
/// composition root builds and passes down is both safer and a truer expression of
/// the same inversion: nothing reaches for ambient state.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    factories: HashMap<&'static str, Factory>,
}

impl ProviderRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider that always constructs.
    ///
    /// The right form when a factory cannot fail: it reads nothing that might be
    /// missing, and any error it could hit belongs inside a request instead.
    ///
    /// Use [`register_fallible`](Self::register_fallible) when construction can
    /// decline. Collapsing the two is what makes "this user has not logged in"
    /// and "we forgot to wire this provider" look identical in a log, and only
    /// the second is a bug.
    ///
    /// Registering a key twice replaces the earlier factory. That is a real need —
    /// a test substitutes a fake for a family it is not exercising — and is safe
    /// here in a way it is not for a global registry, because the replacement is
    /// scoped to one owned value.
    pub fn register<F>(&mut self, provider: &'static str, factory: F)
    where
        F: Fn(Spec) -> Arc<dyn Provider> + Send + Sync + 'static,
    {
        self.register_fallible(provider, move |spec| Ok(factory(spec)));
    }

    /// Register a provider whose construction can decline or fail.
    ///
    /// A factory returns [`Declined::Unavailable`] when the provider is wired
    /// correctly but cannot run here — no stored credential, an unsupported
    /// platform, a half-filled configuration — and [`Declined::Failed`] when
    /// construction was attempted and broke. Neither is confusable with the key
    /// being absent, which is [`RegistryError::NotRegistered`].
    ///
    /// Unlike the reference implementation's `Option`-returning form, declining
    /// requires naming a reason. An unexplained decline is precisely what leaves a
    /// caller unable to tell a user what to do next.
    pub fn register_fallible<F>(&mut self, provider: &'static str, factory: F)
    where
        F: Fn(Spec) -> FactoryOutcome + Send + Sync + 'static,
    {
        self.factories.insert(provider, Arc::new(factory));
    }

    /// Construct the provider identity `spec` names with its selected factory.
    ///
    /// [`Spec::factory`] is the registry key and [`Spec::provider`] remains the
    /// identity passed to the factory. They default to the same value, while a
    /// shared wire family can select one factory for several behavioral profiles.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotRegistered`] when no factory is registered — a wiring
    /// bug, and the message names the key and the function that must be called.
    /// [`RegistryError::Unavailable`] when a factory declined, and
    /// [`RegistryError::Construction`] when one failed.
    pub fn resolve(&self, spec: Spec) -> Result<Arc<dyn Provider>, RegistryError> {
        let factory_key = spec.factory().to_owned();
        let Some(factory) = self.factories.get(factory_key.as_str()).cloned() else {
            return Err(RegistryError::NotRegistered {
                provider: factory_key,
            });
        };
        let provider = spec.provider.clone();
        factory(spec).map_err(|declined| declined.into_error(&provider))
    }

    /// Construct `provider` with no parameters.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve).
    pub fn resolve_key(&self, provider: &str) -> Result<Arc<dyn Provider>, RegistryError> {
        self.resolve(Spec::new(provider))
    }

    /// Whether a factory is registered for `provider`.
    ///
    /// For a caller deciding whether to *offer* a provider. A caller that wants to
    /// *use* one should call [`resolve`](Self::resolve) and read the error, which
    /// distinguishes three outcomes this predicate flattens into two.
    #[must_use]
    pub fn is_registered(&self, provider: &str) -> bool {
        self.factories.contains_key(provider)
    }

    /// Every registered key, sorted.
    ///
    /// Sorted so a diagnostic listing them is stable across runs; `HashMap`
    /// iteration order is not.
    #[must_use]
    pub fn registered(&self) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = self.factories.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// How many factories are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.factories.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }

    /// Every provider that has a credential stored and no factory to use it.
    ///
    /// This is the composition-root audit. Each returned
    /// [`RegistryError::NotRegistered`] names a provider the user has authenticated
    /// with and that this build silently cannot reach — a wiring bug that is
    /// otherwise invisible, because the provider simply never appears and nothing
    /// errors.
    ///
    /// `candidates` are the provider keys worth checking, which the catalog
    /// supplies (todo 26); the registry has no opinion about which providers exist
    /// in the world, only about which ones it can build.
    ///
    /// Credential presence arrives through [`CredentialPresence`] rather than a
    /// dependency on `zuno-auth`, for the same reason the factories arrive through a
    /// map: the spine names capabilities, not implementations.
    #[must_use]
    pub fn unwired(
        &self,
        credentials: &dyn CredentialPresence,
        candidates: &[&str],
    ) -> Vec<RegistryError> {
        let mut missing: Vec<&str> = candidates
            .iter()
            .copied()
            .filter(|provider| {
                credentials.has_credential(provider) && !self.is_registered(provider)
            })
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
            .into_iter()
            .map(|provider| RegistryError::NotRegistered {
                provider: provider.to_owned(),
            })
            .collect()
    }
}

/// Lists the registered keys rather than the factories, which are closures and
/// have nothing to render.
impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("registered", &self.registered())
            .finish()
    }
}
