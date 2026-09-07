//! The resolved catalog: what a model looks like after models.dev, config, env
//! and auth have all had their say.
//!
//! These are the model facts every downstream consumer reads: the picker, agent
//! model policy, and each provider family.
//!
//! - **`api` is resolved native transport metadata, not the external catalog's
//!   optionals.** models.dev package names are translated while importing the
//!   catalog. By the time a model reaches a provider crate, `api.id`,
//!   `api.transport`, and `api.url` are populated without exposing a JavaScript
//!   package as a runtime choice.
//! - **`capabilities` is booleans, not the catalog's arrays.** `modalities.input`
//!   is a list upstream and five flags here, because that is what the oracle
//!   flattens it to (`provider.ts:1465-1481`) and what a caller actually asks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zuno_config::schema::provider::{ProviderRetryConfig, ProviderTransport};

use crate::catalog::availability::Availability;
use crate::catalog::models_dev::{CatalogStatus, Interleaved};

/// A free-form JSON object, as the oracle's `options`/`headers`/variant bags are.
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

/// One resolved provider and every model the user may select from it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedProvider {
    /// The provider key, which is also its id.
    pub id: String,
    /// Human-readable name, config-overridable.
    pub name: String,
    /// Environment variables that make this provider available.
    pub env: Vec<String>,
    /// Provider-level SDK options, config-merged.
    pub options: JsonMap,
    /// Zuno-owned same-request retry policy for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<ProviderRetryConfig>,
    /// How this provider came to be available.
    pub availability: Availability,
    /// Model id to model. Sorted; output order comes from
    /// [`crate::catalog::collate`], not from this map.
    pub models: BTreeMap<String, ResolvedModel>,
}

/// One resolved model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedModel {
    /// The id the user selects by. May differ from [`ModelApi::id`].
    pub id: String,
    /// The provider this model belongs to.
    pub provider_id: String,
    /// Human-readable name.
    pub name: String,
    /// Model family.
    #[serde(default)]
    pub family: String,
    /// Release date, `YYYY-MM-DD`, or empty.
    pub release_date: String,
    /// Lifecycle status, defaulted to [`CatalogStatus::Active`].
    pub status: CatalogStatus,
    /// Resolved transport metadata: wire id, native transport, base URL and endpoint hint.
    pub api: ModelApi,
    /// Flattened capability flags.
    pub capabilities: ModelCapabilities,
    /// Pricing, flattened to the oracle's `{input, output, cache{read,write}}`.
    pub cost: ModelCost,
    /// Context and output ceilings.
    pub limit: ModelLimit,
    /// Per-model SDK options.
    pub options: JsonMap,
    /// Per-model request headers.
    pub headers: BTreeMap<String, String>,
    /// Named alternate configurations, `disabled` ones already removed.
    #[serde(default)]
    pub variants: BTreeMap<String, JsonMap>,
}

/// Where and how to reach a model — `provider.ts:965-969`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelApi {
    /// The id to put on the wire.
    pub id: String,
    /// Native provider transport, or `None` when an external catalog entry names
    /// a protocol this build does not implement.
    pub transport: Option<ProviderTransport>,
    /// The base URL, possibly containing `${VAR}` placeholders.
    pub url: String,
    /// A model-advertised SDK surface, when its provider reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<ModelEndpoint>,
}

/// A model-advertised SDK surface.
///
/// GitHub Copilot reports this independently of the model id. Keeping the three
/// values closed prevents arbitrary endpoint paths from entering surface selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelEndpoint {
    /// Chat completions.
    Chat,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages.
    Messages,
}

/// Flattened capabilities — `provider.ts:991-1000`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Honours a temperature parameter.
    pub temperature: bool,
    /// Produces reasoning output.
    pub reasoning: bool,
    /// Accepts attachments.
    pub attachment: bool,
    /// Supports tool calls. Note the oracle's default here is `true`.
    pub toolcall: bool,
    /// Accepted media types.
    pub input: ModalityFlags,
    /// Produced media types.
    pub output: ModalityFlags,
    /// Where interleaved reasoning arrives, if anywhere.
    pub interleaved: Interleaved,
}

/// The catalog's modality array, flattened to flags — `provider.ts:1465-1481`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalityFlags {
    /// Text.
    pub text: bool,
    /// Audio.
    pub audio: bool,
    /// Still images.
    pub image: bool,
    /// Video.
    pub video: bool,
    /// PDF documents.
    pub pdf: bool,
}

impl Default for ModalityFlags {
    /// Text on, everything else off — the oracle's defaults when the catalog
    /// declares no modalities (`provider.ts:1466`, `:1473`).
    fn default() -> Self {
        Self {
            text: true,
            audio: false,
            image: false,
            video: false,
            pdf: false,
        }
    }
}

/// Pricing, flattened — `provider.ts:1489-1496`.
///
/// Every field is a plain `f64` with a zero default because that is what the
/// oracle coerces a missing price to. A missing price and a free model are
/// therefore indistinguishable downstream, which is upstream's choice, not this
/// crate's; preserving the distinction here would diverge from every cost
/// calculation the rest of the program does.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCost {
    /// Input price per million tokens.
    pub input: f64,
    /// Output price per million tokens.
    pub output: f64,
    /// Cache pricing.
    pub cache: CacheCost,
}

impl ModelCost {
    /// Price per token is per *million* tokens in the catalog, as the oracle stores it.
    const PER_MILLION: f64 = 1_000_000.0;

