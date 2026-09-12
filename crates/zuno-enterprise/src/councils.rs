//! Immutable Council policy and explicit completion-only model bindings.
use crate::{
    Error,
    children::ConfiguredChildren,
    config::{AgentExecutionMode, Definition},
    invalid,
};
use std::collections::{BTreeMap, BTreeSet};
use zuno_application::{
    child::{ChildDefinitionCatalog, ChildDefinitionGrant, ChildWorkspacePolicy},
    council::{CouncilDefinitionCatalog, CouncilDefinitionGrant, CouncilRules},
    runtime::ConfigurationRef,
};

#[derive(Clone, Default)]
pub(crate) struct ConfiguredCouncils {
    entries: Vec<CouncilDefinitionGrant>,
}
impl ConfiguredCouncils {
    pub fn new(definitions: &[Definition], children: &ConfiguredChildren) -> Result<Self, Error> {
        let mut entries = Vec::new();
        for parent in definitions {
            if parent.councils.is_empty() {
                continue;
            }
            let policy = parent
                .delegation
                .as_ref()
                .ok_or_else(|| invalid("Council requires explicit child targets"))?;
            if parent.councils.len() > 32 || policy.maximum_depth < 2 {
                return Err(invalid(
                    "Council template count or delegation depth is invalid",
                ));
            }
            let mut names = BTreeSet::new();
            for council in &parent.councils {
                zuno_engine::council::validate_policy(&council.preset)
                    .map_err(|_| invalid("invalid Council policy"))?;
                if !names.insert(&council.preset.name)
                    || council.preset.seats.len() * (council.preset.retry_policy.max_retries + 1)
                        + 1
                        > policy.maximum_children as usize
                {
                    return Err(invalid(
                        "Council exceeds its configured native child budget",
                    ));
                }
                let resolve_completion = |reference: &ConfigurationRef| -> Result<(&Definition, ChildDefinitionGrant), Error> {
                    let definition = definitions.iter().find(|definition| definition.reference() == *reference)
                        .ok_or_else(|| invalid("Council completion definition is not installed"))?;
                    if definition.agent.mode != AgentExecutionMode::Completion {
                        return Err(invalid("Council repair and synthesis require completion mode"));
                    }
                    let grant = children.resolve(&parent.reference(), &definition.agent.name, None)
                        .ok_or_else(|| invalid("Council completion is outside the parent's child allowlist"))?;
                    if grant.child != *reference { return Err(invalid("Council completion reference changed")); }
                    Ok((definition, grant))
                };
                let (_, synthesis) = resolve_completion(&council.synthesis)?;
                let mut seats = BTreeMap::new();
                let mut repairs = BTreeMap::new();
                for seat in &council.preset.seats {
                    let grant = children
                        .resolve(&parent.reference(), &seat.agent, None)
                        .ok_or_else(|| {
                            invalid("Council seat is outside the parent's child allowlist")
                        })?;
                    if council.preset.retry_policy.max_retries > 0 {
                        let reference = council.repairs.get(&seat.agent).ok_or_else(|| invalid("Council retries require an explicit completion-only repair profile"))?;
                        let (repair, repair_grant) = resolve_completion(reference)?;
                        let original = definitions
                            .iter()
                            .find(|definition| definition.reference() == grant.child)
                            .ok_or_else(|| invalid("Council seat definition is unavailable"))?;
                        if serde_json::to_value(&original.model)
                            .map_err(|_| invalid("invalid Council seat model"))?
                            != serde_json::to_value(&repair.model)
                                .map_err(|_| invalid("invalid Council repair model"))?
                        {
                            return Err(invalid(
                                "Council repair must preserve its seat's model binding",
                            ));
                        }
                        repairs.insert(seat.id.clone(), repair_grant);
                    }
                    seats.insert(seat.id.clone(), grant);
                }
                entries.push(CouncilDefinitionGrant {
                    group: ChildDefinitionGrant {
                        parent: parent.reference(),
                        child: parent.reference(),
                        selection: parent.selection(),
                        maximum_depth: policy.maximum_depth,
                        maximum_children: policy.maximum_children,
                        workspace: ChildWorkspacePolicy::ForkParent,
                    },
                    rules: CouncilRules {
                        preset: council.preset.clone(),
                        repairs,
                    },
                    seats,
                    synthesis,
                });
            }
        }
        Ok(Self { entries })
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn presets(
        &self,
        parent: &ConfigurationRef,
    ) -> Vec<zuno_orchestration::CouncilPresetDescriptor> {
        self.entries
            .iter()
            .filter(|entry| entry.group.parent == *parent)
            .map(|entry| entry.rules.preset.clone())
            .collect()
    }
}
impl CouncilDefinitionCatalog for ConfiguredCouncils {
    fn resolve(&self, parent: &ConfigurationRef, preset: &str) -> Option<CouncilDefinitionGrant> {
        self.entries
            .iter()
            .find(|entry| entry.group.parent == *parent && entry.rules.preset.name == preset)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CouncilDefinition, DelegationDefinition};
    use zuno_orchestration::{
        CouncilPresetDescriptor, CouncilRetryPolicyDescriptor, CouncilSeatDescriptor,
        CouncilSynthesisPolicyDescriptor,
    };
    use zuno_types::identity::ConfigurationId;

