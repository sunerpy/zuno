//! Merging connected MCP servers into the tool registry.
//!
//! # Why a status gate rather than an absence check
//!
//! The oracle keeps two maps: `status` for every configured server and `clients`
//! for the ones that handshook, and `tools()` refuses to trust either alone — it
//! walks `clients` *and* re-checks `status[name] === "connected"`
//! (`mcp/index.ts:666-688`). That belt-and-braces reading exists because a
//! connection can die between discovery and use: the `onclose` hook flips the
//! status to `failed` and deletes the cached definitions
//! (`mcp/index.ts:442-455`), and anything that read the map in between would
//! otherwise advertise tools whose transport is gone.
//!
//! This module keeps the last definition snapshot on a failed entry instead of
//! deleting it, and makes [`ServerStatus::is_connected`] the *single* decision
//! point for exposure. Deleting would make the gate untestable — a failed server
//! would contribute nothing whether or not the gate existed — whereas retaining
//! the snapshot means removing the gate immediately leaks a dead server's tools
//! and the test says so.
//!
//! # Why the three resource tools live here
//!
//! `list_mcp_resources`, `list_mcp_resource_templates`, and `read_mcp_resource`
//! are not any one server's tools; they are this crate's view across every
//! connected server (`session/tools.ts:136-385`). Their names are load-bearing:
//! [`zuno_permission::visibility::permission_key`] collapses exactly those three
//! literals onto the `read` key (`permission/index.ts:204-219`), so one
//! `{"read": "deny"}` hides all three together. Renaming any of them silently
//! breaks that collapse, which is why [`RESOURCE_TOOLS`] is asserted against
//! [`zuno_permission::visibility::READ_TOOLS`] in this module's tests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use zuno_catalog::command::McpPrompt;
use zuno_error::{McpError, ToolError};
use zuno_llm::cache::{LockedTools, McpToolStatus, ToolSnapshot};
use zuno_permission::Rule;
use zuno_permission::visibility::retain_visible_tools;
use zuno_tool::{Attachment, Tool, ToolContext, ToolEffect, ToolOutput};
use zuno_tools::registry::{CustomTool, McpToolLoader, McpToolSnapshot};

use crate::protocol::lock;
use crate::stdio::{ToolCallResult, ToolDefinition, tool_name};

/// Tool id that lists resources across connected servers.
pub const LIST_RESOURCES_TOOL: &str = "list_mcp_resources";

/// Tool id that lists resource templates across connected servers.
pub const LIST_RESOURCE_TEMPLATES_TOOL: &str = "list_mcp_resource_templates";

/// Tool id that reads one resource from one server.
pub const READ_RESOURCE_TOOL: &str = "read_mcp_resource";

/// The three resource tools, in the order the oracle declares them
/// (`session/tools.ts:27-31`).
pub const RESOURCE_TOOLS: [&str; 3] = [
    LIST_RESOURCES_TOOL,
    LIST_RESOURCE_TEMPLATES_TOOL,
    READ_RESOURCE_TOOL,
];

/// Attachment types a resource blob may become (`session/tools.ts:33-39`).
pub const ATTACHABLE_MIMES: [&str; 5] = [
    "application/pdf",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/webp",
];

/// Largest resource blob promoted to an attachment (`session/tools.ts:32`).
pub const MAX_RESOURCE_BLOB_BYTES: usize = 10 * 1024 * 1024;

const EVENT_CAPACITY: usize = 64;

/// One resource advertised by a server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceDefinition {
    /// Opaque MCP identifier, not necessarily a file URL.
    pub uri: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Server-supplied description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared media type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Fields this client does not model are passed through to the model.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One parameterized resource advertised by a server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    /// RFC 6570 template the caller fills in before reading.
    pub uri_template: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Server-supplied description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared media type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Fields this client does not model are passed through to the model.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The `resources/read` payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResourceContents {
    /// Text or blob items, kept as raw JSON because a server may send either.
    #[serde(default)]
    pub contents: Vec<Value>,
}

/// One prompt advertised by a server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptDefinition {
    /// Prompt name, unsanitized.
    pub name: String,
    /// Server-supplied description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared arguments in server order.
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

/// One declared prompt argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptArgument {
    /// Argument name as the command resolver will position it.
    pub name: String,
    /// Server-supplied description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the server requires a value.
    #[serde(default)]
    pub required: bool,
}

/// A server that completed the MCP handshake, whichever transport carried it.
///
/// Both [`crate::StdioClient`] and [`crate::RemoteClient`] implement this. The
/// catalog holds trait objects so a merged tool proxy can relay a call without
/// knowing, or re-deciding, which transport its server used.
#[async_trait]
pub trait ConnectedServer: Send + Sync + 'static {
    /// Configured server name, used for namespacing and diagnostics.
    fn server_name(&self) -> &str;

    /// Whether the server declared a `resources` capability.
    ///
    /// Gates registration of the three resource tools exactly as
    /// `session/tools.ts:155-157` gates it on `getServerCapabilities()?.resources`.
    fn supports_resources(&self) -> bool;

    /// Whether the server declared a `prompts` capability.
    fn supports_prompts(&self) -> bool;

    /// Lists every tool page.
    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError>;

    /// Calls one tool by its **server-local** name.
    async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, McpError>;

    /// Lists every resource page.
    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError>;

    /// Lists every resource-template page.
    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError>;

    /// Reads one resource by URI.
    async fn read_resource(&self, uri: &str) -> Result<ResourceContents, McpError>;

    /// Lists every prompt page.
    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError>;
}

