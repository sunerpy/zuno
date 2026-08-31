//! Standard ACP MCP declarations.
//!
//! These values are deliberately process-local. They contain client-provided
//! commands, environment values, and HTTP headers which must never be persisted
//! with a session or rendered through `Debug`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use http::header::{HeaderName, HeaderValue};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::Url;

const VALID_NAME_MAX_BYTES: usize = 32;
const SLUG_MAX_BYTES: usize = 20;

/// A caller-correctable ACP MCP declaration failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct AcpMcpConfigError {
    message: String,
}

impl AcpMcpConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// One validated standard ACP MCP server declaration.
#[derive(Clone, PartialEq, Eq)]
pub enum AcpMcpServer {
    /// A child process speaking MCP over stdin/stdout.
    Stdio(AcpStdioMcpServer),
    /// A Streamable HTTP MCP endpoint.
    Http(AcpHttpMcpServer),
}

impl AcpMcpServer {
    /// Stable tool namespace after ACP-name normalization.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio(server) => server.name(),
            Self::Http(server) => server.name(),
        }
    }
}

impl fmt::Debug for AcpMcpServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio(server) => formatter.debug_tuple("Stdio").field(server).finish(),
            Self::Http(server) => formatter.debug_tuple("Http").field(server).finish(),
        }
    }
}

/// Validated process-local stdio MCP declaration.
#[derive(Clone, PartialEq, Eq)]
pub struct AcpStdioMcpServer {
    name: String,
    command: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
}

impl AcpStdioMcpServer {
    /// Stable normalized server name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Absolute executable path.
    #[must_use]
    pub fn command(&self) -> &Path {
        &self.command
    }

    /// Process arguments in client order.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Environment overrides. Treat values as secrets.
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }
}

impl fmt::Debug for AcpStdioMcpServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcpStdioMcpServer")
            .field("name", &self.name)
            .field("command", &"<redacted>")
            .field("argument_count", &self.args.len())
            .field("environment", &"<redacted>")
            .finish()
    }
}

/// Validated process-local Streamable HTTP MCP declaration.
#[derive(Clone, PartialEq, Eq)]
pub struct AcpHttpMcpServer {
    name: String,
    url: Url,
    headers: BTreeMap<String, String>,
}

impl AcpHttpMcpServer {
    /// Stable normalized server name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Absolute HTTP(S) endpoint. Treat query and userinfo as sensitive.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Request headers. Treat values as secrets.
    #[must_use]
    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }
}

impl fmt::Debug for AcpHttpMcpServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AcpHttpMcpServer")
            .field("name", &self.name)
            .field("endpoint", &diagnostic_endpoint(&self.url))
            .field("headers", &"<redacted>")
            .finish()
    }
}

/// Parse and fully validate one request's complete `mcpServers` array.
///
/// The returned values are safe to retain in process memory only. Their custom
/// `Debug` implementations redact all command and credential-bearing fields.
///
/// # Errors
///
/// Returns [`AcpMcpConfigError`] for malformed declarations, unsupported
/// transports, invalid environment/header entries, or duplicate normalized
/// names.
pub fn parse_mcp_servers(value: Option<&Value>) -> Result<Vec<AcpMcpServer>, AcpMcpConfigError> {
    let values = value
        .ok_or_else(|| AcpMcpConfigError::new("mcpServers must be an array"))?
        .as_array()
        .ok_or_else(|| AcpMcpConfigError::new("mcpServers must be an array"))?;
    let mut names = BTreeSet::new();
    let mut servers = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let object = value.as_object().ok_or_else(|| {
            AcpMcpConfigError::new(format!("mcpServers[{index}] must be an object"))
        })?;
        let raw_name = string_field(object, index, "name")?;
        let name = normalize_name(raw_name)?;
        if !names.insert(name.clone()) {
            return Err(AcpMcpConfigError::new(format!(
                "mcpServers contains duplicate normalized name: {name}"
            )));
        }
        let server = match object.get("type") {
            None => parse_stdio(object, index, name)?,
            Some(Value::String(kind)) if kind == "http" => parse_http(object, index, name)?,
            Some(Value::String(kind)) => {
                return Err(AcpMcpConfigError::new(format!(
                    "mcpServers[{index}] transport {kind} is not supported"
                )));
            }
            Some(_) => {
                return Err(AcpMcpConfigError::new(format!(
                    "mcpServers[{index}].type must be a string"
                )));
            }
        };
        servers.push(server);
    }
    Ok(servers)
}

