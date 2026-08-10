//! Resolving the catalog: which models the user can actually select.
//!
//! # What this decides
//!
//! Everything downstream — the model picker, the agent model policy (todo 64),
//! all five provider families (todos 29/30/94/95/96) — asks this module what
//! exists. Resolve it differently from the TypeScript binary and the *same* config
//! on the *same* machine yields a different model list, up to and including
//! dropping the model the user actually runs on. So this is a port, not a design:
//! `packages/core/src/models-dev.ts` for the source and cache,
//! `packages/opencode/src/provider/provider.ts:1332-1658` for the merge.
//!
//! # The pipeline, in the oracle's order
//!
//! Order is not cosmetic. Each stage can undo the previous one, so running them
//! out of order changes the answer:
//!
//! 1. **Load** the models.dev document — [`source::CatalogSource`]. Three env vars
//!    decide where from, and whether the network may be touched at all. The
//!    document may legitimately be **empty**: with fetching disabled and no cache,
//!    the oracle returns `{}` rather than failing (`models-dev.ts:222`), and stage 3
//!    is what makes a self-contained `provider.*` block work anyway.
//! 2. **Lift** every catalog provider into resolved shape, expanding
//!    `experimental.modes` into their own model ids — [`merge::from_catalog`].
//! 3. **Extend** from `provider.*` config, which may add models, add whole
//!    providers the catalog has never heard of, and override any field —
//!    [`merge::apply_config`] (`provider.ts:1425-1520`).
//! 4. **Establish availability** from three independent sources: env vars, stored
//!    auth, and the presence of a config block — [`availability`]
//!    (`provider.ts:1523-1595`).
//! 5. **Filter**: `disabled_providers`/`enabled_providers`, then per-model status,
//!    blacklist and whitelist, then drop any provider left with no models
//!    (`provider.ts:1611-1658`).
//!
//! Stage 5 running last is load-bearing: a blacklist that removes every model
//! removes the *provider*, so a provider can be available and still absent.
//! Verified against 1.18.12 — `zhipuai` with `blacklist: ["glm-5"]` and a pinned
//! one-model catalog vanished from `opencode models` entirely.
//!
//! # Parity, and where the plan is wrong about how to check it
//!
//! Todo 26's acceptance criterion names `opencode models --format json`. **That
//! flag does not exist.** `opencode models --help` on 1.18.12 lists exactly
//! `--verbose` and `--refresh`, and passing `--format json` prints the help text
//! instead of a catalog. `models.ts:36-47` writes plain `provider/model` lines,
//! one per model, and `--verbose` interleaves pretty-printed JSON. So the
//! differential in `tests/catalog_differential.rs` compares against
//! `opencode models`, which is the same list the criterion means.
//!
//! `--verbose` is deliberately *not* the differential target: its JSON key order
//! differs between a catalog-derived model and a config-derived one, because the
//! two are built by different code paths in the oracle (a spread-merge versus an
//! object literal). Diffing it would fail on key order, which says nothing about
//! whether the right models resolved.
//!
//! # No network in any test
//!
//! Every test drives both sides from a pinned fixture through
//! `OPENCODE_MODELS_PATH`. A differential that depends on models.dev being
//! reachable is a flaky test and a false claim of determinism.

pub mod availability;
pub mod collate;
pub mod error;
pub mod merge;
pub mod models_dev;
pub mod resolved;
pub mod source;

use std::collections::{BTreeMap, BTreeSet};

use oc_auth::Credential;
use oc_config::schema::Config;
use oc_config::schema::provider::ProviderConfig;

pub use crate::catalog::availability::{Availability, AvailabilitySource};
pub use crate::catalog::error::CatalogError;
pub use crate::catalog::merge::MergeOutcome;
pub use crate::catalog::models_dev::{CatalogDocument, CatalogProvider, CatalogStatus};
pub use crate::catalog::resolved::{ResolvedModel, ResolvedProvider};
pub use crate::catalog::source::{CatalogProvenance, CatalogSource, LoadedCatalog};

/// Everything resolution needs that is not the catalog document itself.
///
/// A struct of borrowed inputs rather than a set of positional arguments, because
/// the three availability sources are independent and a positional API invites a
/// caller to pass two of the three and not notice. Every field has a `with_`
/// builder so a test can state exactly one source.
#[derive(Debug, Default)]
pub struct ResolveInput<'a> {
    config: Option<&'a Config>,
    credentials: BTreeMap<String, Credential>,
    env: BTreeMap<String, String>,
    experimental_models: bool,
}