/// Why a configured server is or is not contributing tools.
///
/// Mirrors the oracle's `Status` union (`mcp/index.ts:83-107`). Only
/// [`ServerStatus::Connected`] exposes tools; every other variant is a
/// diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    /// The handshake completed and the server is answering.
    Connected,
    /// Configuration disabled the server, so its absence is not a fault.
    Disabled,
    /// The connection failed, or a live connection closed.
    Failed {
        /// Failure text, already naming what went wrong.
        error: String,
    },
    /// OAuth must be completed before the handshake can proceed.
    NeedsAuth,
    /// The server rejected dynamic client registration.
    NeedsClientRegistration {
        /// Registration failure text.
        error: String,
    },
}

impl ServerStatus {
    /// Whether this server may contribute tools.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Short machine-readable label used in diagnostics and logs.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disabled => "disabled",
            Self::Failed { .. } => "failed",
            Self::NeedsAuth => "needs_auth",
            Self::NeedsClientRegistration { .. } => "needs_client_registration",
        }
    }
}

/// A server that contributed no tools, and why.
///
/// The oracle logs `server unavailable` with the key and status
/// (`mcp/index.ts:383-386`) and, for the two OAuth cases, raises a toast that
/// names the server (`mcp/index.ts:297-321`). Both spellings agree on the one
/// thing that matters to a user staring at a missing tool: the *name* of the
/// server that is missing. This carries it as data rather than as prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The configured server name.
    pub server: String,
    /// Why it is not contributing.
    pub status: ServerStatus,
}

impl Diagnostic {
    /// One-line message naming the server and its state.
    #[must_use]
    pub fn message(&self) -> String {
        match &self.status {
            ServerStatus::Connected => {
                format!("MCP server {} is connected", self.server)
            }
            ServerStatus::Disabled => {
                format!(
                    "MCP server {} is disabled and contributes no tools",
                    self.server
                )
            }
            ServerStatus::Failed { error } => format!(
                "MCP server {} is unavailable and contributes no tools: {error}",
                self.server
            ),
            ServerStatus::NeedsAuth => format!(
                "MCP server {} requires authentication and contributes no tools: run `zuno mcp auth {}`",
                self.server, self.server
            ),
            ServerStatus::NeedsClientRegistration { error } => format!(
                "MCP server {} requires a pre-registered client id and contributes no tools: {error}",
                self.server
            ),
        }
    }
}

/// A change to the merged tool list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogEvent {
    /// One server's tool list changed, was added, or was withdrawn.
    ///
    /// Payload matches the oracle's `ToolsChanged` event, which carries the
    /// server name and nothing else (`mcp/index.ts:461-471`): a subscriber
    /// re-reads the catalog rather than trusting a diff it was handed.
    ToolsChanged {
        /// The server whose tools changed.
        server: String,
    },
}

struct Entry {
    status: ServerStatus,
    handle: Option<Arc<dyn ConnectedServer>>,
    tools: Vec<ToolDefinition>,
    prompts: Vec<PromptDefinition>,
}

struct Inner {
    /// Keyed by server name so the merged order is deterministic. The oracle
    /// iterates a JavaScript object's insertion order, which is not reproducible
    /// across configuration reads; its own code-mode catalog then sorts by name
    /// anyway (`tool/code-mode.ts:42`). A stable order is required here because
    /// todo 31's locked tool list compares whole snapshots for equality.
    entries: Mutex<BTreeMap<String, Entry>>,
    /// Server names configuration asked for. Discovery stays
    /// [`McpToolStatus::Pending`] until every one of them has reported.
    expected: Mutex<BTreeSet<String>>,
    /// Servers supplied as part of an explicit session contract.
    ///
    /// Their connected tools must be present in the first provider request.
    /// Ordinary host-configured servers are still discovered progressively.
    eager_servers: BTreeSet<String>,
    events: broadcast::Sender<CatalogEvent>,
}

/// The merged view of every configured MCP server.
///
/// Cheap to clone; clones share one entry table and one event channel.
#[derive(Clone)]
pub struct Catalog {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = lock(&self.inner.entries);
        formatter
            .debug_struct("Catalog")
            .field("servers", &entries.len())
            .field(
                "connected",
                &entries.values().filter(|e| e.status.is_connected()).count(),
            )
            .finish()
    }
}

impl Catalog {
    /// A catalog awaiting reports from `expected` configured servers.
    ///
    /// Naming the expected set up front is what lets [`Self::discovery_status`]
    /// distinguish "no MCP servers configured" (immediately settled) from "the
    /// servers have not answered yet" (still pending, so the tool list must not
    /// be treated as final).
    #[must_use]
    pub fn new<I, S>(expected: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new_with_eager_servers(expected, std::iter::empty::<String>())
    }

