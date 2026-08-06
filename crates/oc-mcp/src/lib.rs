//! Model Context Protocol client: stdio and remote transports, tools, resources, prompts.

mod oauth;
mod protocol;
pub mod remote;
pub mod stdio;

pub use remote::{AuthorizationRequest, RemoteClient, RemoteConnect, RemoteError, RemoteTransport};

pub use stdio::{
    DEFAULT_REQUEST_TIMEOUT, InitializeResult, Notification, PROTOCOL_VERSION, StdioClient,
    ToolCallResult, ToolDefinition, ToolsChanged, tool_name,
};
