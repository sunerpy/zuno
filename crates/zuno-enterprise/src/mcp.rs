//! Explicit owner-to-target assignments. Connection sessions are operation
//! scoped and tokens are reloaded before each call to support revocation.
use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zuno_application::mcp::{
    MAX_MCP_RESULT_BYTES, McpConnectionProvider, McpFailure, McpToolBinding, PreparedMcpCall,
};
use zuno_types::{activity::ActivityName, identity::PrincipalKey};

use crate::{Error, config, invalid};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConnectionConfig {
    pub owner: PrincipalKey,
    pub connection: ActivityName,
    pub revision: u64,
    pub endpoint: String,
    /// Target-specific token only. Never a Zuno API or Worker service token.
    pub access_token_file: PathBuf,
    pub root_certificate: Option<PathBuf>,
    pub timeout_millis: u32,
}

pub struct ConfiguredMcpConnections {
    entries: Vec<(McpConnectionConfig, reqwest::Client)>,
}
impl ConfiguredMcpConnections {
    pub async fn new(configs: Vec<McpConnectionConfig>) -> Result<Self, Error> {
        if configs.len() > 1024 {
            return Err(invalid("configure at most 1024 owner MCP connections"));
        }
        let mut seen = BTreeSet::new();
        let mut entries = Vec::new();
        for config in configs {
            let endpoint =
                url::Url::parse(&config.endpoint).map_err(|_| invalid("invalid MCP endpoint"))?;
            if endpoint.scheme() != "https"
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || config.revision == 0
                || !config.access_token_file.is_absolute()
                || !(100..=120_000).contains(&config.timeout_millis)
                || !seen.insert((config.owner.clone(), config.connection.as_str().to_owned()))
            {
                return Err(invalid("invalid or ambiguous owner MCP connection"));
            }
            // Validate the configured credential before exposing this backend.
            config::secret(&config.access_token_file).await?;
            let mut builder = zuno_network::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(u64::from(config.timeout_millis)));
            if let Some(path) = &config.root_certificate {
                let bytes = config::read_file(path, 1048576).await?;
                builder = builder.add_root_certificate(
                    reqwest::Certificate::from_pem(&bytes)
                        .map_err(|_| invalid("invalid MCP root certificate"))?,
                );
            }
            let client = builder
                .build()
                .map_err(|_| invalid("invalid MCP HTTP client"))?;
            entries.push((config, client));
        }
        Ok(Self { entries })
    }
}
#[async_trait]
impl McpConnectionProvider for ConfiguredMcpConnections {
    async fn prepare(
        &self,
        owner: &PrincipalKey,
        binding: &McpToolBinding,
    ) -> Result<Box<dyn PreparedMcpCall>, McpFailure> {
        binding
            .validate()
            .map_err(|_| McpFailure::DefinitionChanged)?;
        let (config, http) = self
            .entries
            .iter()
            .find(|(config, _)| {
                config.owner == *owner
                    && config.connection == binding.connection
                    && config.revision == binding.revision
                    && config.endpoint == binding.endpoint
            })
            .ok_or(McpFailure::AuthorizationRevoked)?;
        let bearer = config::secret(&config.access_token_file)
            .await
            .map_err(|_| McpFailure::AuthorizationRevoked)?;
        let remote = zuno_config::schema::mcp::McpRemote {
            kind: zuno_config::schema::mcp::RemoteKind::Remote,
            url: config.endpoint.clone(),
            enabled: Some(true),
            headers: None,
            oauth: Some(zuno_config::schema::mcp::McpOauth::Disabled(
                zuno_config::schema::ordered::False,
            )),
            timeout: std::num::NonZeroU32::new(config.timeout_millis),
            streamable_http_only: true,
        };
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(u64::from(config.timeout_millis));
        let client = tokio::time::timeout_at(
            deadline,
            zuno_mcp::RemoteClient::connect_with_client(
                binding.server.as_str(),
                &remote,
                Some(bearer),
                http.clone(),
            ),
        )
        .await
        .map_err(|_| McpFailure::ConnectionUnavailable)?
        .map_err(|_| McpFailure::ConnectionUnavailable)?;
        let result = tokio::time::timeout_at(deadline, async {
            let tools = client
                .list_tools_bounded(256, 524288)
                .await
                .map_err(|_| McpFailure::ConnectionUnavailable)?;
            let mut matching = tools
                .iter()
                .filter(|tool| tool.name == binding.tool.as_str());
            let tool = matching.next().ok_or(McpFailure::DefinitionChanged)?;
            if matching.next().is_some()
                || serde_json::to_value(tool).map_err(|_| McpFailure::DefinitionChanged)?
                    != binding.definition
            {
                return Err(McpFailure::DefinitionChanged);
            }
            Ok(())
        })
        .await
        .unwrap_or(Err(McpFailure::ConnectionUnavailable));
        if let Err(error) = result {
            client.close().await;
            return Err(error);
        }
        Ok(Box::new(PreparedConnection {
            client,
            tool: binding.tool.clone(),
        }))
    }
}
struct PreparedConnection {
    client: zuno_mcp::RemoteClient,
    tool: ActivityName,
}
#[async_trait]
impl PreparedMcpCall for PreparedConnection {
    async fn call(self: Box<Self>, arguments: &Value) -> Result<Value, McpFailure> {
        let args = arguments.as_object().ok_or(McpFailure::DefinitionChanged)?;
        // Once tools/call starts, HTTP/protocol failures do not prove that the
        // external side effect did not happen.
        let output = self
            .client
            .call_tool(self.tool.as_str(), args.clone())
            .await;
        self.client.close().await;
        let value = serde_json::to_value(output.map_err(|_| McpFailure::LostOutcome)?)
            .map_err(|_| McpFailure::LostOutcome)?;
        if serde_json::to_vec(&value)
            .map_err(|_| McpFailure::LostOutcome)?
            .len()
            > MAX_MCP_RESULT_BYTES
        {
            return Err(McpFailure::ResultTooLarge);
        }
        Ok(value)
    }
}