    /// A catalog whose selected servers are an explicit session dependency.
    ///
    /// Tools contributed by `eager_servers` are kept in the first provider
    /// request. Other connected servers in the same catalog remain eligible for
    /// progressive discovery.
    #[must_use]
    pub fn new_with_eager_servers<I, S, J, T>(expected: I, eager_servers: J) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        J: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(BTreeMap::new()),
                expected: Mutex::new(expected.into_iter().map(Into::into).collect()),
                eager_servers: eager_servers.into_iter().map(Into::into).collect(),
                events,
            }),
        }
    }

    /// Receives catalog changes from this point forward.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<CatalogEvent> {
        self.inner.events.subscribe()
    }

    /// Records a server that handshook, with the tools it advertised.
    ///
    /// Publishes [`CatalogEvent::ToolsChanged`] so a late connection is visible
    /// to whoever owns the locked tool list.
    pub fn connected(&self, server: Arc<dyn ConnectedServer>, tools: Vec<ToolDefinition>) {
        self.connected_with_prompts(server, tools, Vec::new());
    }

    /// [`Self::connected`] plus the server's prompts, for the command resolver.
    pub fn connected_with_prompts(
        &self,
        server: Arc<dyn ConnectedServer>,
        tools: Vec<ToolDefinition>,
        prompts: Vec<PromptDefinition>,
    ) {
        let name = server.server_name().to_owned();
        lock(&self.inner.entries).insert(
            name.clone(),
            Entry {
                status: ServerStatus::Connected,
                handle: Some(server),
                tools,
                prompts,
            },
        );
        self.settle(&name);
        self.publish(name);
    }

    /// Records a server that will not contribute tools.
    ///
    /// Keeps both the definitions **and** the handle the entry already had, so
    /// [`ServerStatus::is_connected`] is the only thing standing between a dead
    /// server and the merged list. Clearing either would hide a missing gate
    /// behind a second, accidental one: the tools would vanish for the wrong
    /// reason and a test could not tell the difference.
    pub fn unavailable(&self, server: impl Into<String>, status: ServerStatus) {
        let name = server.into();
        {
            let mut entries = lock(&self.inner.entries);
            match entries.get_mut(&name) {
                Some(entry) => entry.status = status,
                None => {
                    entries.insert(
                        name.clone(),
                        Entry {
                            status,
                            handle: None,
                            tools: Vec::new(),
                            prompts: Vec::new(),
                        },
                    );
                }
            }
        }
        self.settle(&name);
        self.publish(name);
    }

    /// Re-lists one connected server's tools after
    /// `notifications/tools/list_changed` and publishes the change.
    ///
    /// A failed re-list leaves the previous snapshot in place, matching
    /// `mcp/index.ts:465-466` where a `listed` of `undefined` returns without
    /// touching the cache: a transient list failure must not empty a working
    /// server's tools.
    ///
    /// # Errors
    ///
    /// [`RefreshError::NotConnected`] when the server is not connected, or
    /// [`RefreshError::List`] carrying the transport error from `tools/list`.
    pub async fn refresh(&self, server: &str) -> Result<Vec<ToolDefinition>, RefreshError> {
        let handle = self.connected_handle(server)?;
        let tools = handle.list_tools().await.map_err(RefreshError::List)?;
        {
            let mut entries = lock(&self.inner.entries);
            let entry = entries
                .get_mut(server)
                .filter(|entry| entry.status.is_connected())
                .ok_or_else(|| RefreshError::NotConnected {
                    server: server.to_owned(),
                })?;
            entry.tools = tools.clone();
        }
        self.publish(server.to_owned());
        Ok(tools)
    }

    /// Whether asynchronous discovery can still add tools.
    ///
    /// [`McpToolStatus::Ready`] only once every expected server has reported,
    /// success or failure. A server that never reports keeps this `Pending`,
    /// which is the conservative answer: the locked list then holds its one
    /// rebuild in reserve rather than spending it on an incomplete view.
    #[must_use]
    pub fn discovery_status(&self) -> McpToolStatus {
        if lock(&self.inner.expected).is_empty() {
            McpToolStatus::Ready
        } else {
            McpToolStatus::Pending
        }
    }

    /// Every server that is not contributing tools, in name order.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        lock(&self.inner.entries)
            .iter()
            .filter(|(_, entry)| !entry.status.is_connected())
            .map(|(server, entry)| Diagnostic {
                server: server.clone(),
                status: entry.status.clone(),
            })
            .collect()
    }

    /// Connected servers, in name order.
    #[must_use]
    pub fn connected_servers(&self) -> Vec<String> {
        lock(&self.inner.entries)
            .iter()
            .filter(|(_, entry)| entry.status.is_connected())
            .map(|(server, _)| server.clone())
            .collect()
    }

    /// Connected servers that declared a `resources` capability, in name order.
    ///
    /// Oracle: the `resourceServers` list computed in `session/tools.ts:161-165`.
    #[must_use]
    pub fn resource_servers(&self) -> Vec<String> {
        lock(&self.inner.entries)
            .iter()
            .filter(|(_, entry)| {
                entry.status.is_connected()
                    && entry
                        .handle
                        .as_ref()
                        .is_some_and(|handle| handle.supports_resources())
            })
            .map(|(server, _)| server.clone())
            .collect()
    }

    /// The merged tool list: namespaced server tools, then the resource tools.
    ///
    /// Only connected servers contribute. The resource tools are appended once,
    /// for the whole catalog, and only when some connected server can serve
    /// resources (`session/tools.ts:155-157`).
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tool_snapshot().tools
    }

    fn tool_snapshot(&self) -> McpToolSnapshot {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut eager_tool_ids = Vec::new();
        let mut has_resources = false;
        let mut eager_resources = false;
        let entries = lock(&self.inner.entries);
        for (server, entry) in entries.iter() {
            if !entry.status.is_connected() {
                continue;
            }
            let Some(handle) = entry.handle.as_ref() else {
                tracing::warn!(%server, "connected MCP server has no handle; skipping its tools");
                continue;
            };
            let eager = self.inner.eager_servers.contains(server);
            for definition in &entry.tools {
                if eager {
                    eager_tool_ids.push(tool_name(server, &definition.name));
                }
                tools.push(Arc::new(McpToolProxy::new(
                    server,
                    definition,
                    Arc::clone(handle),
                )));
            }
            has_resources |= handle.supports_resources();
            eager_resources |= eager && handle.supports_resources();
        }
        drop(entries);
        if has_resources {
            tools.push(Arc::new(ListResourcesTool {
                catalog: self.clone(),
            }));
            tools.push(Arc::new(ListResourceTemplatesTool {
                catalog: self.clone(),
            }));
            tools.push(Arc::new(ReadResourceTool {
                catalog: self.clone(),
            }));
        }
        if eager_resources {
            eager_tool_ids.extend(RESOURCE_TOOLS.map(str::to_owned));
        }
        McpToolSnapshot {
            tools,
            eager_tool_ids,
        }
    }

    /// [`Self::tools`] with tools hidden by an unconditional deny removed.
    ///
    /// Oracle: `tool/registry.ts:275-284` applies `Permission.visibleTools`
    /// *after* `mcp.tools()`. Filtering here as well as in the registry is not
    /// redundant — the code-mode path describes this list without going through
    /// the registry, so the deny has to hold on both routes.
    #[must_use]
    pub fn visible_tools(&self, rules: &[Rule]) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.tools();
        retain_visible_tools(&mut tools, rules, |tool| tool.id());
        tools
    }

    /// The merged tool ids, in the same order as [`Self::tools`].
    #[must_use]
    pub fn tool_ids(&self) -> Vec<String> {
        self.tools()
            .iter()
            .map(|tool| tool.id().to_owned())
            .collect()
    }

    /// Prompts from connected servers, ready for the command resolver.
    ///
    /// Wave 3 places MCP prompts at level 3 of
    /// [`zuno_catalog::command::Sources`], above config commands and below nothing
    /// — skills only fill free names. Feeding this list is all this crate owes
    /// that precedence.
    #[must_use]
    pub fn prompts(&self) -> Vec<McpPrompt> {
        lock(&self.inner.entries)
            .iter()
            .filter(|(_, entry)| entry.status.is_connected())
            .flat_map(|(server, entry)| {
                entry.prompts.iter().map(move |prompt| McpPrompt {
                    client: server.clone(),
                    prompt: prompt.name.clone(),
                    description: prompt.description.clone(),
                    arguments: prompt
                        .arguments
                        .iter()
                        .map(|argument| argument.name.clone())
                        .collect(),
                })
            })
            .collect()
    }

    /// Feeds todo 31's locked tool list and returns the frozen snapshot.
    ///
    /// The once-only property is [`LockedTools`]'s, not this method's: it spends
    /// its single late-MCP rebuild the first time it is handed
    /// [`McpToolStatus::Ready`] with a changed list, and ignores every later
    /// change. This method must therefore never reset the lock — a reset would
    /// re-arm that rebuild and let a third connection rewrite a prompt prefix
    /// that providers have already cached.
    pub fn tools_for_request(&self, locked: &mut LockedTools<String>) -> ToolSnapshot<String> {
        locked.tools_for_request(&self.tool_ids(), self.discovery_status())
    }

    /// A handle implementing the registry's MCP loader seam.
    #[must_use]
    pub fn loader(&self) -> CatalogLoader {
        CatalogLoader {
            catalog: self.clone(),
        }
    }

    fn connected_handle(&self, server: &str) -> Result<Arc<dyn ConnectedServer>, RefreshError> {
        lock(&self.inner.entries)
            .get(server)
            .filter(|entry| entry.status.is_connected())
            .and_then(|entry| entry.handle.clone())
            .ok_or_else(|| RefreshError::NotConnected {
                server: server.to_owned(),
            })
    }

    fn settle(&self, server: &str) {
        lock(&self.inner.expected).remove(server);
    }

    fn publish(&self, server: String) {
        let _receivers = self
            .inner
            .events
            .send(CatalogEvent::ToolsChanged { server });
    }
}

