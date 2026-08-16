//! Failures talking to an MCP server.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::time::Duration;

/// A failure talking to an MCP server.
///
/// [`McpError::Protocol`] carries a concrete `serde_json::Error` on purpose.
/// Framing and JSON-RPC decode bugs are the failure mode that hurts most here —
/// a wrong `Content-Length` or a stray newline produces a decode error whose line
/// and column are the only clue to what went wrong, and boxing it away or
/// flattening it into a message throws that clue out.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The transport could not be established: a process that would not spawn, a
    /// socket that refused, an HTTP endpoint that was unreachable. Retryable,
    /// because a server still coming up is the common cause.
    #[error("mcp server {server} could not be reached")]
    Connect {
        server: String,
        #[source]
        source: BoxSource,
    },

    /// The transport came up but the initialize exchange failed or the server
    /// declared an unusable capability set.
    #[error("mcp server {server} failed to initialize")]
    Handshake {
        server: String,
        #[source]
        source: BoxSource,
    },

    /// A frame could not be decoded. Framing bugs live here.
    #[error("mcp server {server} sent a message that could not be decoded")]
    Protocol {
        server: String,
        #[source]
        source: serde_json::Error,
    },

    /// The server did not answer in time. Retryable.
    #[error("mcp server {server} did not respond within {elapsed:?}")]
    Timeout { server: String, elapsed: Duration },

    /// A tool the server exposes failed when called. `tool` identifies which one.
    #[error("mcp server {server} failed to run tool {tool}")]
    ToolCall {
        server: String,
        tool: String,
        #[source]
        source: BoxSource,
    },
}

impl McpError {
    /// The name of the server that failed.
    #[must_use]
    pub fn server(&self) -> &str {
        match self {
            Self::Connect { server, .. }
            | Self::Handshake { server, .. }
            | Self::Protocol { server, .. }
            | Self::Timeout { server, .. }
            | Self::ToolCall { server, .. } => server,
        }
    }

    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when the identical exchange may succeed on another attempt.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }
}

impl Recoverable for McpError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Connect { .. } | Self::Timeout { .. } => Recovery::Retry { after: None },
            Self::Handshake { .. } | Self::Protocol { .. } | Self::ToolCall { .. } => {
                Recovery::Fail
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<McpError> {
        vec![
            McpError::Connect {
                server: "playwright".to_owned(),
                source: Box::new(std::io::Error::other("connection refused")),
            },
            McpError::Handshake {
                server: "playwright".to_owned(),
                source: Box::new(std::io::Error::other("unsupported protocol version")),
            },
            McpError::Protocol {
                server: "playwright".to_owned(),
                source: serde_json::from_str::<serde_json::Value>("{\"jsonrpc\":").unwrap_err(),
            },
            McpError::Timeout {
                server: "playwright".to_owned(),
                elapsed: Duration::from_secs(30),
            },
            McpError::ToolCall {
                server: "playwright".to_owned(),
                tool: "browser_navigate".to_owned(),
                source: Box::new(std::io::Error::other("target closed")),
            },
        ]
    }

    #[test]
    fn every_variant_names_its_server() {
        for e in every_variant() {
            assert_eq!(e.server(), "playwright", "{e}");
        }
    }

    #[test]
    fn transport_level_failures_retry_and_protocol_failures_do_not() {
        for e in every_variant() {
            let expected = matches!(e, McpError::Connect { .. } | McpError::Timeout { .. });
            assert_eq!(e.is_retryable(), expected, "{e}");
        }
    }

    #[test]
    fn protocol_failure_keeps_the_decode_position() {
        let source = serde_json::from_str::<serde_json::Value>("{\"jsonrpc\":").unwrap_err();
        let column = source.column();
        let e = McpError::Protocol {
            server: "playwright".to_owned(),
            source,
        };
        let McpError::Protocol { source, .. } = &e else {
            panic!("constructed a Protocol, matched something else");
        };
        assert_eq!(source.column(), column);
        assert!(column > 0, "a decode error must point somewhere");
    }

    #[test]
    fn tool_call_failure_names_both_server_and_tool() {
        let e = McpError::ToolCall {
            server: "playwright".to_owned(),
            tool: "browser_navigate".to_owned(),
            source: Box::new(std::io::Error::other("target closed")),
        };
        assert_eq!(
            e.to_string(),
            "mcp server playwright failed to run tool browser_navigate"
        );
    }
}
