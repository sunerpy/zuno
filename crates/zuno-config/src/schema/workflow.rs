//! Validated, named multi-agent workflow templates.

use crate::schema::agent::AgentConfig;
use crate::schema::ordered::OrderedMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Default upper bound for simultaneously running workflow nodes.
pub const DEFAULT_MAX_PARALLEL: u8 = 4;
/// Default upper bound for nodes in one workflow run.
pub const DEFAULT_MAX_AGENTS: u8 = 12;
/// Hard safety bound for either workflow limit.
pub const MAX_WORKFLOW_LIMIT: u8 = 64;

/// One node in a configured workflow DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNodeConfig {
    /// Stable node id within the template.
    pub id: String,
    /// Configured or built-in agent to run.
    pub agent: String,
    /// Optional node-specific instruction appended to the run prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Human-readable purpose shown in runtime projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Node ids that must complete successfully first.
    #[serde(rename = "dependsOn", default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// A named, configuration-owned workflow template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentWorkflowConfig {
    /// Maximum simultaneously running nodes. Defaults to four.
    #[serde(
        rename = "maxParallel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_parallel: Option<u8>,
    /// Maximum nodes admitted into one run. Defaults to twelve.
    #[serde(rename = "maxAgents", default, skip_serializing_if = "Option::is_none")]
    pub max_agents: Option<u8>,
    /// The immutable DAG instantiated by the model-facing workflow tool.
    pub nodes: Vec<WorkflowNodeConfig>,
}

impl AgentWorkflowConfig {
    /// Effective simultaneous-node limit.
    #[must_use]
    pub fn resolved_max_parallel(&self) -> usize {
        usize::from(self.max_parallel.unwrap_or(DEFAULT_MAX_PARALLEL))
    }

    /// Effective per-run node limit.
    #[must_use]
    pub fn resolved_max_agents(&self) -> usize {
        usize::from(self.max_agents.unwrap_or(DEFAULT_MAX_AGENTS))
    }

    /// Validate limits, identities, dependencies, cycles, and agent references.
    pub fn validate(&self, workflow: &str, agents: &OrderedMap<AgentConfig>) -> Result<(), String> {
        self.validate_structure(workflow)?;
        for node in &self.nodes {
            let Some(agent) = agents.get(&node.agent) else {
                return Err(format!(
                    "workflows.{workflow}.nodes.{} references unknown agent `{}`",
                    node.id, node.agent
                ));
            };
            if agent.disable == Some(true) {
                return Err(format!(
                    "workflows.{workflow}.nodes.{} references disabled agent `{}`",
                    node.id, node.agent
                ));
            }
        }
        Ok(())
    }

    /// Validate everything not requiring the resolved agent catalog.
    pub fn validate_structure(&self, workflow: &str) -> Result<(), String> {
        if workflow.trim().is_empty() {
            return Err("workflow names must not be empty".to_owned());
        }
        let max_parallel = self.max_parallel.unwrap_or(DEFAULT_MAX_PARALLEL);
        let max_agents = self.max_agents.unwrap_or(DEFAULT_MAX_AGENTS);
        for (field, value) in [("maxParallel", max_parallel), ("maxAgents", max_agents)] {
            if !(1..=MAX_WORKFLOW_LIMIT).contains(&value) {
                return Err(format!(
                    "workflows.{workflow}.{field} must be between 1 and {MAX_WORKFLOW_LIMIT}"
                ));
            }
        }
        if max_parallel > max_agents {
            return Err(format!(
                "workflows.{workflow}.maxParallel cannot exceed maxAgents"
            ));
        }
        if self.nodes.is_empty() {
            return Err(format!("workflows.{workflow}.nodes must not be empty"));
        }
        if self.nodes.len() > usize::from(max_agents) {
            return Err(format!(
                "workflows.{workflow} declares {} nodes but maxAgents is {max_agents}",
                self.nodes.len()
            ));
        }

        let mut nodes = BTreeMap::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() || node.agent.trim().is_empty() {
                return Err(format!(
                    "workflows.{workflow} node id and agent must not be empty"
                ));
            }
            if node
                .prompt
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(format!(
                    "workflows.{workflow}.nodes.{}.prompt must not be empty when present",
                    node.id
                ));
            }
            if nodes.insert(node.id.as_str(), node).is_some() {
                return Err(format!(
                    "workflows.{workflow} contains duplicate node id `{}`",
                    node.id
                ));
            }
            let unique = node.depends_on.iter().collect::<BTreeSet<_>>();
            if unique.len() != node.depends_on.len() {
                return Err(format!(
                    "workflows.{workflow}.nodes.{} contains duplicate dependencies",
                    node.id
                ));
            }
        }
        for node in &self.nodes {
            for dependency in &node.depends_on {
                if dependency == &node.id {
                    return Err(format!(
                        "workflows.{workflow}.nodes.{} cannot depend on itself",
                        node.id
                    ));
                }
                if !nodes.contains_key(dependency.as_str()) {
                    return Err(format!(
                        "workflows.{workflow}.nodes.{} references missing dependency `{dependency}`",
                        node.id
                    ));
                }
            }
        }

        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for id in nodes.keys().copied() {
            visit(workflow, id, &nodes, &mut visiting, &mut visited)?;
        }
        Ok(())
    }
}

fn visit<'a>(
    workflow: &str,
    id: &'a str,
    nodes: &BTreeMap<&'a str, &'a WorkflowNodeConfig>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), String> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(format!(
            "workflows.{workflow} contains a dependency cycle through `{id}`"
        ));
    }
    if let Some(node) = nodes.get(id) {
        for dependency in &node.depends_on {
            visit(workflow, dependency, nodes, visiting, visited)?;
        }
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agents() -> OrderedMap<AgentConfig> {
        serde_json::from_value(serde_json::json!({
            "researcher": {"model":"myopenai/gpt-5.6-sol"},
            "implementer": {"model":"myopenai/gpt-5.6-sol"}
        }))
        .expect("agents")
    }

    fn workflow() -> AgentWorkflowConfig {
        serde_json::from_value(serde_json::json!({
            "maxParallel": 2,
            "maxAgents": 4,
            "nodes": [
                {"id":"scan","agent":"researcher"},
                {"id":"review","agent":"researcher"},
                {"id":"implement","agent":"implementer","dependsOn":["scan","review"]},
                {"id":"verify","agent":"implementer","dependsOn":["implement"]}
            ]
        }))
        .expect("workflow")
    }

    #[test]
    fn validates_a_bounded_agent_dag() {
        workflow()
            .validate("release-hardening", &agents())
            .expect("valid");
    }

    #[test]
    fn rejects_cycles_unknown_agents_and_unsafe_limits() {
        let mut cyclic = workflow();
        cyclic.nodes[0].depends_on.push("verify".to_owned());
        assert!(
            cyclic
                .validate("release", &agents())
                .unwrap_err()
                .contains("cycle")
        );

        let mut unknown = workflow();
        unknown.nodes[0].agent = "missing".to_owned();
        assert!(
            unknown
                .validate("release", &agents())
                .unwrap_err()
                .contains("unknown agent")
        );

        let mut unbounded = workflow();
        unbounded.max_parallel = Some(65);
        assert!(
            unbounded
                .validate("release", &agents())
                .unwrap_err()
                .contains("between 1 and 64")
        );
    }
}