    fn definitions() -> Vec<Definition> {
        let mut root: Definition =
            serde_json::from_str(include_str!("../../../enterprise/examples/definition.json"))
                .unwrap();
        let mut seat = root.clone();
        seat.id = ConfigurationId::new("seat").unwrap();
        seat.agent.name = "seat".to_owned();
        let mut completion = seat.clone();
        completion.id = ConfigurationId::new("completion").unwrap();
        completion.agent.name = "completion".to_owned();
        completion.agent.mode = AgentExecutionMode::Completion;
        completion.environment = None;
        root.delegation = Some(DelegationDefinition {
            targets: vec![seat.reference(), completion.reference()],
            maximum_depth: 2,
            maximum_children: 4,
        });
        root.councils = vec![CouncilDefinition {
            preset: CouncilPresetDescriptor {
                name: "inspect".to_owned(),
                source_id: "fixture:inspect".to_owned(),
                quorum: 1,
                max_parallel: 1,
                deadline_ms: 60000,
                seat_output_bytes: 8192,
                retry_policy: CouncilRetryPolicyDescriptor { max_retries: 1 },
                synthesis_policy: CouncilSynthesisPolicyDescriptor {
                    timeout_ms: 10000,
                    max_input_bytes: 32768,
                },
                seats: vec![CouncilSeatDescriptor {
                    id: "source".to_owned(),
                    agent: "seat".to_owned(),
                    instruction: "Inspect source evidence".to_owned(),
                }],
            },
            synthesis: completion.reference(),
            repairs: [("seat".to_owned(), completion.reference())].into(),
        }];
        vec![root, seat, completion]
    }
    #[test]
    fn council_bindings_keep_repairs_and_synthesis_model_only_with_the_original_model() {
        let definitions = definitions();
        let children = ConfiguredChildren::new(&definitions).unwrap();
        let catalog = ConfiguredCouncils::new(&definitions, &children).unwrap();
        let resolved = catalog
            .resolve(&definitions[0].reference(), "inspect")
            .unwrap();
        assert_eq!(
            resolved.seats["source"].workspace,
            ChildWorkspacePolicy::ForkParent
        );
        assert_eq!(
            resolved.rules.repairs["source"].workspace,
            ChildWorkspacePolicy::ModelOnly
        );
        assert_eq!(
            resolved.synthesis.workspace,
            ChildWorkspacePolicy::ModelOnly
        );
        assert!(
            catalog
                .resolve(&definitions[1].reference(), "inspect")
                .is_none()
        );
    }
    #[test]
    fn council_cannot_expand_capacity_repeat_tools_or_change_the_repair_model() {
        for case in 0..5 {
            let mut definitions = definitions();
            match case {
                0 => definitions[0].delegation.as_mut().unwrap().maximum_children = 2,
                1 => {
                    definitions[2].agent.mode = AgentExecutionMode::Agent;
                    definitions[2].environment = definitions[1].environment.clone();
                }
                2 => definitions[2].model.model_id = "different-model".to_owned(),
                3 => definitions[0].councils[0].repairs.clear(),
                _ => {
                    definitions[0].councils[0]
                        .preset
                        .synthesis_policy
                        .timeout_ms = 60000
                }
            }
            let completion = definitions[2].reference();
            definitions[0].delegation.as_mut().unwrap().targets[1] = completion.clone();
            definitions[0].councils[0].synthesis = completion.clone();
            if case != 3 {
                definitions[0].councils[0]
                    .repairs
                    .insert("seat".to_owned(), completion);
            }
            let children = ConfiguredChildren::new(&definitions).unwrap();
            assert!(
                ConfiguredCouncils::new(&definitions, &children).is_err(),
                "case {case}"
            );
        }
    }
}
