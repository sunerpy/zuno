//! LSP server configuration.
//!
//! Oracle: `packages/core/src/v1/config/lsp.ts:5-18` (the entry union), `:22-61`
//! (the built-in server ids), `:63-78` (the "custom servers must declare
//! extensions" check, which is part of the schema, not a runtime concern).

use crate::schema::JsonMap;
use crate::schema::ordered::OrderedMap;
use schemars::JsonSchema;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// The built-in LSP server ids, verbatim from `config/lsp.ts:22-61`.
///
/// A server id outside this list is a custom server, and a custom server has to
/// declare `extensions` because the runtime cannot infer them.
pub const BUILTIN_SERVER_IDS: &[&str] = &[
    "deno",
    "typescript",
    "vue",
    "eslint",
    "oxlint",
    "biome",
    "gopls",
    "ruby-lsp",
    "ty",
    "pyright",
    "elixir-ls",
    "zls",
    "csharp",
    "razor",
    "fsharp",
    "sourcekit-lsp",
    "rust",
    "clangd",
    "svelte",
    "astro",
    "jdtls",
    "kotlin-ls",
    "yaml-ls",
    "lua-ls",
    "php intelephense",
    "prisma",
    "dart",
    "ocaml-lsp",
    "bash",
    "terraform",
    "texlab",
    "dockerfile",
    "gleam",
    "clojure-lsp",
    "nixd",
    "tinymist",
    "haskell-language-server",
    "julials",
];

/// The `lsp` key: a switch, or per-server overrides.
///
/// Omitted disables LSP, `true` enables the built-ins, and an object enables the
/// built-ins with the listed overrides applied.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LspConfig {
    /// Enable or disable every server at once.
    Enabled(bool),
    /// Per-server overrides, keyed by server id.
    Servers(OrderedMap<LspEntry>),
}

impl<'de> Deserialize<'de> for LspConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LspConfigVisitor;

        impl<'de> Visitor<'de> for LspConfigVisitor {
            type Value = LspConfig;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a boolean or an object of LSP server overrides")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(LspConfig::Enabled(value))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut servers = OrderedMap::new();
                while let Some((id, entry)) = access.next_entry::<String, LspEntry>()? {
                    if !entry.is_disabled()
                        && !BUILTIN_SERVER_IDS.contains(&id.as_str())
                        && entry.extensions.is_none()
                    {
                        return Err(de::Error::custom(format!(
                            "custom LSP server {id:?} must declare an 'extensions' array"
                        )));
                    }
                    servers.insert(id, entry);
                }
                Ok(LspConfig::Servers(servers))
            }
        }

        deserializer.deserialize_any(LspConfigVisitor)
    }
}

/// One LSP server's configuration.
///
/// The oracle spells this as a union of `{ disabled: true }` and `{ command, ... }`.
/// Collapsing it into one struct with the same requirement — `command` is
/// mandatory unless the server is switched off — accepts and rejects exactly the
/// same documents while keeping every key the author wrote, which the union arm
/// order would otherwise discard.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "LspEntryWire")]
pub struct LspEntry {
    /// The server command, argv-style. Required unless `disabled` is `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// File extensions the server handles. Required for custom servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Turn this server off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Extra environment for the server process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    /// `initializationOptions` handed to the server on startup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initialization: Option<JsonMap>,
}

impl LspEntry {
    /// Whether this entry switches the server off.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled == Some(true)
    }
}

#[derive(JsonSchema, Deserialize)]
struct LspEntryWire {
    command: Option<Vec<String>>,
    extensions: Option<Vec<String>>,
    disabled: Option<bool>,
    env: Option<BTreeMap<String, String>>,
    initialization: Option<JsonMap>,
}

impl TryFrom<LspEntryWire> for LspEntry {
    type Error = String;

    fn try_from(wire: LspEntryWire) -> Result<Self, Self::Error> {
        if wire.command.is_none() && wire.disabled != Some(true) {
            return Err("an LSP server needs a 'command' unless 'disabled' is true".to_owned());
        }
        Ok(Self {
            command: wire.command,
            extensions: wire.extensions,
            disabled: wire.disabled,
            env: wire.env,
            initialization: wire.initialization,
        })
    }
}
