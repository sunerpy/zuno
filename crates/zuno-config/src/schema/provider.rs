//! Provider and model configuration.
//!
//! Oracle: `packages/core/src/v1/config/provider.ts:6-127`.
//!
//! `Schema.Finite` maps to `f64`: JSON has a single number type and these fields
//! (cost, limits, temperature) are read as numbers, not as integers.

use crate::schema::JsonMap;
use crate::schema::ordered::{False, OrderedMap};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::num::NonZeroU32;

/// One entry of the `provider` map (`config/provider.ts:82-126`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Base API URL for the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Environment variables that supply this provider's credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    /// Provider id override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// npm package implementing the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// Models to keep, to the exclusion of the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whitelist: Option<Vec<String>>,
    /// Models to drop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blacklist: Option<Vec<String>>,
    /// Provider-level options, including SDK options this schema does not name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ProviderOptions>,
    /// Per-model configuration and overrides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<OrderedMap<ModelConfig>>,
}

/// Provider options (`config/provider.ts:90-124`).
///
/// The oracle spells this `StructWithRest(..., [Record(String, Any)])`, so any key
/// the schema does not name is still valid and is handed to the provider SDK. That
/// rest record is [`ProviderOptions::extra`].
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProviderOptions {
    /// API key.
    #[serde(rename = "apiKey", skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Base URL override.
    #[serde(rename = "baseURL", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// GitHub Enterprise URL, for copilot authentication.
    #[serde(rename = "enterpriseUrl", skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
    /// Send `promptCacheKey` for this provider (default false).
    #[serde(rename = "setCacheKey", skip_serializing_if = "Option::is_none")]
    pub set_cache_key: Option<bool>,
    /// Whole-request timeout in milliseconds, or `false` for none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Timeout>,
    /// Response-header timeout in milliseconds, or `false` for none.
    #[serde(rename = "headerTimeout", skip_serializing_if = "Option::is_none")]
    pub header_timeout: Option<Timeout>,
    /// Maximum gap between streamed SSE chunks, in milliseconds.
    #[serde(rename = "chunkTimeout", skip_serializing_if = "Option::is_none")]
    pub chunk_timeout: Option<NonZeroU32>,
    /// Every other option, passed through to the provider SDK.
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// A timeout in milliseconds, or `false` to disable it
/// (`config/provider.ts:101-116`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Timeout {
    /// Milliseconds to wait.
    Millis(NonZeroU32),
    /// The literal `false`.
    Disabled(False),
}

/// A model's lifecycle status (`config/provider.ts:6`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    /// Alpha.
    Alpha,
    /// Beta.
    Beta,
    /// Deprecated.
    Deprecated,
    /// Active.
    Active,
}

/// A modality a model accepts or emits (`config/provider.ts:56-59`).
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    /// Text.
    Text,
    /// Audio.
    Audio,
    /// Image.
    Image,
    /// Video.
    Video,
    /// PDF.
    Pdf,
}

/// Interleaved-reasoning configuration (`config/provider.ts:22-30`).
///
/// The oracle's field arm is `Union([Literals([...]), String])`, which accepts any
/// string; the three literals are documentation, not a constraint.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Interleaved {
    /// Switch interleaved reasoning on or off.
    Enabled(bool),
    /// The stream field carrying reasoning.
    Field(String),
    /// The same, spelled as an object.
    Wrapped(InterleavedField),
}

/// The object arm of [`Interleaved`] (`config/provider.ts:26-28`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterleavedField {
    /// The stream field carrying reasoning.
    pub field: String,
}

/// Per-token pricing (`config/provider.ts:31-46`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    /// Input price.
    pub input: f64,
    /// Output price.
    pub output: f64,
    /// Cache-read price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Pricing that takes over past a 200k-token context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<ModelCostTier>,
}

/// The long-context pricing tier (`config/provider.ts:37-44`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCostTier {
    /// Input price.
    pub input: f64,
    /// Output price.
    pub output: f64,
    /// Cache-read price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Token limits (`config/provider.ts:47-53`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelLimit {
    /// Total context window.
    pub context: f64,
    /// Maximum input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// Maximum output tokens.
    pub output: f64,
}

/// Input and output modalities (`config/provider.ts:54-61`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelModalities {
    /// Accepted modalities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<Modality>>,
    /// Emitted modalities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<Modality>>,
}

/// The npm package and API endpoint backing a single model
/// (`config/provider.ts:64-66`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelProvider {
    /// npm package implementing the provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// API endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// One named variant of a model (`config/provider.ts:69-79`).
///
/// `StructWithRest`: everything but `disabled` is variant-specific payload.
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelVariant {
    /// Disable this variant for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Everything else the variant carries.
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// One entry of a provider's `models` map (`config/provider.ts:13-80`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model id override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Model family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Release date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// Whether the model accepts attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<bool>,
    /// Whether the model reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// Whether the model honours `temperature`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
    /// Whether the model can call tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
    /// Interleaved-reasoning configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<Interleaved>,
    /// Pricing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
    /// Token limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<ModelLimit>,
    /// Input and output modalities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ModelModalities>,
    /// Whether the model is experimental.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental: Option<bool>,
    /// Lifecycle status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ModelStatus>,
    /// The npm package and API endpoint backing this model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ModelProvider>,
    /// Model options handed to the provider SDK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<JsonMap>,
    /// Extra headers for requests to this model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Named variants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<OrderedMap<ModelVariant>>,
}
