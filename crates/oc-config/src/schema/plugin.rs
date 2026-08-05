//! Plugin specs.
//!
//! Oracle: `packages/core/src/v1/config/plugin.ts:5-9` — `String | [String,
//! Record<String, Unknown>]`.

use crate::schema::JsonMap;
use serde::{Deserialize, Serialize};

/// One entry of the `plugin` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSpec {
    /// A bare specifier: an npm package, a `file://` URL, or a path.
    Name(String),
    /// A specifier paired with options handed to the plugin at load time.
    WithOptions(String, JsonMap),
}

impl PluginSpec {
    /// The specifier, whichever arm this is.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) | Self::WithOptions(name, _) => name,
        }
    }

    /// The plugin's options, if the spec carries any.
    #[must_use]
    pub fn options(&self) -> Option<&JsonMap> {
        match self {
            Self::Name(_) => None,
            Self::WithOptions(_, options) => Some(options),
        }
    }
}
