//! The TUI's own configuration surface.
//!
//! # Why this is not in `oc-config`
//!
//! The `theme` key is deliberately absent from the main config schema
//! (`packages/core/src/v1/config/config.ts`); the oracle declares it on the TUI's
//! own `Info` struct instead (`packages/tui/src/config/index.tsx:55`). Keeping it
//! here preserves that separation: a headless server or CLI run never parses, and
//! never has to know about, a terminal colour scheme.
//!
//! This module is additive by design. Every field is optional with a `serde`
//! default, so a config file that sets one key and omits the rest parses, and a
//! later key can be added without touching any existing one.

use serde::{Deserialize, Serialize};

/// The `[tui]` configuration block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    /// The theme to render with.
    ///
    /// Any name any theme layer provides, including the special value `system`,
    /// which derives a palette from the terminal's own colours. Absent means the
    /// built-in default ([`crate::theme::DEFAULT_THEME`]). A name no layer provides
    /// is not an error: [`crate::theme::ThemeRegistry::resolve`] falls back and
    /// reports a diagnostic naming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
}

impl TuiConfig {
    /// The configured theme name, or `None` when the key was omitted.
    #[must_use]
    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }
}
