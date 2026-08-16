//! The resolved catalog: what a model looks like after models.dev, config, env
//! and auth have all had their say.
//!
//! These are ports of `Provider.Info` and `Provider.Model`
//! (`packages/opencode/src/provider/provider.ts:1036-1062`), which is the shape
//! `opencode models --verbose` prints and therefore the shape every consumer
//! downstream of this crate — the picker, the agent model policy, each provider
//! family — reads. Two things are worth naming about it:
//!
//! - **`api` is resolved transport metadata, not the catalog's optionals.** The catalog has
//!   an optional provider-level `api`/`npm` and an optional per-model override;
//!   by the time a model reaches a provider crate the choice is already made and
//!   `api.id`/`api.npm`/`api.url` are populated. A plugin-advertised endpoint stays
//!   optional because the model-id rule remains the fallback when it is absent.
//! - **`capabilities` is booleans, not the catalog's arrays.** `modalities.input`
//!   is a list upstream and five flags here, because that is what the oracle
//!   flattens it to (`provider.ts:1465-1481`) and what a caller actually asks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
    /// Resolved transport metadata: wire id, npm package, base URL and endpoint hint.
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
    /// The npm package whose factory speaks this model's protocol.
    pub npm: String,
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
    /// Total context window in tokens.
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