fn parse_stdio(
    object: &serde_json::Map<String, Value>,
    index: usize,
    name: String,
) -> Result<AcpMcpServer, AcpMcpConfigError> {
    let command = PathBuf::from(string_field(object, index, "command")?);
    if !command.is_absolute() {
        return Err(AcpMcpConfigError::new(format!(
            "mcpServers[{index}].command must be an absolute path"
        )));
    }
    let args = string_array_field(object, index, "args")?;
    let environment = entry_map(object, index, "env", EntryKind::Environment)?;
    Ok(AcpMcpServer::Stdio(AcpStdioMcpServer {
        name,
        command,
        args,
        environment,
    }))
}

fn parse_http(
    object: &serde_json::Map<String, Value>,
    index: usize,
    name: String,
) -> Result<AcpMcpServer, AcpMcpConfigError> {
    let raw_url = string_field(object, index, "url")?;
    let url = Url::parse(raw_url).map_err(|_| {
        AcpMcpConfigError::new(format!(
            "mcpServers[{index}].url must be an absolute HTTP(S) URL"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(AcpMcpConfigError::new(format!(
            "mcpServers[{index}].url must be an absolute HTTP(S) URL"
        )));
    }
    let headers = entry_map(object, index, "headers", EntryKind::Header)?;
    Ok(AcpMcpServer::Http(AcpHttpMcpServer { name, url, headers }))
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    index: usize,
    field: &str,
) -> Result<&'a str, AcpMcpConfigError> {
    object.get(field).and_then(Value::as_str).ok_or_else(|| {
        AcpMcpConfigError::new(format!("mcpServers[{index}].{field} must be a string"))
    })
}

fn string_array_field(
    object: &serde_json::Map<String, Value>,
    index: usize,
    field: &str,
) -> Result<Vec<String>, AcpMcpConfigError> {
    let values = object.get(field).and_then(Value::as_array).ok_or_else(|| {
        AcpMcpConfigError::new(format!("mcpServers[{index}].{field} must be an array"))
    })?;
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                AcpMcpConfigError::new(format!(
                    "mcpServers[{index}].{field} must contain only strings"
                ))
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
enum EntryKind {
    Environment,
    Header,
}

fn entry_map(
    object: &serde_json::Map<String, Value>,
    index: usize,
    field: &str,
    kind: EntryKind,
) -> Result<BTreeMap<String, String>, AcpMcpConfigError> {
    let values = object.get(field).and_then(Value::as_array).ok_or_else(|| {
        AcpMcpConfigError::new(format!("mcpServers[{index}].{field} must be an array"))
    })?;
    let mut identities = BTreeSet::new();
    let mut entries = BTreeMap::new();
    for value in values {
        let entry = value.as_object().ok_or_else(|| {
            AcpMcpConfigError::new(format!(
                "mcpServers[{index}].{field} contains an invalid entry"
            ))
        })?;
        let entry_name = entry.get("name").and_then(Value::as_str).ok_or_else(|| {
            AcpMcpConfigError::new(format!(
                "mcpServers[{index}].{field} contains an invalid entry"
            ))
        })?;
        let entry_value = entry.get("value").and_then(Value::as_str).ok_or_else(|| {
            AcpMcpConfigError::new(format!(
                "mcpServers[{index}].{field} contains an invalid entry"
            ))
        })?;
        let identity = match kind {
            EntryKind::Environment => {
                if entry_name.is_empty()
                    || entry_name.contains(['=', '\0'])
                    || entry_value.contains('\0')
                {
                    return Err(AcpMcpConfigError::new(format!(
                        "mcpServers[{index}].{field} contains an invalid environment entry"
                    )));
                }
                entry_name.to_owned()
            }
            EntryKind::Header => {
                if HeaderName::from_bytes(entry_name.as_bytes()).is_err()
                    || HeaderValue::from_str(entry_value).is_err()
                {
                    return Err(AcpMcpConfigError::new(format!(
                        "mcpServers[{index}].{field} contains an invalid header entry"
                    )));
                }
                entry_name.to_ascii_lowercase()
            }
        };
        if !identities.insert(identity) {
            return Err(AcpMcpConfigError::new(format!(
                "mcpServers[{index}].{field} contains duplicate name: {entry_name}"
            )));
        }
        entries.insert(entry_name.to_owned(), entry_value.to_owned());
    }
    Ok(entries)
}

fn normalize_name(name: &str) -> Result<String, AcpMcpConfigError> {
    if name.trim().is_empty() || name.chars().any(char::is_control) {
        return Err(AcpMcpConfigError::new(
            "mcpServers contains an invalid server name",
        ));
    }
    if name.len() <= VALID_NAME_MAX_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Ok(name.to_owned());
    }
    let mut slug = String::with_capacity(SLUG_MAX_BYTES);
    let mut previous_separator = false;
    for character in name.chars() {
        if slug.len() >= SLUG_MAX_BYTES {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            slug.push(character);
            previous_separator = false;
        } else if !previous_separator && !slug.is_empty() {
            slug.push('_');
            previous_separator = true;
        }
    }
    while slug.ends_with('_') {
        let _removed = slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("server");
    }
    let digest = hex::encode(Sha256::digest(name.as_bytes()));
    Ok(format!("{slug}_{}", &digest[..8]))
}

fn diagnostic_endpoint(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<invalid>");
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("{}://{host}{port}{}", url.scheme(), url.path())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_and_redacts_standard_servers() {
        let value = json!([
            {
                "name": "Fancy server!",
                "command": "/usr/bin/node",
                "args": ["server.js"],
                "env": [{"name": "TOKEN", "value": "sentinel-secret"}]
            },
            {
                "type": "http",
                "name": "!!!",
                "url": "https://user:password@example.test/mcp?token=secret",
                "headers": [{"name": "Authorization", "value": "Bearer sentinel-secret"}]
            }
        ]);
        let servers = parse_mcp_servers(Some(&value)).expect("valid declarations");
        assert!(servers[0].name().starts_with("Fancy_server_"));
        assert!(servers[1].name().starts_with("server_"));
        let debug = format!("{servers:?}");
        assert!(!debug.contains("/usr/bin/node"));
        assert!(!debug.contains("sentinel-secret"));
        assert!(!debug.contains("password"));
        assert!(!debug.contains("token=secret"));
    }

    #[test]
    fn rejects_invalid_and_duplicate_entries() {
        for value in [
            json!([{
                "name": "fixture",
                "command": "node",
                "args": [],
                "env": []
            }]),
            json!([{
                "name": "fixture",
                "command": "/bin/node",
                "args": [],
                "env": [
                    {"name": "A", "value": "one"},
                    {"name": "A", "value": "two"}
                ]
            }]),
            json!([{
                "type": "http",
                "name": "web",
                "url": "file:///tmp/mcp",
                "headers": []
            }]),
            json!([{
                "type": "http",
                "name": "web",
                "url": "https://example.test/mcp",
                "headers": [
                    {"name": "X-Key", "value": "one"},
                    {"name": "x-key", "value": "two"}
                ]
            }]),
        ] {
            assert!(parse_mcp_servers(Some(&value)).is_err());
        }
    }

    #[test]
    fn normalized_names_are_stable_and_collision_checked() {
        let value = json!([
            {
                "name": "same name",
                "command": "/bin/true",
                "args": [],
                "env": []
            },
            {
                "name": "same name",
                "command": "/bin/true",
                "args": [],
                "env": []
            }
        ]);
        let error = parse_mcp_servers(Some(&value)).expect_err("duplicate rejected");
        assert!(error.to_string().contains("duplicate normalized name"));
    }
}
