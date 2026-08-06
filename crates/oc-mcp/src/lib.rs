//! Model Context Protocol client: stdio and remote transports, tools, resources, prompts.

pub mod catalog;
mod oauth;
mod protocol;
pub mod remote;
pub mod stdio;

pub use catalog::{
    ATTACHABLE_MIMES, Catalog, CatalogEvent, CatalogLoader, ConnectedServer, Diagnostic,
    LIST_RESOURCE_TEMPLATES_TOOL, LIST_RESOURCES_TOOL, MAX_RESOURCE_BLOB_BYTES, PromptArgument,
    PromptDefinition, READ_RESOURCE_TOOL, RESOURCE_TOOLS, RefreshError, ResourceContents,
    ResourceDefinition, ResourceTemplate, ServerStatus, resource_permission_patterns,
};

pub use remote::{AuthorizationRequest, RemoteClient, RemoteConnect, RemoteError, RemoteTransport};

pub use stdio::{
    DEFAULT_REQUEST_TIMEOUT, InitializeResult, Notification, PROTOCOL_VERSION, StdioClient,
    ToolCallResult, ToolDefinition, ToolsChanged, tool_name,
};
