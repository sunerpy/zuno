//! Resolution of the `formatter` config surface.
//!
//! The schema types live in [`oc_config::schema::formatter`] (todo 7); this module
//! adds the resolved view the post-edit formatter runner consumes.
//!
//! Oracle: `packages/core/src/v1/config/formatter.ts:5-13` — `Union([Boolean,
//! Record(String, Entry)])`, two arms, where `Entry` is `{ disabled?, command?,
//! environment?, extensions? }`. The enablement rules come from the runtime,
//! `packages/opencode/src/format/index.ts:120-157`:
//!
//! - the key absent or `false` disables every formatter (`if (!cfg.formatter)`),
//! - `true` enables the built-ins with no overrides,
//! - an object enables the built-ins and applies the listed overrides, and
//! - `ruff` and `uv` are one backend, so disabling either disables both
//!   (`format/index.ts:138-143`).

use oc_config::schema::formatter::{FormatterConfig, FormatterEntry};
use oc_config::schema::ordered::OrderedMap;
use std::collections::BTreeMap;

/// The two formatter names the oracle treats as one backend
/// (`format/index.ts:139`).
pub const LINKED_RUFF_FORMATTERS: [&str; 2] = ["ruff", "uv"];

/// One formatter's override, with the union arm already interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFormatter {
    /// The name the override was declared under.
    pub name: String,
    /// The command to run, argv-style. `None` means "keep the built-in's".
    pub command: Option<Vec<String>>,
    /// Extra environment for the command.
    pub environment: BTreeMap<String, String>,
    /// The extensions this formatter claims. `None` means "keep the built-in's".
    pub extensions: Option<Vec<String>>,
}

/// The `formatter` key resolved into an answer to "may I format, and how".
///
/// This is the view `oc-format` consumes: it never sees the union again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedFormatters {
    /// Whether formatting runs at all. `false` for an absent key or `false`.
    enabled: bool,
    /// Formatter names switched off individually, including the linked ruff/uv
    /// pair.
    disabled: Vec<String>,
    /// Overrides for the formatters that stayed enabled, in declaration order.
    overrides: Vec<ResolvedFormatter>,
}

impl ResolvedFormatters {
    /// Resolve the `formatter` key. `None` — the key absent — disables
    /// formatting, matching `if (!cfg.formatter)` in the runtime.
    #[must_use]
    pub fn resolve(formatter: Option<&FormatterConfig>) -> Self {
        match formatter {
            None | Some(FormatterConfig::Enabled(false)) => Self::default(),
            Some(FormatterConfig::Enabled(true)) => Self {
                enabled: true,
                disabled: Vec::new(),
                overrides: Vec::new(),
            },
            Some(FormatterConfig::Formatters(map)) => Self::from_map(map),
        }
    }

    fn from_map(map: &OrderedMap<FormatterEntry>) -> Self {
        let ruff_linked_off = LINKED_RUFF_FORMATTERS.iter().any(|name| {
            map.get(name)
                .is_some_and(|entry| entry.disabled == Some(true))
        });

        let mut disabled = Vec::new();
        let mut overrides = Vec::new();
        for (name, entry) in map.iter() {
            if ruff_linked_off && LINKED_RUFF_FORMATTERS.contains(&name) {
                push_unique(&mut disabled, name);
                continue;
            }
            if entry.disabled == Some(true) {
                push_unique(&mut disabled, name);
                continue;
            }
            overrides.push(ResolvedFormatter {
                name: name.to_owned(),
                command: entry.command.clone(),
                environment: entry.environment.clone().unwrap_or_default(),
                extensions: entry.extensions.clone(),
            });
        }
        if ruff_linked_off {
            for name in LINKED_RUFF_FORMATTERS {
                push_unique(&mut disabled, name);
            }
        }
        Self {
            enabled: true,
            disabled,
            overrides,
        }
    }

    /// Whether formatting runs at all.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the formatter named `name` may run.
    ///
    /// `false` when formatting is off wholesale, or when this formatter — or the
    /// backend it shares — was switched off.
    #[must_use]
    pub fn is_formatter_enabled(&self, name: &str) -> bool {
        self.enabled && !self.disabled.iter().any(|entry| entry == name)
    }

    /// The names switched off individually, in declaration order.
    pub fn disabled(&self) -> impl Iterator<Item = &str> {
        self.disabled.iter().map(String::as_str)
    }

    /// Overrides for the formatters that stayed enabled, in declaration order.
    pub fn overrides(&self) -> impl Iterator<Item = &ResolvedFormatter> {
        self.overrides.iter()
    }

    /// The override declared for `name`, if it stayed enabled.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ResolvedFormatter> {
        self.overrides.iter().find(|entry| entry.name == name)
    }

    /// The command configured for `name`, or `None` when the formatter is off or
    /// keeps its built-in command.
    #[must_use]
    pub fn command_for(&self, name: &str) -> Option<&[String]> {
        if !self.is_formatter_enabled(name) {
            return None;
        }
        self.get(name).and_then(|entry| entry.command.as_deref())
    }

    /// Enabled overrides that claim `extension`, which is matched with the
    /// leading dot the runtime uses (`format/index.ts:58`).
    pub fn for_extension<'a>(
        &'a self,
        extension: &'a str,
    ) -> impl Iterator<Item = &'a ResolvedFormatter> {
        self.overrides.iter().filter(move |entry| {
            entry
                .extensions
                .as_ref()
                .is_some_and(|list| list.iter().any(|candidate| candidate == extension))
        })
    }
}

fn push_unique(list: &mut Vec<String>, name: &str) {
    if !list.iter().any(|entry| entry == name) {
        list.push(name.to_owned());
    }
}

#[cfg(test)]
mod tests;
