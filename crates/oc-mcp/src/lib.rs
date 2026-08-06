//! Model Context Protocol client: stdio and remote transports, tools, resources, prompts.

pub mod stdio;

pub use stdio::{
    DEFAULT_REQUEST_TIMEOUT, InitializeResult, Notification, PROTOCOL_VERSION, StdioClient,
    ToolCallResult, ToolDefinition, ToolsChanged, tool_name,
};
