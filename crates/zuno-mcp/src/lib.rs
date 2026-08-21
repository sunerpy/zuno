//! Model Context Protocol client: stdio and remote transports, tools, resources, prompts.

pub mod catalog;
mod lifecycle;
mod oauth;
mod protocol;
pub mod remote;
pub mod stdio;

pub(crate) const CLIENT_NAME: &str = "zuno";

pub use catalog::{
    ATTACHABLE_MIMES, Catalog, CatalogEvent, CatalogLoader, ConnectedServer, Diagnostic,
    LIST_RESOURCE_TEMPLATES_TOOL, LIST_RESOURCES_TOOL, MAX_RESOURCE_BLOB_BYTES, PromptArgument,
    PromptDefinition, READ_RESOURCE_TOOL, RESOURCE_TOOLS, RefreshError, ResourceContents,
    ResourceDefinition, ResourceTemplate, ServerStatus, resource_permission_patterns,
};

pub use lifecycle::{
    McpConnectOutcome, McpConnection, McpConnector, McpLifecycleError, McpLifecycleOptions,
    McpServerController, McpServerEvent, McpServerSnapshot, McpServerState,
};

pub use remote::{AuthorizationRequest, RemoteClient, RemoteConnect, RemoteError, RemoteTransport};

pub use stdio::{
    DEFAULT_REQUEST_TIMEOUT, InitializeResult, Notification, PROTOCOL_VERSION, StdioClient,
    ToolCallResult, ToolDefinition, ToolsChanged, tool_name,
};