/// Both transports satisfy the same contract; only the error type differs, and
/// the catalog does not care which one carried a call.
#[async_trait]
impl ConnectedServer for crate::StdioClient {
    fn server_name(&self) -> &str {
        Self::server_name(self)
    }

    fn supports_resources(&self) -> bool {
        declares_capability(self.initialization(), "resources")
    }

    fn supports_prompts(&self) -> bool {
        declares_capability(self.initialization(), "prompts")
    }

    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        Self::list_tools(self).await
    }

    async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, McpError> {
        Self::call_tool(self, tool, arguments).await
    }

    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        Self::list_resources(self).await
    }

    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        Self::list_resource_templates(self).await
    }

    async fn read_resource(&self, uri: &str) -> Result<ResourceContents, McpError> {
        Self::read_resource(self, uri).await
    }

    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        Self::list_prompts(self).await
    }
}

/// Remote failures are re-wrapped as [`McpError::Connect`] so the catalog holds
/// one error type. The `RemoteError` display already names the server and the
/// transport, so nothing is lost by boxing it as the source.
#[async_trait]
impl ConnectedServer for crate::RemoteClient {
    fn server_name(&self) -> &str {
        Self::server_name(self)
    }

    fn supports_resources(&self) -> bool {
        declares_capability(Some(self.initialization()), "resources")
    }

