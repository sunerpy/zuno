//! Operator-selected, immutable model profiles for private learning.
use crate::{
    Error,
    config::{AgentExecutionMode, Definition},
    invalid,
};
use zuno_application::runtime::ConfigurationRef;
use zuno_learning::distributed::{LearningExecutionLimits, MemoryLearningGrant};

#[derive(Clone)]
pub struct ConfiguredLearning {
    grants: Vec<MemoryLearningGrant>,
}
impl ConfiguredLearning {
    pub fn new(definitions: &[Definition]) -> Result<Self, Error> {
        fn target<'a>(
            definitions: &'a [Definition],
            source: &Definition,
            reference: &ConfigurationRef,
        ) -> Result<&'a Definition, Error> {
            let target = definitions
                .iter()
                .find(|entry| entry.reference() == *reference)
                .ok_or_else(|| invalid("learning requires an installed immutable model profile"))?;
            if target.agent.mode != AgentExecutionMode::Completion
                || target.workspace.id != source.workspace.id
            {
                return Err(invalid(
                    "learning profiles must be completion-only in the same workspace",
                ));
            }
            target.validate()?;
            Ok(target)
        }
        fn limits(definition: &Definition) -> Result<LearningExecutionLimits, Error> {
            let maximum_output_tokens = definition.model.max_output_tokens.get();
            let request_tokens = definition.budget.tokens.get() / 3;
            let input = request_tokens
                .checked_sub(u64::from(maximum_output_tokens) + 1024)
                .ok_or_else(|| {
                    invalid("learning token budget cannot fit one bounded model request")
                })?;
            let limits = LearningExecutionLimits {
                maximum_input_bytes: input
                    .min(definition.model.context_tokens.get().saturating_mul(3))
                    .min(131072) as u32,
                maximum_output_tokens,
                request_tokens,
                total_tokens: definition.budget.tokens.get(),
                maximum_attempts: 3,
                duration_ms: definition
                    .budget
                    .duration_seconds
                    .get()
                    .saturating_mul(1000),
            };
            limits.validate()?;
            Ok(limits)
        }
        let mut grants = Vec::new();
        let identity = |definition: &Definition| zuno_learning::LearningModelIdentity {
            provider_id: definition.model.provider_id.clone(),
            model_id: definition.model.model_id.clone(),
            wire_id: definition.model.model_id.clone(),
        };
        for source in definitions {
            let Some(learning) = &source.memory_learning else {
                continue;
            };
            source.validate()?;
            let extraction = target(definitions, source, &learning.extraction)?;
            let maintenance = target(definitions, source, &learning.maintenance)?;
            grants.push(MemoryLearningGrant {
                source: source.reference(),
                extraction: extraction.reference(),
                maintenance: maintenance.reference(),
                workspace: source.workspace.id.clone(),
                extraction_limits: limits(extraction)?,
                maintenance_limits: limits(maintenance)?,
                extraction_model: identity(extraction),
                maintenance_model: identity(maintenance),
            });
        }
        Ok(Self { grants })
    }
    pub fn grants(&self) -> &[MemoryLearningGrant] {
        &self.grants
    }
    pub fn configurations(&self) -> Vec<ConfigurationRef> {
        let mut result = Vec::new();
        for grant in &self.grants {
            for configured in [&grant.extraction, &grant.maintenance] {
                if !result.contains(configured) {
                    result.push(configured.clone());
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn learning_is_explicit_and_cannot_inherit_an_agent_tool_profile() {
        let mut source: Definition =
            serde_json::from_str(include_str!("../../../enterprise/examples/definition.json"))
                .unwrap();
        assert!(
            ConfiguredLearning::new(&[source.clone()])
                .unwrap()
                .grants()
                .is_empty()
        );
        let mut model = source.clone();
        model.id = zuno_types::identity::ConfigurationId::new("memory-model").unwrap();
        model.agent.mode = AgentExecutionMode::Completion;
        model.environment = None;
        model.budget.duration_seconds = std::num::NonZeroU64::new(300).unwrap();
        source.memory_learning = Some(crate::config::MemoryLearningDefinition {
            extraction: model.reference(),
            maintenance: model.reference(),
        });
        let catalog = ConfiguredLearning::new(&[source.clone(), model.clone()]).unwrap();
        assert_eq!(catalog.configurations(), vec![model.reference()]);
        let mut changed = model.clone();
        changed.agent.system_prompt.push_str(" changed");
        assert!(ConfiguredLearning::new(&[source.clone(), changed]).is_err());
        source.memory_learning = Some(crate::config::MemoryLearningDefinition {
            extraction: source.reference(),
            maintenance: model.reference(),
        });
        assert!(ConfiguredLearning::new(&[source, model]).is_err());
    }
}
