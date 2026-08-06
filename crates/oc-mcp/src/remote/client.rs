use std::collections::HashSet;
use std::sync::atomic::Ordering;

use oc_auth::McpAuthStore;
use oc_config::schema::mcp::{McpOauth, McpRemote};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::protocol::{ReaderFailure, fail_pending, lock};
use crate::stdio::{InitializeResult, ToolCallResult, ToolDefinition};

use super::transport::connect_transport;
use super::{
    MAX_LIST_PAGES, RemoteClient, RemoteConnect, RemoteError, RemoteTransport, SESSION_HEADER,
};

impl RemoteClient {
    /// Connects using the process-wide `mcp-auth.json` store.
    pub async fn connect(
        server: impl Into<String>,
        config: &McpRemote,
    ) -> Result<RemoteConnect, RemoteError> {
        Self::connect_with_store(server, config, McpAuthStore::new(oc_paths::mcp_auth_file())).await
    }

    /// Connects using an explicit credential store.
    pub async fn connect_with_store(
        server: impl Into<String>,
        config: &McpRemote,
        store: McpAuthStore,
    ) -> Result<RemoteConnect, RemoteError> {
        let server = server.into();
        let bearer = crate::oauth::bearer_token(&server, config, &store).await?;
        match connect_transport(
            &server,
            config,
            RemoteTransport::StreamableHttp,
            bearer.clone(),
        )
        .await
        {
            Ok(client) => Ok(RemoteConnect::Connected(client)),
            Err(error) if error.is_authorization_required() => {
                authorization_outcome(server, config, store, error).await
            }
            Err(streamable) => {
                match connect_transport(&server, config, RemoteTransport::Sse, bearer).await {
                    Ok(client) => Ok(RemoteConnect::Connected(client)),
                    Err(error) if error.is_authorization_required() => {
                        authorization_outcome(server, config, store, error).await
                    }
                    Err(sse) => Err(RemoteError::Fallback {
                        streamable: Box::new(streamable),
                        sse: Box::new(sse),
                    }),
                }
            }
        }
    }

    /// Selected wire transport.
    #[must_use]
    pub fn transport(&self) -> RemoteTransport {
        self.inner.transport
    }

    /// Successful initialize payload.
    #[must_use]
    pub fn initialization(&self) -> &InitializeResult {
        self.inner
            .initialization
            .get()
            .expect("connected remote client is initialized")
    }

    /// Lists every tool page advertised by the server.
    pub async fn list_tools(&self) -> Result<Vec<ToolDefinition>, RemoteError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let value = self.request("tools/list", params).await?;
            let page: ListToolsResult = serde_json::from_value(value).map_err(|error| {
                self.protocol_error(format!("invalid tools/list result: {error}"))
            })?;
            tools.extend(page.tools);
            let Some(next) = page.next_cursor else {
                return Ok(tools);
            };
            if !seen.insert(next.clone()) {
                return Err(
                    self.protocol_error(format!("tools/list returned duplicate cursor {next:?}"))
                );
            }
            cursor = Some(next);
        }
        Err(self.protocol_error(format!("tools/list exceeded {MAX_LIST_PAGES} pages")))
    }

    /// Calls one remote MCP tool.
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<ToolCallResult, RemoteError> {
        let value = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|error| self.protocol_error(format!("invalid tools/call result: {error}")))
    }

    /// Stops background SSE reading and deletes an active HTTP session when present.
    pub async fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        fail_pending(&self.inner.pending, ReaderFailure::Closed);
        let legacy_reader = self
            .inner
            .legacy
            .as_ref()
            .and_then(|legacy| lock(&legacy.reader).take());
        if let Some(reader) = legacy_reader {
            reader.abort();
            let _result = reader.await;
        }
        if self.inner.transport == RemoteTransport::StreamableHttp
            && let Some(session) = self.inner.session_id.lock().await.clone()
        {
            let _result = self
                .request_builder(reqwest::Method::DELETE, self.inner.base_url.clone())
                .header(SESSION_HEADER, session)
                .send()
                .await;
        }
    }
}

async fn authorization_outcome(
    server: String,
    config: &McpRemote,
    store: McpAuthStore,
    error: RemoteError,
) -> Result<RemoteConnect, RemoteError> {
    if matches!(config.oauth, Some(McpOauth::Disabled(_))) {
        return Err(RemoteError::OAuthDisabled { server });
    }
    crate::oauth::begin_authorization(&server, config, store, error.challenge()).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListToolsResult {
    tools: Vec<ToolDefinition>,
    #[serde(default)]
    next_cursor: Option<String>,
}
