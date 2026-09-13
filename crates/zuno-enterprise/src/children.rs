//! Immutable, composition-owned child target and model bindings.

use crate::{Error, config::Definition, invalid};
use std::collections::{BTreeMap, BTreeSet};
use zuno_application::{
    child::{ChildDefinitionCatalog, ChildDefinitionGrant, ChildWorkspacePolicy},
    runtime::ConfigurationRef,
};
use zuno_config::schema::provider::ProviderTransport;
use zuno_llm::effort::{EffortCapabilities, ProviderFamily};
use zuno_worker::child::ChildToolTarget;

#[derive(Clone, Default)]
pub(crate) struct ConfiguredChildren {
    entries: Vec<Entry>,
}
#[derive(Clone)]
struct Entry {
    grant: ChildDefinitionGrant,
    facts: zuno_tools::ModelFacts,
}
impl ConfiguredChildren {
    pub fn new(definitions: &[Definition]) -> Result<Self, Error> {
        let mut entries = Vec::new();
        for parent in definitions {
            let Some(policy) = &parent.delegation else {
                continue;
            };
            let mut agents = BTreeSet::new();
            for target in &policy.targets {
                let child = definitions
                    .iter()
                    .find(|definition| {
                        definition.id == target.id && definition.version == target.version
                    })
                    .ok_or_else(|| invalid("an allowed child definition is not installed"))?;
                if child.reference() != *target {
                    return Err(invalid(
                        "child definition digest changed without updating its parent snapshot",
                    ));
                }
                if child.workspace.id != parent.workspace.id {
                    return Err(invalid(
                        "child definitions must retain the parent's logical workspace",
                    ));
                }
                let workspace = if child.agent.mode == crate::config::AgentExecutionMode::Completion
                {
                    ChildWorkspacePolicy::ModelOnly
                } else if let (Some(parent), Some(child)) =
                    (&parent.environment, &child.environment)
                    && (child.gateway_id != parent.gateway_id || child.endpoint == parent.endpoint)
                {
                    ChildWorkspacePolicy::ForkParent
                } else {
                    return Err(invalid(
                        "child workspace forks require configured environments and consistent gateway endpoints",
                    ));
                };
                if !agents.insert(child.agent.name.clone()) {
                    return Err(invalid(
                        "child Agent names must resolve to one immutable definition",
                    ));
                }
                zuno_tools::task::DelegationTargets::new([child.agent.name.clone()])
                    .map_err(|_| invalid("invalid child Agent target"))?;
                let family = match child.model.transport {
                    ProviderTransport::Anthropic | ProviderTransport::GoogleVertexAnthropic => {
                        ProviderFamily::Anthropic
                    }
                    ProviderTransport::Bedrock | ProviderTransport::BedrockRuntime => {
                        ProviderFamily::Bedrock
                    }
                    ProviderTransport::Google | ProviderTransport::GoogleVertex => {
                        ProviderFamily::Google
                    }
                    ProviderTransport::Openrouter => ProviderFamily::OpenRouter,
                    _ => ProviderFamily::OpenAi,
                };
                // Definitions own model options. The ordinary task schema does
                // not expose a caller-selected effort/variant override.
                entries.push(Entry {
                    grant: ChildDefinitionGrant {
                        parent: parent.reference(),
                        child: child.reference(),
                        selection: child.selection(),
                        maximum_depth: policy.maximum_depth,
                        maximum_children: policy.maximum_children,
                        workspace,
                    },
                    facts: zuno_tools::ModelFacts {
                        family,
                        reasoning: false,
                        effort: EffortCapabilities::default(),
                        variants: BTreeMap::new(),
                    },
                });
            }
        }
        Ok(Self { entries })
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn targets(
        &self,
        parent: &ConfigurationRef,
    ) -> Option<(u32, BTreeMap<String, ChildToolTarget>)> {
        let mut targets = BTreeMap::new();
        let mut depth = None;
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.grant.parent == *parent)
        {
            depth = Some(entry.grant.maximum_depth);
            targets.insert(
                entry.grant.selection.agent.clone(),
                ChildToolTarget {
                    model: format!(
                        "{}/{}",
                        entry.grant.selection.model.provider_id,
                        entry.grant.selection.model.model_id
                    ),
                    facts: entry.facts.clone(),
                },
            );
        }
        depth.map(|depth| (depth, targets))
    }
}
impl ChildDefinitionCatalog for ConfiguredChildren {
    fn resolve(
        &self,
        parent: &ConfigurationRef,
        agent: &str,
        model: Option<&str>,
    ) -> Option<ChildDefinitionGrant> {
        self.entries
            .iter()
            .find(|entry| {
                entry.grant.parent == *parent
                    && entry.grant.selection.agent == agent
                    && model.is_none_or(|model| {
                        model
                            == format!(
                                "{}/{}",
                                entry.grant.selection.model.provider_id,
                                entry.grant.selection.model.model_id
                            )
                    })
            })
            .map(|entry| entry.grant.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DelegationDefinition;
    use zuno_types::identity::ConfigurationId;

    fn definitions() -> (Definition, Definition) {
        let mut parent: Definition =
            serde_json::from_str(include_str!("../../../enterprise/examples/definition.json"))
                .unwrap();
        let mut child = parent.clone();
        child.id = ConfigurationId::new("helper").unwrap();
        child.agent.name = "helper".to_owned();
        parent.delegation = Some(DelegationDefinition {
            targets: vec![child.reference()],
            maximum_depth: 2,
            maximum_children: 4,
        });
        (parent, child)
    }

    #[test]
    fn parent_snapshot_pins_child_bytes_and_caller_cannot_select_another_model() {
        let (parent, child) = definitions();
        let catalog = ConfiguredChildren::new(&[parent.clone(), child.clone()]).unwrap();
        let grant = catalog
            .resolve(&parent.reference(), "helper", None)
            .unwrap();
        assert_eq!(grant.child, child.reference());
        assert!(
            catalog
                .resolve(&parent.reference(), "helper", Some("unapproved/model"))
                .is_none()
        );
        let mut changed = child;
        changed.agent.system_prompt.push_str(" changed");
        assert!(
            ConfiguredChildren::new(&[parent, changed]).is_err(),
            "same ID/version is insufficient when child bytes changed"
        );
    }

    #[test]
    fn a_completion_child_receives_no_environment_grant() {
        let (mut parent, mut child) = definitions();
        child.agent.mode = crate::config::AgentExecutionMode::Completion;
        child.environment = None;
        child.validate().unwrap();
        parent.delegation.as_mut().unwrap().targets = vec![child.reference()];
        let catalog = ConfiguredChildren::new(&[parent.clone(), child]).unwrap();
        assert_eq!(
            catalog
                .resolve(&parent.reference(), "helper", None)
                .unwrap()
                .workspace,
            ChildWorkspacePolicy::ModelOnly
        );
    }

    #[test]
    fn inherited_workspace_must_resolve_to_the_same_docker_owner() {
        let (mut parent, mut child) = definitions();
        child.environment.as_mut().unwrap().endpoint = "https://other-gateway.example/".to_owned();
        parent.delegation.as_mut().unwrap().targets = vec![child.reference()];
        assert!(ConfiguredChildren::new(&[parent, child]).is_err());
    }

    #[test]
    fn child_workspace_can_use_another_configured_gateway() {
        let (mut parent, mut child) = definitions();
        let environment = child.environment.as_mut().unwrap();
        environment.gateway_id = zuno_types::identity::GatewayId::new("remote-gateway").unwrap();
        environment.endpoint = "https://other-gateway.example/".to_owned();
        parent.delegation.as_mut().unwrap().targets = vec![child.reference()];
        let catalog = ConfiguredChildren::new(&[parent.clone(), child.clone()])
            .expect("a configured remote child receives a verified snapshot of the parent");
        let grant = catalog
            .resolve(&parent.reference(), "helper", None)
            .unwrap();
        assert_eq!(grant.child, child.reference());
        assert_eq!(grant.workspace, ChildWorkspacePolicy::ForkParent);
    }
}
