//! The models.dev document, exactly as it arrives.
//!
//! Ported from `packages/core/src/models-dev.ts:15-132`, which is the schema the
//! real binary validates the fetched document against. Field names, optionality
//! and the union shapes are all load-bearing: this crate reads the *same file* the
//! oracle reads, so a field typed too narrowly here turns a working catalog into
//! a parse error, and a field typed too loosely silently drops a capability.
//!
//! Two deliberate looseness decisions, both because the catalog is a live remote
//! document this workspace does not control:
//!
//! - Unknown keys are **ignored**, not rejected. models.dev adds fields; a new
//!   one must not take the user's model list away. (The user's *config* is the
//!   opposite case and `oc-config` rejects unknown keys there, correctly — a typo
//!   in a hand-written file is a mistake worth reporting.)
//! - `status` and modality enums keep an `Other` escape hatch for values this
//!   build has not seen, so an unrecognised status degrades to "not filtered"
//!   rather than to "catalog unparseable".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The whole document: provider id to provider.
///
/// A [`BTreeMap`] rather than an insertion-ordered map because output order is
/// decided by [`crate::catalog::collate`], never by the file, and a sorted map
/// makes that independence structural.
pub type CatalogDocument = BTreeMap<String, CatalogProvider>;

/// One provider entry — `models-dev.ts:123-130`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvider {
    /// The base URL for the provider's API, when the catalog knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Human-readable provider name.
    pub name: String,
    /// Environment variables whose presence means "the user has a key for this".
    #[serde(default)]
    pub env: Vec<String>,
    /// The provider's own id, which normally equals its key in the document.
    pub id: String,
    /// The npm package implementing the wire protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// Model id to model.
    #[serde(default)]
    pub models: BTreeMap<String, CatalogModel>,
}

/// One model entry — `models-dev.ts:67-120`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// The id to send on the wire, which may differ from the catalog key.
    pub id: String,
    /// Human-readable model name.
    pub name: String,
    /// Model family, used for grouping in the picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Release date, `YYYY-MM-DD`.
    #[serde(default)]
    pub release_date: String,
    /// Accepts file attachments.
    #[serde(default)]
    pub attachment: bool,
    /// Produces reasoning output.
    #[serde(default)]
    pub reasoning: bool,
    /// Honours a temperature parameter.
    #[serde(default)]
    pub temperature: bool,
    /// Supports tool calls.
    #[serde(default)]
    pub tool_call: bool,
    /// Where interleaved reasoning arrives, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<Interleaved>,
    /// Per-token pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CatalogCost>,
    /// Context and output ceilings.
    #[serde(default)]
    pub limit: CatalogLimit,
    /// Accepted and produced media types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<CatalogModalities>,
    /// Alternate wire configurations exposed as separate model ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<CatalogExperimental>,
    /// Lifecycle status, which decides whether the model is listed at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CatalogStatus>,
    /// Per-model override of the provider's npm package or API URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<CatalogModelProvider>,
}

/// `models-dev.ts:117-119` — a model overriding its provider's transport.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogModelProvider {
    /// npm package for this model specifically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// API base URL for this model specifically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// `models-dev.ts:87-91`.
///
/// `context` and `output` default to zero rather than being required: the oracle
/// coerces a missing limit to `0` when merging (`provider.ts:1499-1501`), and a
/// catalog entry that omits the block must not fail the whole document.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogLimit {
    /// Total context window in tokens.
    #[serde(default)]
    pub context: f64,
    /// Maximum input tokens, when it differs from the context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// Maximum output tokens.
    #[serde(default)]
    pub output: f64,
}

/// `models-dev.ts:36-50`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogCost {
    /// Input price per million tokens.
    #[serde(default)]
    pub input: f64,
    /// Output price per million tokens.
    #[serde(default)]
    pub output: f64,
    /// Cache-read price per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Context-length pricing tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<CatalogCostTier>>,
    /// The long-context surcharge, priced separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<CatalogCostBand>,
}

/// `models-dev.ts:25-34` — a price band gated on a context size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogCostTier {
    /// Input price per million tokens inside this tier.
    #[serde(default)]
    pub input: f64,
    /// Output price per million tokens inside this tier.
    #[serde(default)]
    pub output: f64,
    /// Cache-read price inside this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price inside this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// The context threshold this tier applies above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<CatalogCostTierBound>,
}

/// The `{ type: "context", size }` bound on a cost tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogCostTierBound {
    /// Always `"context"` today; kept as a string so a new kind parses.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The context size the tier applies above.
    #[serde(default)]
    pub size: f64,
}