    fn supports_prompts(&self) -> bool {
        declares_capability(Some(self.initialization()), "prompts")
    }

    async fn list_tools(&self) -> Result<Vec<ToolDefinition>, McpError> {
        Self::list_tools(self)
            .await
            .map_err(|error| remote_error(Self::server_name(self), error))
    }

    async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, McpError> {
        Self::call_tool(self, tool, arguments)
            .await
            .map_err(|error| remote_error(Self::server_name(self), error))
    }

    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        Self::list_resources(self)
            .await
            .map_err(|error| remote_error(Self::server_name(self), error))
    }

    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        Self::list_resource_templates(self)
            .await
            .map_err(|error| remote_error(Self::server_name(self), error))
    }

    async fn read_resource(&self, uri: &str) -> Result<ResourceContents, McpError> {
        Self::read_resource(self, uri)
            .await
            .map_err(|error| remote_error(Self::server_name(self), error))
    }

    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        Self::list_prompts(self)
            .await
            .map_err(|error| remote_error(Self::server_name(self), error))
    }
}

fn remote_error(server: &str, error: crate::RemoteError) -> McpError {
    McpError::Connect {
        server: server.to_owned(),
        source: Box::new(error),
    }
}

/// A capability counts as declared when the server sent the key at all.
///
/// The oracle reads `getServerCapabilities()?.resources` and treats any present
/// value as support (`session/tools.ts:155-157`), so an empty object still
/// counts. A server that omits the key gets no resource tools.
fn declares_capability(initialization: Option<&crate::InitializeResult>, key: &str) -> bool {
    initialization.is_some_and(|initialization| {
        initialization
            .capabilities
            .get(key)
            .is_some_and(|value| !value.is_null())
    })
}

/// Supplies the catalog's tools through
/// [`zuno_tools::registry::McpToolLoader`].
///
/// The registry appends these after built-ins and plugin tools and then applies
/// permission hiding to the whole list (`zuno-tools/src/registry.rs:267-287`,
/// `:387-395`), which is why this returns the merged list unfiltered.
#[derive(Clone, Debug)]
pub struct CatalogLoader {
    catalog: Catalog,
}

impl McpToolLoader for CatalogLoader {
    fn tools(&self) -> Vec<CustomTool> {
        self.catalog.tools()
    }

    fn eager_tool_ids(&self) -> Vec<String> {
        self.catalog.tool_snapshot().eager_tool_ids
    }

    fn snapshot(&self) -> McpToolSnapshot {
        self.catalog.tool_snapshot()
    }
}

/// One server tool, exposed under its namespaced id.
struct McpToolProxy {
    id: String,
    tool: String,
    description: String,
    schema: Value,
    server: Arc<dyn ConnectedServer>,
}

impl McpToolProxy {
    fn new(
        server_name: &str,
        definition: &ToolDefinition,
        server: Arc<dyn ConnectedServer>,
    ) -> Self {
        Self {
            id: tool_name(server_name, &definition.name),
            tool: definition.name.clone(),
            description: definition.description.clone().unwrap_or_default(),
            schema: object_schema(definition.input_schema.clone()),
            server,
        }
    }
}

