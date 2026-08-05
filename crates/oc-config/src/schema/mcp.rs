//! MCP server configuration.
//!
//! Oracle: `packages/core/src/v1/config/mcp.ts:6-62`, plus the extra union arm the
//! top-level `mcp` record adds at `config/config.ts:113-115`:
//! `Record(String, Union([Local | Remote, Struct({ enabled: Boolean })]))`. The
//! third arm is how a config layer switches off a server another layer defined,
//! without restating its command or url.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::num::{NonZeroU16, NonZeroU32};

/// The `type: "local"` discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalKind {
    /// A server this process spawns.
    Local,
}

/// The `type: "remote"` discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    /// A server reached over HTTP.
    Remote,
}

/// One entry of the `mcp` map.
///
/// The arms are tried in the oracle's union order — local, remote, then the
/// enabled-only toggle — and the first that fits wins.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// A locally spawned server.
    Local(McpLocal),
    /// A remote server.
    Remote(McpRemote),
    /// A switch for a server defined by another config layer.
    Toggle(McpToggle),
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Ok(local) = McpLocal::deserialize(&value) {
            return Ok(Self::Local(local));
        }
        if let Ok(remote) = McpRemote::deserialize(&value) {
            return Ok(Self::Remote(remote));
        }
        if let Ok(toggle) = McpToggle::deserialize(&value) {
            return Ok(Self::Toggle(toggle));
        }
        // Nothing fit. Report against the arm the author's own `type` names, so the
        // message says "missing field `url`" instead of "no variant matched".
        let reported = match value.get("type").and_then(serde_json::Value::as_str) {
            Some("local") => McpLocal::deserialize(&value).map(|_| ()),
            Some("remote") => McpRemote::deserialize(&value).map(|_| ()),
            _ => McpToggle::deserialize(&value).map(|_| ()),
        };
        Err(D::Error::custom(match reported {
            Err(error) => error.to_string(),
            Ok(()) => "not a valid MCP server configuration".to_owned(),
        }))
    }
}

/// A locally spawned MCP server (`config/mcp.ts:6-23`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpLocal {
    /// Always `local`.
    #[serde(rename = "type")]
    pub kind: LocalKind,
    /// Command and arguments to run the server.
    pub command: Vec<String>,
    /// Working directory; relative paths resolve from the workspace directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Environment variables for the server process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<BTreeMap<String, String>>,
    /// Start the server, or not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Request timeout in milliseconds; the runtime defaults to 5000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<NonZeroU32>,
}

/// A remote MCP server (`config/mcp.ts:44-59`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpRemote {
    /// Always `remote`.
    #[serde(rename = "type")]
    pub kind: RemoteKind,
    /// URL of the remote server.
    pub url: String,
    /// Start the server, or not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Headers sent with every request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// OAuth settings, or `false` to suppress OAuth auto-detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOauth>,
    /// Request timeout in milliseconds; the runtime defaults to 5000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<NonZeroU32>,
}

/// A switch for a server another config layer defined
/// (`config/config.ts:113-115`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToggle {
    /// Start the server, or not.
    pub enabled: bool,
}

/// The `oauth` key: settings, or `false` to disable auto-detection
/// (`config/mcp.ts:53-55`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpOauth {
    /// Explicit OAuth settings.
    Config(McpOauthConfig),
    /// The literal `false`.
    Disabled(crate::schema::ordered::False),
}

/// OAuth settings for a remote MCP server (`config/mcp.ts:26-41`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct McpOauthConfig {
    /// Client id; absent means dynamic client registration (RFC 7591).
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Client secret, when the authorization server requires one.
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Scopes to request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Port for the local callback server; the runtime defaults to 19876.
    ///
    /// `NonZeroU16` reproduces the oracle's `Int.isBetween(1, 65535)` exactly.
    #[serde(rename = "callbackPort", skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<NonZeroU16>,
    /// Full redirect URI, which overrides `callbackPort`.
    #[serde(rename = "redirectUri", skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}
