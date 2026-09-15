use crate::WorkflowError;
use crate::WorkflowLimits;
use crate::WorkflowNode;
use crate::WorkflowRoute;
use crate::validate_identity;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

mod engine;
mod node;
pub use engine::GRAPH_ENGINE_REVISION;
pub use engine::GraphWorkflowEngine;

/// Dependency indices and deterministic topological stages for `graph/v1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledGraph {
    dependencies: Vec<Vec<usize>>,
    stages: Vec<Vec<usize>>,
    terminal_nodes: Vec<usize>,
}

impl CompiledGraph {
    pub(crate) fn compile(
        nodes: &[WorkflowNode],
        routes: &BTreeMap<String, WorkflowRoute>,
        limits: &WorkflowLimits,
    ) -> Result<Self, WorkflowError> {
        if nodes.is_empty() || nodes.len() > limits.max_total_agents as usize {
            return Err(WorkflowError::InvalidProgram {
                engine: crate::WorkflowEngine::GraphV1,
                message: format!(
                    "node count {} must be between 1 and maxTotalAgents {}",
                    nodes.len(),
                    limits.max_total_agents
                ),
            });
        }
        let mut indices = BTreeMap::new();
        for (index, node) in nodes.iter().enumerate() {
            validate_identity("workflow node id", &node.id)?;
            validate_identity("workflow node route", &node.route)?;
            if !routes.contains_key(&node.route) {
                return Err(WorkflowError::UnknownRoute {
                    node: node.id.clone(),
                    route: node.route.clone(),
                });
            }
            if indices.insert(node.id.as_str(), index).is_some() {
                return Err(WorkflowError::DuplicateNode(node.id.clone()));
            }
        }
        let mut dependencies = Vec::with_capacity(nodes.len());
        for (index, node) in nodes.iter().enumerate() {
            let mut unique = BTreeSet::new();
            let mut resolved = Vec::with_capacity(node.needs.len());
            for dependency in &node.needs {
                validate_identity("workflow dependency", dependency)?;
                let Some(dependency_index) = indices.get(dependency.as_str()).copied() else {
                    return Err(WorkflowError::MissingDependency {
                        node: node.id.clone(),
                        dependency: dependency.clone(),
                    });
                };
                if dependency_index == index {
                    return Err(WorkflowError::SelfDependency(node.id.clone()));
                }
                if !unique.insert(dependency_index) {
                    return Err(WorkflowError::DuplicateDependency {
                        node: node.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
                resolved.push(dependency_index);
            }
            dependencies.push(resolved);
        }
        let stages = topological_stages(&dependencies)?;
        let depended_on = dependencies
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        let terminal_nodes = (0..nodes.len())
            .filter(|index| !depended_on.contains(index))
            .collect();
        Ok(Self {
            dependencies,
            stages,
            terminal_nodes,
        })
    }

    pub fn dependencies(&self, index: usize) -> Option<&[usize]> {
        self.dependencies.get(index).map(Vec::as_slice)
    }

    pub fn stages(&self) -> &[Vec<usize>] {
        &self.stages
    }

    pub fn terminal_nodes(&self) -> &[usize] {
        &self.terminal_nodes
    }
}

fn topological_stages(dependencies: &[Vec<usize>]) -> Result<Vec<Vec<usize>>, WorkflowError> {
    let mut emitted = vec![false; dependencies.len()];
    let mut stages = Vec::new();
    while emitted.iter().any(|done| !done) {
        let stage = dependencies
            .iter()
            .enumerate()
            .filter(|(index, deps)| {
                !emitted[*index] && deps.iter().all(|dependency| emitted[*dependency])
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if stage.is_empty() {
            return Err(WorkflowError::DependencyCycle);
        }
        for index in &stage {
            emitted[*index] = true;
        }
        stages.push(stage);
    }
    Ok(stages)
}

#[cfg(test)]
#[path = "graph/tests.rs"]
mod tests;