#[async_trait]
impl Tool for McpToolProxy {
    fn id(&self) -> &str {
        &self.id
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn raw_parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    /// Relays the call under the server's **own** tool name.
    ///
    /// The namespaced id is an addressing detail of this process; splitting it
    /// back apart at call time would be guesswork, because the sanitizer is not
    /// injective — `a.b` and `a/b` both become `a_b`. The oracle avoids the same
    /// trap by capturing the original definition in the closure
    /// (`mcp/catalog.ts:42-67`), and so does this proxy.
    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let arguments = match args {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            other => {
                return Err(ToolError::InvalidArgs {
                    tool: self.id.clone(),
                    source: Box::new(NotAnObject { found: other }),
                });
            }
        };
        let result = self
            .server
            .call_tool(&self.tool, arguments)
            .await
            .map_err(|source| ToolError::Failed {
                tool: self.id.clone(),
                source: Box::new(source),
            })?;
        if result.is_error {
            return Err(ToolError::Failed {
                tool: self.id.clone(),
                source: Box::new(Message(render_content(&result.content).text)),
            });
        }
        let rendered = render_content(&result.content);
        let mut output = ToolOutput::text(String::new(), rendered.text);
        output.attachments = rendered.attachments;
        Ok(output)
    }
}

struct ListResourcesTool {
    catalog: Catalog,
}

#[async_trait]
impl Tool for ListResourcesTool {
    fn id(&self) -> &str {
        LIST_RESOURCES_TOOL
    }

    fn description(&self) -> &str {
        "Lists resources provided by connected MCP servers. Resources provide context such as files, database schemas, or application-specific information."
    }

    fn raw_parameters_schema(&self) -> Value {
        optional_server_schema(
            "Optional MCP server name. When omitted, lists resources from every connected server.",
        )
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let requested = optional_server(self.id(), &args)?;
        let servers = self
            .catalog
            .selected_resource_servers(self.id(), requested.as_deref())?;
        let mut listed = Vec::new();
        for server in &servers {
            let handle = self.catalog.resource_handle(self.id(), server)?;
            for resource in list_of(self.id(), server, handle.list_resources().await)? {
                let key = (resource.name.clone(), resource.uri.clone());
                listed.push(labelled(server, key, resource));
            }
        }
        listed.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
        let payload = json!({
            "resources": listed.into_iter().map(|item| item.value).collect::<Vec<_>>(),
        });
        Ok(resource_output(
            requested.as_ref().map_or_else(
                || "MCP resources".to_owned(),
                |server| format!("MCP resources: {server}"),
            ),
            &payload,
            &servers,
        ))
    }
}

struct ListResourceTemplatesTool {
    catalog: Catalog,
}

#[async_trait]
impl Tool for ListResourceTemplatesTool {
    fn id(&self) -> &str {
        LIST_RESOURCE_TEMPLATES_TOOL
    }

    fn description(&self) -> &str {
        "Lists resource templates provided by connected MCP servers. Resource templates are parameterized resources that can be read after filling in their URI template."
    }

    fn raw_parameters_schema(&self) -> Value {
        optional_server_schema(
            "Optional MCP server name. When omitted, lists resource templates from every connected server.",
        )
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let requested = optional_server(self.id(), &args)?;
        let servers = self
            .catalog
            .selected_resource_servers(self.id(), requested.as_deref())?;
        let mut listed = Vec::new();
        for server in &servers {
            let handle = self.catalog.resource_handle(self.id(), server)?;
            for template in list_of(self.id(), server, handle.list_resource_templates().await)? {
                let key = (template.name.clone(), template.uri_template.clone());
                listed.push(labelled(server, key, template));
            }
        }
        listed.sort_by(|left, right| left.sort_key.cmp(&right.sort_key));
        let payload = json!({
            "resourceTemplates": listed.into_iter().map(|item| item.value).collect::<Vec<_>>(),
        });
        Ok(resource_output(
            requested.as_ref().map_or_else(
                || "MCP resource templates".to_owned(),
                |server| format!("MCP resource templates: {server}"),
            ),
            &payload,
            &servers,
        ))
    }
}

struct ReadResourceTool {
    catalog: Catalog,
}

#[async_trait]
impl Tool for ReadResourceTool {
    fn id(&self) -> &str {
        READ_RESOURCE_TOOL
    }

    fn description(&self) -> &str {
        "Read a specific resource from an MCP server using the server name and resource URI. The URI is an MCP identifier and does not need to be a file URL."
    }

    fn raw_parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": format!("MCP server name exactly as returned by {LIST_RESOURCES_TOOL}."),
                },
                "uri": {
                    "type": "string",
                    "description": format!("Resource URI to read. Use the exact URI string returned by {LIST_RESOURCES_TOOL}."),
                },
            },
            "required": ["server", "uri"],
            "additionalProperties": false,
        })
    }

    fn effect(&self, _args: &Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let server = required_string(self.id(), &args, "server")?;
        let uri = required_string(self.id(), &args, "uri")?;
        let _selected = self
            .catalog
            .selected_resource_servers(self.id(), Some(&server))?;
        let handle = self.catalog.resource_handle(self.id(), &server)?;
        let contents = handle
            .read_resource(&uri)
            .await
            .map_err(|source| ToolError::Failed {
                tool: self.id().to_owned(),
                source: Box::new(source),
            })?;
        let rendered = render_resource_contents(&server, &uri, &contents);
        let mut output = ToolOutput::text(format!("MCP resource: {uri}"), rendered.text);
        output.attachments = rendered.attachments;
        output.metadata.insert("server".to_owned(), json!(server));
        output.metadata.insert("uri".to_owned(), json!(uri));
        output
            .metadata
            .insert("contents".to_owned(), json!(rendered.items));
        output
            .metadata
            .insert("attachments".to_owned(), json!(output.attachments.len()));
        Ok(output)
    }
}