impl<'a> ResolveInput<'a> {
    /// An empty input: no config, no credentials, no environment.
    ///
    /// Resolving with this yields an empty catalog, which is correct — verified
    /// against 1.18.12, whose `opencode models` printed nothing under an isolated
    /// `HOME` with a pinned catalog and nothing else set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Supply the user's config.
    #[must_use]
    pub fn with_config(mut self, config: &'a Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Supply stored credentials, keyed by provider id.
    #[must_use]
    pub fn with_credentials(mut self, credentials: BTreeMap<String, Credential>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Supply the environment the provider `env` lists are checked against.
    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Supply one environment variable.
    #[must_use]
    pub fn with_env_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Include `alpha` models, as the experimental runtime flag does.
    #[must_use]
    pub const fn with_experimental_models(mut self, enabled: bool) -> Self {
        self.experimental_models = enabled;
        self
    }
}

/// The resolved catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    providers: BTreeMap<String, ResolvedProvider>,
    outcome: MergeOutcome,
}

impl Catalog {
    /// Resolve a catalog document against config, env and stored auth.
    ///
    /// Synchronous on purpose: loading the document is the only part that can
    /// touch the network, and it lives in [`CatalogSource::load`]. Splitting them
    /// is what lets every merge and availability test run with no I/O at all, and
    /// what makes "do not fetch at startup when the flag is set" a property of one
    /// small function rather than of this whole pipeline.
    #[must_use]
    pub fn resolve(document: &CatalogDocument, input: &ResolveInput<'_>) -> Self {
        let mut outcome = MergeOutcome::default();
        let empty_config = Config::default();
        let config = input.config.unwrap_or(&empty_config);

        // Stage 2: lift the catalog.
        let mut providers: BTreeMap<String, ResolvedProvider> = document
            .iter()
            .map(|(id, provider)| (id.clone(), merge::from_catalog(id, provider)))
            .collect();

        // Stage 3: extend from config. A provider the catalog does not know is a
        // supported case, not an error.
        let config_providers: BTreeMap<&str, &ProviderConfig> = config
            .provider
            .as_ref()
            .map(|map| map.iter().collect())
            .unwrap_or_default();
        for (provider_id, provider_config) in &config_providers {
            let existing = providers.get(*provider_id).cloned();
            let merged = merge::apply_config(
                provider_id,
                provider_config,
                existing.as_ref(),
                document.get(*provider_id),
                &mut outcome,
            );
            providers.insert((*provider_id).to_owned(), merged);
        }

        // Stage 4: availability, one independent source at a time.
        for (provider_id, provider) in &mut providers {
            let mut found = Availability::none();
            let lookup = |name: &str| input.env.get(name).cloned();
            if let Some(source) = availability::env_var_source(&provider.env, &lookup) {
                found.record(source);
            }
            if let Some(credential) = input.credentials.get(provider_id) {
                found.record(availability::credential_source(credential));
            }
            if config_providers.contains_key(provider_id.as_str()) {
                found.record(AvailabilitySource::ConfigBlock);
            }
            provider.availability = found;
        }

        // Stage 5: filtering, in the oracle's order.
        let disabled: BTreeSet<&str> = config
            .disabled_providers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect();
        let enabled: Option<BTreeSet<&str>> = config
            .enabled_providers
            .as_ref()
            .map(|list| list.iter().map(String::as_str).collect());

        providers.retain(|provider_id, provider| {
            if disabled.contains(provider_id.as_str()) {
                return false;
            }
            if let Some(enabled) = &enabled
                && !enabled.contains(provider_id.as_str())
            {
                return false;
            }
            provider.availability.is_available()
        });

        for (provider_id, provider) in &mut providers {
            merge::filter_models(
                provider,
                config_providers.get(provider_id.as_str()).copied(),
                input.experimental_models,
            );
        }
        // A provider with nothing selectable is not a provider — `:1654-1657`.
        providers.retain(|_, provider| provider.has_models());

        Self { providers, outcome }
    }

    /// Every resolved provider, keyed by id.
    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, ResolvedProvider> {
        &self.providers
    }

    /// One provider by id.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ResolvedProvider> {
        self.providers.get(id)
    }

    /// One provider by id, for a production extension loader that mutates it.
    pub fn provider_mut(&mut self, id: &str) -> Option<&mut ResolvedProvider> {
        self.providers.get_mut(id)
    }

    /// Replace the models of an already-resolved provider from a plugin hook.
    pub fn replace_provider_models(
        &mut self,
        id: &str,
        models: BTreeMap<String, ResolvedModel>,
    ) -> bool {
        let Some(provider) = self.providers.get_mut(id) else {
            return false;
        };
        provider.models = models;
        if provider.models.is_empty() {
            self.providers.remove(id);
        }
        true
    }

    /// One model by provider and model id.
    #[must_use]
    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<&ResolvedModel> {
        self.providers.get(provider_id)?.models.get(model_id)
    }

    /// Seams this resolution deliberately left for a later todo.
    #[must_use]
    pub const fn outcome(&self) -> &MergeOutcome {
        &self.outcome
    }

