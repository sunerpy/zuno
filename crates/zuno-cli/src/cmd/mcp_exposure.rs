//! Provider-schema exposure, independent from MCP transport startup and authority.
//!
//! Resolve only after permission, Agent allowlist and parent-identity filtering.
//! Small service catalogs are eager by default; large catalogs remain discoverable.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use zuno_config::schema::mcp::{McpToolExposureConfig, McpToolExposureMode};
use zuno_tool::Tool;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpSchemaMetadata {
    pub id: String,
    pub server: Option<String>,
    pub schema_bytes: usize,
}

impl McpSchemaMetadata {
    pub(crate) fn of(tool: &dyn Tool) -> Self {
        let definition = tool.definition();
        Self {
            id: definition.id.clone(),
            server: tool.source().map(|source| source.name),
            schema_bytes: serde_json::to_vec(&definition.parameters)
                .expect("JSON Value is serializable")
                .len()
                .saturating_add(definition.id.len())
                .saturating_add(definition.description.len()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpExposurePlan {
    pub eager: Vec<String>,
    pub deferred: Vec<String>,
    pub discovery_available: bool,
}

pub(crate) fn resolve(
    config: &McpToolExposureConfig,
    tools: &[McpSchemaMetadata],
    pinned: &BTreeSet<String>,
    discovery_available: bool,
) -> McpExposurePlan {
    let mut eager = BTreeSet::new();
    let mut grouped = BTreeMap::<String, Vec<&McpSchemaMetadata>>::new();
    for tool in tools {
        if pinned.contains(&tool.id) || !discovery_available {
            eager.insert(tool.id.clone());
        } else {
            grouped
                .entry(tool.server.as_deref().unwrap_or("<connected>").to_owned())
                .or_default()
                .push(tool);
        }
    }
    let mut auto_tools = 0usize;
    let mut auto_bytes = 0usize;
    for (server, tools) in grouped {
        let mode = config.servers.get(&server).copied().unwrap_or(config.mode);
        let bytes = tools.iter().fold(0usize, |bytes, tool| {
            bytes.saturating_add(tool.schema_bytes)
        });
        let automatic = mode == McpToolExposureMode::Auto
            && tools.len() <= usize::from(config.small_server_tool_limit.get())
            && auto_tools.saturating_add(tools.len()) <= usize::from(config.auto_tool_limit.get())
            && auto_bytes.saturating_add(bytes) <= config.auto_schema_bytes.get() as usize;
        if mode == McpToolExposureMode::Eager || automatic {
            if automatic {
                auto_tools += tools.len();
                auto_bytes += bytes;
            }
            eager.extend(tools.iter().map(|tool| tool.id.clone()));
        }
    }
    McpExposurePlan {
        eager: tools
            .iter()
            .filter(|tool| eager.contains(&tool.id))
            .map(|tool| tool.id.clone())
            .collect(),
        deferred: tools
            .iter()
            .filter(|tool| !eager.contains(&tool.id))
            .map(|tool| tool.id.clone())
            .collect(),
        discovery_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(server: &str, id: &str, schema_bytes: usize) -> McpSchemaMetadata {
        McpSchemaMetadata {
            id: id.to_owned(),
            server: Some(server.to_owned()),
            schema_bytes,
        }
    }

    #[test]
    fn small_knowledge_servers_are_eager_without_user_selection() {
        let tools = [
            tool("aws-knowledge-mcp-server", "aws_search", 1_024),
            tool("aws-knowledge-mcp-server", "aws_read", 1_024),
            tool("microsoft-learn", "learn_search", 1_024),
        ];
        let plan = resolve(
            &McpToolExposureConfig::default(),
            &tools,
            &BTreeSet::new(),
            true,
        );
        assert_eq!(plan.eager, ["aws_search", "aws_read", "learn_search"]);
        assert!(plan.deferred.is_empty());
    }

    #[test]
    fn large_catalogs_and_schema_bytes_defer_as_whole_services() {
        let mut tools = (0..9)
            .map(|n| tool("large", &format!("large_{n}"), 10))
            .collect::<Vec<_>>();
        tools.push(tool("oversized", "huge", 100_000));
        tools.push(tool("small", "small_read", 10));
        let plan = resolve(
            &McpToolExposureConfig::default(),
            &tools,
            &BTreeSet::new(),
            true,
        );
        assert_eq!(plan.eager, ["small_read"]);
        assert_eq!(plan.deferred.len(), 10);
    }

    #[test]
    fn explicit_pins_and_unreachable_search_never_strand_authorized_tools() {
        let config = McpToolExposureConfig {
            mode: McpToolExposureMode::Deferred,
            ..Default::default()
        };
        let tools = [tool("aws", "aws_search", 10), tool("aws", "aws_read", 10)];
        let pins = BTreeSet::from(["aws_search".to_owned()]);
        let plan = resolve(&config, &tools, &pins, true);
        assert_eq!(plan.eager, ["aws_search"]);
        assert_eq!(plan.deferred, ["aws_read"]);
        assert!(resolve(&config, &tools, &pins, false).deferred.is_empty());
    }
}
