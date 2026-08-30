//! Resolution of the `lsp` config surface.
//!
//! The schema types live in [`zuno_config::schema::lsp`] (todo 7); this module adds
//! the resolved view the LSP runtime consumes.
//!
//! Oracle: `packages/core/src/v1/config/lsp.ts:5-18` — the per-server entry is
//! itself a union of `{ disabled: true }` and `{ command, extensions?, disabled?,
//! env?, initialization? }` — and `:76-78`, the outer `Union([Boolean,
//! Record(String, Entry)])`. Enablement comes from the runtime,
//! `packages/opencode/src/lsp/lsp.ts:151-180`:
//!
//! - the key absent or `false` disables every server (`if (!cfg.lsp)`),
//! - `true` enables the built-ins with no overrides,
//! - an object enables the built-ins and applies the listed overrides, and
//! - a truthy `disabled` removes that one server and leaves the rest alone.

use std::collections::BTreeMap;
use zuno_config::schema::JsonMap;
use zuno_config::schema::lsp::{LspConfig, LspEntry};
use zuno_config::schema::ordered::OrderedMap;

pub use zuno_config::schema::lsp::BUILTIN_SERVER_IDS;

/// One LSP server's configuration, with both union layers interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedServer {
    /// The server id the override was declared under.
    pub id: String,
    /// The command to spawn, argv-style. Always present: the schema requires it
    /// for any server that is not switched off.
    pub command: Vec<String>,
    /// Extensions the server handles. Empty means "keep the built-in's", which
    /// only a built-in id can do.
    pub extensions: Vec<String>,
    /// Extra environment for the server process.
    pub env: BTreeMap<String, String>,
    /// `initializationOptions` handed to the server on startup.
    pub initialization: Option<JsonMap>,
}

impl ResolvedServer {
    /// Whether this id is one of the runtime's built-in servers.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        BUILTIN_SERVER_IDS.contains(&self.id.as_str())
    }
}

/// The `lsp` key resolved into an answer to "is this server enabled, and with
/// what command".
///
/// This is the view `zuno-lsp` consumes: it never sees either union again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedLsp {
    /// Whether any LSP runs. `false` for an absent key or `false`.
    enabled: bool,
    /// Server ids switched off individually.
    disabled: Vec<String>,
    /// Overrides for the servers that stayed enabled, in declaration order.
    servers: Vec<ResolvedServer>,
}

impl ResolvedLsp {
    /// Resolve the `lsp` key. `None` — the key absent — disables every server,
    /// matching `if (!cfg.lsp)` in the runtime.
    #[must_use]
    pub fn resolve(lsp: Option<&LspConfig>) -> Self {
        match lsp {
            None | Some(LspConfig::Enabled(false)) => Self::default(),
            Some(LspConfig::Enabled(true)) => Self {
                enabled: true,
                disabled: Vec::new(),
                servers: Vec::new(),
            },
            Some(LspConfig::Servers(map)) => Self::from_map(map),
        }
    }

    fn from_map(map: &OrderedMap<LspEntry>) -> Self {
        let mut disabled = Vec::new();
        let mut servers = Vec::new();
        for (id, entry) in map.iter() {
            if entry.is_disabled() {
                disabled.push(id.to_owned());
                continue;
            }
            servers.push(ResolvedServer {
                id: id.to_owned(),
                // The schema rejects a non-disabled entry without a command, so
                // an absent one here cannot happen; an empty argv is the only
                // representation that keeps this infallible.
                command: entry.command.clone().unwrap_or_default(),
                extensions: entry.extensions.clone().unwrap_or_default(),
                env: entry.env.clone().unwrap_or_default(),
                initialization: entry.initialization.clone(),
            });
        }
        Self {
            enabled: true,
            disabled,
            servers,
        }
    }

    /// Whether any LSP server runs.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the server with id `id` may run.
    ///
    /// `false` when LSP is off wholesale or when this one server was switched
    /// off. A built-in id with no override is enabled, because an object arm
    /// enables the built-ins and only then applies overrides.
    #[must_use]
    pub fn is_server_enabled(&self, id: &str) -> bool {
        self.enabled && !self.disabled.iter().any(|entry| entry == id)
    }

    /// The server ids switched off individually, in declaration order.
    pub fn disabled(&self) -> impl Iterator<Item = &str> {
        self.disabled.iter().map(String::as_str)
    }

    /// Overrides for the servers that stayed enabled, in declaration order.
    pub fn servers(&self) -> impl Iterator<Item = &ResolvedServer> {
        self.servers.iter()
    }

    /// The override declared for `id`, if it stayed enabled.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ResolvedServer> {
        self.servers.iter().find(|server| server.id == id)
    }

    /// The command configured for `id`, or `None` when the server is off or has
    /// no override.
    #[must_use]
    pub fn command_for(&self, id: &str) -> Option<&[String]> {
        if !self.is_server_enabled(id) {
            return None;
        }
        self.get(id).map(|server| server.command.as_slice())
    }

    /// The extensions configured for `id`, or `None` when the server is off or
    /// keeps the built-in's extensions.
    #[must_use]
    pub fn extensions_for(&self, id: &str) -> Option<&[String]> {
        if !self.is_server_enabled(id) {
            return None;
        }
        self.get(id)
            .map(|server| server.extensions.as_slice())
            .filter(|list| !list.is_empty())
    }

    /// The `initializationOptions` configured for `id`.
    #[must_use]
    pub fn initialization_for(&self, id: &str) -> Option<&JsonMap> {
        if !self.is_server_enabled(id) {
            return None;
        }
        self.get(id)
            .and_then(|server| server.initialization.as_ref())
    }

    /// Enabled overrides that claim `extension`.
    pub fn for_extension<'a>(
        &'a self,
        extension: &'a str,
    ) -> impl Iterator<Item = &'a ResolvedServer> {
        self.servers.iter().filter(move |server| {
            server
                .extensions
                .iter()
                .any(|candidate| candidate == extension)
        })
    }
}

#[cfg(test)]
mod tests;
