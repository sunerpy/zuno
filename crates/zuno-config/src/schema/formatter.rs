//! Formatter configuration.
//!
//! Oracle: `packages/core/src/v1/config/formatter.ts:5-13` — `Boolean |
//! Record<String, Entry>`.

use crate::schema::ordered::OrderedMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The `formatter` key: a switch, or per-formatter overrides.
///
/// Omitted disables formatting, `true` enables the built-ins, and an object
/// enables the built-ins with the listed overrides applied.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FormatterConfig {
    /// Enable or disable every formatter at once.
    Enabled(bool),
    /// Per-formatter overrides, keyed by formatter name.
    Formatters(OrderedMap<FormatterEntry>),
}

/// One formatter's overrides (`config/formatter.ts:5-10`).
#[derive(JsonSchema, Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FormatterEntry {
    /// Turn this formatter off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// The command to run, argv-style.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Extra environment for the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<BTreeMap<String, String>>,
    /// File extensions this formatter claims.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
}