    /// Provider ids in the order `opencode models` lists them.
    ///
    /// `opencode*` first, then [`collate::compare`] — `models.ts:56-62`.
    #[must_use]
    pub fn provider_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.providers.keys().map(String::as_str).collect();
        ids.sort_by(|left, right| collate::compare_provider_ids(left, right));
        ids
    }

    /// Every model as `provider/model`, in the order `opencode models` prints.
    ///
    /// This is the differential target: byte-identical to the real binary's
    /// stdout, one line per model.
    #[must_use]
    pub fn model_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for provider_id in self.provider_ids() {
            let Some(provider) = self.providers.get(provider_id) else {
                continue;
            };
            let mut model_ids: Vec<&str> = provider.models.keys().map(String::as_str).collect();
            model_ids.sort_by(|left, right| collate::compare(left, right));
            for model_id in model_ids {
                lines.push(format!("{provider_id}/{model_id}"));
            }
        }
        lines
    }

    /// Providers that have a credential but are not selectable, with the reason.
    ///
    /// The reasons are [`crate::registry::Unavailable`], never
    /// `RegistryError::NotRegistered`: whether a provider is *wired into this
    /// build* is a fact about `oc-cli`'s composition root and is unknowable from a
    /// catalog, a config file and `auth.json`. Keeping the two apart is what stops
    /// a user with no API key from being told the program is miswired.
    #[must_use]
    pub fn unavailable(
        document: &CatalogDocument,
        input: &ResolveInput<'_>,
    ) -> BTreeMap<String, crate::registry::Unavailable> {
        let resolved = Self::resolve(document, input);
        let mut out = BTreeMap::new();
        for provider_id in input.credentials.keys() {
            if resolved.providers.contains_key(provider_id) {
                continue;
            }
            let mut found = Availability::none();
            if let Some(credential) = input.credentials.get(provider_id) {
                found.record(availability::credential_source(credential));
            }
            if let Some(reason) = found.unavailable_reason() {
                out.insert(provider_id.clone(), reason);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> CatalogDocument {
        serde_json::from_str(
            r#"{
              "deepseek": {"name":"DeepSeek","id":"deepseek","env":["DEEPSEEK_API_KEY"],
                "npm":"@ai-sdk/openai-compatible","api":"https://api.deepseek.com",
                "models":{"deepseek-chat":{"id":"deepseek-chat","name":"Chat",
                  "limit":{"context":1,"output":1}}}},
              "groq": {"name":"Groq","id":"groq","env":["GROQ_API_KEY"],
                "npm":"@ai-sdk/groq",
                "models":{"allam-2-7b":{"id":"allam-2-7b","name":"Allam",
                  "limit":{"context":1,"output":1}}}}
            }"#,
        )
        .expect("fixture parses")
    }

    fn config(json: &str) -> Config {
        serde_json::from_str(json).expect("config parses")
    }

    #[test]
    fn nothing_configured_resolves_to_nothing() {
        let catalog = Catalog::resolve(&document(), &ResolveInput::new());
        assert!(
            catalog.model_lines().is_empty(),
            "an empty environment must not autoload providers"
        );
    }

    #[test]
    fn a_provider_with_every_model_blacklisted_disappears_entirely() {
        // `:1654-1657`. The provider is available; it still must not be listed.
        let cfg = config(r#"{"provider":{"deepseek":{"blacklist":["deepseek-chat"]}}}"#);
        let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
        assert!(catalog.provider("deepseek").is_none());
        assert!(catalog.model_lines().is_empty());
    }

    #[test]
    fn disabled_providers_wins_over_a_config_block() {
        let cfg =
            config(r#"{"disabled_providers":["deepseek"],"provider":{"deepseek":{},"groq":{}}}"#);
        let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
        assert_eq!(catalog.model_lines(), vec!["groq/allam-2-7b"]);
    }

    #[test]
    fn enabled_providers_is_an_allow_list() {
        let cfg = config(r#"{"enabled_providers":["groq"],"provider":{"deepseek":{},"groq":{}}}"#);
        let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
        assert_eq!(catalog.model_lines(), vec!["groq/allam-2-7b"]);
    }

    #[test]
    fn opencode_providers_lead_the_listing() {
        let mut doc = document();
        let opencode: CatalogProvider = serde_json::from_str(
            r#"{"name":"opencode","id":"opencode","env":["OPENCODE_API_KEY"],
                "models":{"zen":{"id":"zen","name":"Zen","limit":{"context":1,"output":1}}}}"#,
        )
        .expect("fixture parses");
        doc.insert("opencode".to_owned(), opencode);
        let cfg = config(r#"{"provider":{"deepseek":{},"groq":{},"opencode":{}}}"#);
        let catalog = Catalog::resolve(&doc, &ResolveInput::new().with_config(&cfg));
        assert_eq!(
            catalog.model_lines(),
            vec!["opencode/zen", "deepseek/deepseek-chat", "groq/allam-2-7b"]
        );
    }

    #[test]
    fn a_credentialed_but_unlisted_provider_reports_unavailable_not_unregistered() {
        use oc_auth::Secret;
        let mut credentials = BTreeMap::new();
        credentials.insert(
            "deepseek".to_owned(),
            Credential::Oauth {
                refresh: Secret::new("r"),
                access: Secret::new("a"),
                expires: 0,
                account_id: None,
                enterprise_url: None,
            },
        );
        let input = ResolveInput::new().with_credentials(credentials);
        let reasons = Catalog::unavailable(&document(), &input);
        assert_eq!(
            reasons.get("deepseek"),
            Some(&crate::registry::Unavailable::IncompleteConfiguration)
        );
    }
}