    /// What one request costs, given its **disjoint** token buckets.
    ///
    /// Every argument must count tokens no other argument counts: `input` is the
    /// prompt minus both cache figures, and `output` is the generated tokens minus
    /// `reasoning`. `PromptAccounting::uncached_input` produces the first;
    /// `StreamEvent::TokenUsage` documents the second. Passing a provider's raw
    /// totals instead charges the cached prompt and the reasoning twice.
    ///
    /// `reasoning` is billed at the output rate rather than at one of its own:
    /// OpenAI states that reasoning tokens "are billed as output tokens", and no
    /// vendor in this catalog publishes a separate reasoning price. It is a distinct
    /// argument regardless, because the caller's buckets are disjoint and folding it
    /// into `output` at the call site is exactly the kind of quiet arithmetic this
    /// signature exists to prevent.
    ///
    /// A missing price is a zero here, which the type's documentation already owns:
    /// upstream coerces an absent price to zero, so a free model and an unpriced one
    /// are indistinguishable. This returns `0.0` for both rather than pretending to
    /// know which it met.
    #[must_use]
    pub fn charge(
        self,
        input: u64,
        output: u64,
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> f64 {
        let priced = |tokens: u64, per_million: f64| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "token counts are far below f64's exact-integer range"
            )]
            let tokens = tokens as f64;
            tokens * per_million / Self::PER_MILLION
        };
        priced(input, self.input)
            + priced(output.saturating_add(reasoning), self.output)
            + priced(cache_read, self.cache.read)
            + priced(cache_write, self.cache.write)
    }
}

/// Cache pricing — `provider.ts:1492-1495`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheCost {
    /// Cache-read price per million tokens.
    pub read: f64,
    /// Cache-write price per million tokens.
    pub write: f64,
}

/// Context and output ceilings — `provider.ts:1498-1502`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelLimit {
    /// Maximum provider-visible prompt/input tokens used for runtime budgeting.
    pub context: f64,
    /// Maximum input tokens, when it differs from the context window.
    pub input: Option<f64>,
    /// Maximum output tokens.
    pub output: f64,
}

impl Default for Interleaved {
    /// `false` — the oracle's default when nothing declares interleaving
    /// (`provider.ts:1487`).
    fn default() -> Self {
        Self::Flag(false)
    }
}

impl ResolvedProvider {
    /// True when this provider has at least one selectable model.
    ///
    /// The oracle drops a provider with zero models entirely
    /// (`provider.ts:1654-1657`), so an over-aggressive blacklist removes the
    /// provider rather than leaving an empty entry in the picker.
    #[must_use]
    pub fn has_models(&self) -> bool {
        !self.models.is_empty()
    }
}

impl ResolvedModel {
    /// The `provider/model` line `opencode models` prints.
    #[must_use]
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Sonnet's published rates, in dollars per million tokens.
    const SONNET: ModelCost = ModelCost {
        input: 3.0,
        output: 15.0,
        cache: CacheCost {
            read: 0.3,
            write: 3.75,
        },
    };

    /// Floating-point money compares to a tolerance, not to a bit pattern.
    fn assert_dollars(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected ${expected}, got ${actual}"
        );
    }

    #[test]
    fn each_bucket_is_priced_at_its_own_rate() {
        // 1000 uncached prompt, 100 visible answer, 900 reasoning, 10000 read, 2000
        // written.
        assert_dollars(
            SONNET.charge(1_000, 100, 900, 10_000, 2_000),
            0.003 + 0.015 + 0.003 + 0.007_5,
        );
    }

    /// Reasoning is output, priced at the output rate.
    ///
    /// OpenAI bills reasoning tokens as output tokens and no vendor in this catalog
    /// publishes a separate reasoning price, so the split is for accounting clarity
    /// rather than for a different rate.
    #[test]
    fn reasoning_costs_exactly_what_the_same_visible_output_would() {
        assert_dollars(
            SONNET.charge(0, 1_000, 0, 0, 0),
            SONNET.charge(0, 0, 1_000, 0, 0),
        );
        assert_dollars(
            SONNET.charge(0, 400, 600, 0, 0),
            SONNET.charge(0, 1_000, 0, 0, 0),
        );
    }

    /// A cache read is far cheaper than the same tokens uncached.
    ///
    /// The reason the prompt side must arrive already normalized: charging 10000
    /// cached tokens at the input rate instead of the cache-read rate is a tenfold
    /// overcharge on this model.
    #[test]
    fn a_cached_prompt_is_not_priced_as_an_uncached_one() {
        let cached = SONNET.charge(0, 0, 0, 10_000, 0);
        let uncached = SONNET.charge(10_000, 0, 0, 0, 0);
        assert!(
            cached < uncached,
            "a cache read must be cheaper: ${cached} vs ${uncached}"
        );
        assert_dollars(cached, 0.003);
        assert_dollars(uncached, 0.03);
    }

    /// An unpriced model charges nothing rather than guessing.
    ///
    /// The type's own documentation owns this: upstream coerces a missing price to
    /// zero, so a free model and an unpriced one are indistinguishable here.
    #[test]
    fn a_model_with_no_published_prices_costs_nothing() {
        assert_dollars(ModelCost::default().charge(4_210, 86, 100, 1_024, 64), 0.0);
    }

    #[test]
    fn a_request_that_spent_nothing_costs_nothing() {
        assert_dollars(SONNET.charge(0, 0, 0, 0, 0), 0.0);
    }
}