impl Catalog {
    /// The resource servers one call may address, or a refusal naming the ones
    /// that exist.
    ///
    /// Oracle: `session/tools.ts:166-172`. The message deliberately lists the
    /// available servers, because the common failure is a plausible-looking name
    /// the model invented and the fix is knowing the real ones.
    fn selected_resource_servers(
        &self,
        tool: &str,
        requested: Option<&str>,
    ) -> Result<Vec<String>, ToolError> {
        let available = self.resource_servers();
        let Some(requested) = requested else {
            return Ok(available);
        };
        if available.iter().any(|server| server == requested) {
            return Ok(vec![requested.to_owned()]);
        }
        Err(ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(NoSuchResourceServer {
                server: requested.to_owned(),
                available,
            }),
        })
    }

    fn resource_handle(
        &self,
        tool: &str,
        server: &str,
    ) -> Result<Arc<dyn ConnectedServer>, ToolError> {
        self.connected_handle(server)
            .map_err(|source| ToolError::Failed {
                tool: tool.to_owned(),
                source: Box::new(source),
            })
    }
}

/// Why a catalog refresh could not run.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// The named server is not currently connected, so it has no tools to list.
    #[error("mcp server {server} is not connected")]
    NotConnected {
        /// The configured server name.
        server: String,
    },
    /// `tools/list` itself failed; the previous snapshot was left in place.
    #[error(transparent)]
    List(#[from] McpError),
}

#[derive(Debug, thiserror::Error)]
#[error("arguments were {found} rather than a JSON object")]
struct NotAnObject {
    found: Value,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Message(String);

#[derive(Debug, thiserror::Error)]
struct NoSuchResourceServer {
    server: String,
    available: Vec<String>,
}

impl std::fmt::Display for NoSuchResourceServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.available.is_empty() {
            write!(
                formatter,
                "MCP server {:?} does not support resources",
                self.server
            )
        } else {
            write!(
                formatter,
                "MCP server {:?} does not support resources. Available resource servers: {}",
                self.server,
                self.available.join(", ")
            )
        }
    }
}

struct Labelled {
    sort_key: (String, String, String),
    value: Value,
}

/// Rewrites a listed item's internal `client` field as the public `server`
/// field, and computes the oracle's sort key.
///
/// Oracle: `formatMcpResource` (`session/tools.ts:523-526`) drops `client` and
/// adds `server`; the sort is by `client`, `name`, `uri` joined with NUL
/// (`session/tools.ts:190-194`).
fn labelled<T: Serialize>(server: &str, (name, uri): (String, String), item: T) -> Labelled {
    let mut value = serde_json::to_value(item).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.remove("client");
        map.insert("server".to_owned(), json!(server));
    }
    Labelled {
        sort_key: (server.to_owned(), name, uri),
        value,
    }
}

fn list_of<T>(
    tool: &str,
    server: &str,
    result: Result<Vec<T>, McpError>,
) -> Result<Vec<T>, ToolError> {
    result.map_err(|source| {
        tracing::warn!(%server, %tool, "MCP list failed");
        ToolError::Failed {
            tool: tool.to_owned(),
            source: Box::new(source),
        }
    })
}

fn resource_output(title: String, payload: &Value, servers: &[String]) -> ToolOutput {
    let count = payload
        .as_object()
        .and_then(|object| object.values().next())
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let text = serde_json::to_string_pretty(payload).unwrap_or_else(|_| "{}".to_owned());
    let mut output = ToolOutput::text(title, text);
    output.metadata.insert("count".to_owned(), json!(count));
    output.metadata.insert("servers".to_owned(), json!(servers));
    output
}

/// The permission patterns the oracle asks with for a resource call.
///
/// Oracle: `session/tools.ts:173-175` — `mcp:<server>:*` for the addressed
/// server, or one pattern per resource server when none was named. The registry
/// gate that fronts these tools asks under the coarser `read` / `*` pair
/// (`zuno-tools/src/registry.rs:429-440`), so this exists to keep the finer form
/// ported and testable for whoever wires a per-server gate; it is strictly
/// narrower than what the coarse gate already allows.
#[must_use]
pub fn resource_permission_patterns(server: Option<&str>, servers: &[String]) -> Vec<String> {
    match server {
        Some(server) => vec![format!("mcp:{server}:*")],
        None => servers
            .iter()
            .map(|server| format!("mcp:{server}:*"))
            .collect(),
    }
}

struct Rendered {
    text: String,
    attachments: Vec<Attachment>,
    items: usize,
}

