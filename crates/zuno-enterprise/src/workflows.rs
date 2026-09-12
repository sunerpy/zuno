//! The installed parent definition owns the DAG, targets and all capacity limits.
use super::children::ConfiguredChildren;
use crate::{Error, config::Definition, invalid};
use std::collections::{BTreeMap, BTreeSet};
use zuno_application::{
    child::{ChildDefinitionCatalog, ChildDefinitionGrant, ChildWorkspacePolicy},
    runtime::ConfigurationRef,
    workflow::{WorkflowDefinitionCatalog, WorkflowDefinitionGrant},
};
use zuno_engine::workflow::WorkflowGraph;

#[derive(Clone, Default)]
pub(crate) struct ConfiguredWorkflows {
    entries: Vec<WorkflowDefinitionGrant>,
}
impl ConfiguredWorkflows {
    pub fn new(definitions: &[Definition], children: &ConfiguredChildren) -> Result<Self, Error> {
        let mut entries = Vec::new();
        for parent in definitions {
            if parent.workflows.is_empty() {
                continue;
            }
            if parent.workflows.len() > 32 {
                return Err(invalid("a definition supports at most 32 workflows"));
            }
            let policy = parent
                .delegation
                .as_ref()
                .ok_or_else(|| invalid("workflows require explicit child target bindings"))?;
            let mut names = BTreeSet::new();
            for template in &parent.workflows {
                if template.name.trim().is_empty()
                    || template.name.trim() != template.name
                    || template.name.len() > 256
                    || template.name.chars().any(char::is_control)
                    || template.source_id.trim().is_empty()
                    || template.source_id.len() > 2048
                    || template.source_id.chars().any(char::is_control)
                    || !names.insert(template.name.clone())
                    || template.nodes.len() > template.max_agents
                    || template.max_agents > policy.maximum_children as usize
                    || template.max_agents > 64
                    || template.max_parallel > template.max_agents
                    || policy.maximum_depth < 2
                {
                    return Err(invalid(
                        "invalid workflow definition, capacity or delegation depth",
                    ));
                }
                WorkflowGraph::new(
                    template
                        .nodes
                        .iter()
                        .map(|node| (node.id.clone(), node.depends_on.clone())),
                    template.max_parallel,
                )
                .map_err(|_| invalid("invalid workflow DAG"))?;
                let mut nodes = BTreeMap::new();
                for node in &template.nodes {
                    if node.prompt.as_ref().is_some_and(|text| {
                        text.trim().is_empty()
                            || text.len() > zuno_application::MAX_INPUT_BYTES
                            || text.contains('\0')
                    }) || node.description.as_ref().is_some_and(|text| {
                        text.trim().is_empty() || text.len() > 4096 || text.contains('\0')
                    }) {
                        return Err(invalid("workflow node instructions exceed their bounds"));
                    }
                    let grant = children
                        .resolve(&parent.reference(), &node.agent, None)
                        .ok_or_else(|| {
                            invalid("workflow node is outside the fixed child catalog")
                        })?;
                    nodes.insert(node.id.clone(), grant);
                }
                entries.push(WorkflowDefinitionGrant {
                    council: None,
                    group: ChildDefinitionGrant {
                        parent: parent.reference(),
                        child: parent.reference(),
                        selection: parent.selection(),
                        maximum_depth: policy.maximum_depth,
                        maximum_children: policy.maximum_children,
                        workspace: ChildWorkspacePolicy::ForkParent,
                    },
                    template: template.clone(),
                    nodes,
                });
            }
        }
        Ok(Self { entries })
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn templates(
        &self,
        parent: &ConfigurationRef,
    ) -> Vec<zuno_orchestration::WorkflowTemplateDescriptor> {
        self.entries
            .iter()
            .filter(|entry| entry.group.parent == *parent)
            .map(|entry| entry.template.clone())
            .collect()
    }
}

impl WorkflowDefinitionCatalog for ConfiguredWorkflows {
    fn resolve(
        &self,
        parent: &ConfigurationRef,
        template: &str,
    ) -> Option<WorkflowDefinitionGrant> {
        self.entries
            .iter()
            .find(|entry| entry.group.parent == *parent && entry.template.name == template)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DelegationDefinition;
    use zuno_types::identity::ConfigurationId;
    fn definitions() -> Vec<Definition> {
        let mut root: Definition =
            serde_json::from_str(include_str!("../../../enterprise/examples/definition.json"))
                .unwrap();
        let mut child = root.clone();
        child.id = ConfigurationId::new("helper").unwrap();
        child.agent.name = "helper".to_owned();
        root.delegation = Some(DelegationDefinition {
            targets: vec![child.reference()],
            maximum_depth: 2,
            maximum_children: 4,
        });
        root.workflows = vec![zuno_orchestration::WorkflowTemplateDescriptor {
            name: "inspect".to_owned(),
            source_id: "fixture:inspect".to_owned(),
            max_parallel: 1,
            max_agents: 2,
            nodes: ["read", "review"]
                .into_iter()
                .enumerate()
                .map(|(index, id)| zuno_orchestration::WorkflowNodeDescriptor {
                    id: id.to_owned(),
                    agent: "helper".to_owned(),
                    prompt: None,
                    description: None,
                    depends_on: if index == 0 {
                        vec![]
                    } else {
                        vec!["read".to_owned()]
                    },
                })
                .collect(),
        }];
        vec![root, child]
    }
    #[test]
    fn templates_pin_nodes_to_the_original_child_definition_and_parent_snapshot() {
        let definitions = definitions();
        let children = ConfiguredChildren::new(&definitions).unwrap();
        let workflows = ConfiguredWorkflows::new(&definitions, &children).unwrap();
        let grant = workflows
            .resolve(&definitions[0].reference(), "inspect")
            .unwrap();
        assert_eq!(grant.nodes["read"].child, definitions[1].reference());
        assert_eq!(grant.group.child, definitions[0].reference());
        assert!(
            workflows
                .resolve(&definitions[1].reference(), "inspect")
                .is_none()
        );
        assert!(
            workflows
                .resolve(&definitions[0].reference(), "invented")
                .is_none()
        );
    }
    #[test]
    fn cycles_unapproved_agents_and_capacity_expansion_fail_at_configuration() {
        for case in 0..4 {
            let mut definitions = definitions();
            match case {
                0 => definitions[0].workflows[0].nodes[0]
                    .depends_on
                    .push("review".to_owned()),
                1 => definitions[0].workflows[0].nodes[0].agent = "unapproved".to_owned(),
                2 => definitions[0].workflows[0].max_agents = 5,
                _ => definitions[0].delegation.as_mut().unwrap().maximum_depth = 1,
            }
            let children = ConfiguredChildren::new(&definitions).unwrap();
            assert!(ConfiguredWorkflows::new(&definitions, &children).is_err());
        }
    }
}