/// `models-dev.ts:42-49` — the `context_over_200k` band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogCostBand {
    /// Input price per million tokens above the threshold.
    #[serde(default)]
    pub input: f64,
    /// Output price per million tokens above the threshold.
    #[serde(default)]
    pub output: f64,
    /// Cache-read price above the threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price above the threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// `models-dev.ts:92-97`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogModalities {
    /// Media types the model accepts.
    #[serde(default)]
    pub input: Vec<Modality>,
    /// Media types the model produces.
    #[serde(default)]
    pub output: Vec<Modality>,
}

/// `models-dev.ts:94` — one media type.
///
/// `Other` exists so a modality added upstream degrades to "not one this build
/// knows" instead of failing the whole document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    /// Plain text.
    Text,
    /// Audio.
    Audio,
    /// Still images.
    Image,
    /// Video.
    Video,
    /// PDF documents.
    Pdf,
    /// A modality this build does not know.
    #[serde(untagged)]
    Other(String),
}

/// `models-dev.ts:15` — a model's lifecycle status.
///
/// The catalog omits the field for a normal model; the oracle then treats it as
/// `"active"` (`provider.ts:1457`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogStatus {
    /// Experimental: hidden unless experimental models are enabled.
    Alpha,
    /// Preview, but listed.
    Beta,
    /// Never listed.
    Deprecated,
    /// Listed. Not a models.dev value; the oracle's default for "no status".
    Active,
}

/// `models-dev.ts:98-115` — the `experimental.modes` map.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogExperimental {
    /// Mode name to its overrides. Each entry becomes its own model id.
    #[serde(default)]
    pub modes: BTreeMap<String, CatalogMode>,
}

/// One alternate wire configuration for a model.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogMode {
    /// Pricing that replaces the base model's, merged over it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CatalogCost>,
    /// Request body and header overrides for this mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<CatalogModeProvider>,
}

/// `models-dev.ts:105-110` — a mode's transport overrides.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CatalogModeProvider {
    /// Extra request-body fields, in the catalog's snake_case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Map<String, serde_json::Value>>,
    /// Extra request headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

/// `models-dev.ts:77-85` — where interleaved reasoning arrives.
///
/// Three shapes in the wild: a bare boolean, a bare field name, and
/// `{ field }`. All three are live in the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Interleaved {
    /// `false` for none, `true` for the provider's default field.
    Flag(bool),
    /// `{ "field": "reasoning_content" }`.
    Field {
        /// The response field carrying reasoning.
        field: String,
    },
    /// A bare `"reasoning_content"`.
    Name(String),
}

impl Interleaved {
    /// The field name, when one is named.
    ///
    /// Collapses the three shapes so a consumer never re-matches the union.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::Flag(_) => None,
            Self::Field { field } => Some(field),
            Self::Name(name) => Some(name),
        }
    }

    /// True unless the shape is an explicit `false`.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        match self {
            Self::Flag(flag) => *flag,
            Self::Field { .. } | Self::Name(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_top_level_field_does_not_break_the_document() {
        // models.dev is a live remote document. A field added upstream must not
        // cost the user their model list.
        let json = r#"{
          "acme": {
            "name": "Acme", "id": "acme", "env": ["ACME_KEY"],
            "brand_new_field": {"nested": true},
            "models": {"m": {"id": "m", "name": "M", "limit": {"context": 1, "output": 1},
                             "another_new_field": 7}}
          }
        }"#;
        let doc: CatalogDocument = serde_json::from_str(json).expect("unknown fields are ignored");
        assert_eq!(doc["acme"].models["m"].name, "M");
    }

    #[test]
    fn all_three_interleaved_shapes_parse() {
        let flag: Interleaved = serde_json::from_str("false").expect("bool shape");
        let named: Interleaved = serde_json::from_str(r#""reasoning_content""#).expect("bare name");
        let field: Interleaved =
            serde_json::from_str(r#"{"field":"reasoning_text"}"#).expect("field shape");
        assert_eq!((flag.enabled(), flag.field()), (false, None));
        assert_eq!(
            (named.enabled(), named.field()),
            (true, Some("reasoning_content"))
        );
        assert_eq!(
            (field.enabled(), field.field()),
            (true, Some("reasoning_text"))
        );
    }

    #[test]
    fn an_unknown_modality_degrades_instead_of_failing() {
        let modalities: CatalogModalities =
            serde_json::from_str(r#"{"input":["text","hologram"],"output":["text"]}"#)
                .expect("unknown modality parses");
        assert_eq!(
            modalities.input,
            vec![Modality::Text, Modality::Other("hologram".to_owned())]
        );
    }

    #[test]
    fn a_missing_limit_block_defaults_to_zero() {
        let model: CatalogModel =
            serde_json::from_str(r#"{"id":"m","name":"M"}"#).expect("limit is optional");
        assert_eq!(model.limit.context, 0.0);
        assert_eq!(model.limit.output, 0.0);
        assert_eq!(model.limit.input, None);
    }
}