/// Flattens `tools/call` content blocks into text plus attachments.
///
/// Oracle: `session/tools.ts:430-470`.
fn render_content(content: &[Value]) -> Rendered {
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    text.push(value.to_owned());
                }
            }
            Some("image") => {
                if let (Some(mime), Some(data)) = (
                    block.get("mimeType").and_then(Value::as_str),
                    block.get("data").and_then(Value::as_str),
                ) {
                    attachments.push(Attachment::new(mime, format!("data:{mime};base64,{data}")));
                }
            }
            Some("resource") => {
                if let Some(resource) = block.get("resource") {
                    push_resource_item(resource, "", &mut text, &mut attachments);
                }
            }
            _ => {}
        }
    }
    Rendered {
        items: content.len(),
        text: text.join("\n\n"),
        attachments,
    }
}

/// Renders a `resources/read` payload.
///
/// Oracle: `formatMcpResourceContent` (`session/tools.ts:533-575`), including
/// its fallback sentence when a server returns no contents at all.
fn render_resource_contents(server: &str, uri: &str, contents: &ResourceContents) -> Rendered {
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for item in &contents.contents {
        push_resource_item(item, uri, &mut text, &mut attachments);
    }
    let joined = text.join("\n\n");
    Rendered {
        items: contents.contents.len(),
        text: if joined.is_empty() {
            format!("MCP resource {uri} from {server} returned no contents.")
        } else {
            joined
        },
        attachments,
    }
}

fn push_resource_item(
    item: &Value,
    fallback_uri: &str,
    text: &mut Vec<String>,
    attachments: &mut Vec<Attachment>,
) {
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or(fallback_uri);
    let mime = item
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    if let Some(value) = item.get("text").and_then(Value::as_str) {
        text.push(format!("Resource: {uri}\nMIME: {mime}\n{value}"));
        return;
    }
    let Some(blob) = item.get("blob").and_then(Value::as_str) else {
        text.push(format!(
            "[MCP resource content without text or blob: {uri}]"
        ));
        return;
    };
    let size = base64_size(blob);
    if !ATTACHABLE_MIMES.contains(&mime) {
        text.push(format!(
            "[Binary MCP resource omitted: {uri} ({mime}, {}) is not a supported attachment type]",
            format_bytes(size)
        ));
        return;
    }
    if size > MAX_RESOURCE_BLOB_BYTES {
        text.push(format!(
            "[Binary MCP resource omitted: {uri} ({mime}, {}) exceeds {}]",
            format_bytes(size),
            format_bytes(MAX_RESOURCE_BLOB_BYTES)
        ));
        return;
    }
    text.push(format!("[Binary MCP resource attached: {uri} ({mime})]"));
    let mut attachment = Attachment::new(mime, format!("data:{mime};base64,{blob}"));
    attachment.filename = Some(uri.to_owned());
    attachments.push(attachment);
}

/// Decoded byte length of a base64 payload, without decoding it.
///
/// Oracle: `base64Size` (`session/tools.ts:578-582`). Measuring the encoded
/// form is the point — a 10 MB limit must be enforced before allocating the
/// decoded copy.
fn base64_size(value: &str) -> usize {
    let trimmed: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let padding = if trimmed.ends_with("==") {
        2
    } else if trimmed.ends_with('=') {
        1
    } else {
        0
    };
    (trimmed.len() * 3 / 4).saturating_sub(padding)
}

/// Oracle: `formatBytes` (`session/tools.ts:584-588`) — rounds **up**, so a
/// 1-byte overage still reads as a distinct size.
fn format_bytes(value: usize) -> String {
    if value < 1024 {
        return format!("{value} B");
    }
    if value < 1024 * 1024 {
        return format!("{} KB", value.div_ceil(1024));
    }
    format!("{} MB", value.div_ceil(1024 * 1024))
}

/// Forces a server's schema into the object shape providers require.
///
/// Oracle: `convertTool` (`mcp/catalog.ts:43-48`) overrides `type`, defaults
/// `properties`, and closes the object. A server that omits `properties` would
/// otherwise produce a schema some providers reject outright.
fn object_schema(schema: Value) -> Value {
    let mut object = match schema {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    object.insert("type".to_owned(), json!("object"));
    if !object.get("properties").is_some_and(Value::is_object) {
        object.insert("properties".to_owned(), json!({}));
    }
    object.insert("additionalProperties".to_owned(), json!(false));
    Value::Object(object)
}

fn optional_server_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": description,
            },
        },
        "additionalProperties": false,
    })
}

/// Oracle: `optionalString` (`session/tools.ts:512-518`) treats an empty string
/// as absent, so a model that sends `""` gets the all-servers listing rather
/// than a refusal.
fn optional_server(tool: &str, args: &Value) -> Result<Option<String>, ToolError> {
    match args.get("server") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.is_empty() => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(Message("server must be a string".to_owned())),
        }),
    }
}

/// Oracle: `requiredString` (`session/tools.ts:520-524`).
fn required_string(tool: &str, args: &Value, key: &str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(Value::String(_)) | None | Some(Value::Null) => Err(ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(Message(format!("{key} is required"))),
        }),
        Some(_) => Err(ToolError::InvalidArgs {
            tool: tool.to_owned(),
            source: Box::new(Message(format!("{key} must be a string"))),
        }),
    }
}
