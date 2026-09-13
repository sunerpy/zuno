//! Keep historical Responses input independent of current tool authority.
//!
//! Sealed reasoning binds the assistant output that produced it. Current tool
//! visibility cannot rewrite that output, and hooks cannot edit it as ordinary
//! prose. Only hashes are retained here; error messages never disclose capsules.

use super::{
    ApiSurface, MessageWithParts, ReasoningReplayPolicy, RequestContentBlock, RequestMessage,
    ResolvedModel, Role, StepItem, project_history, sha256_json,
};
use zuno_llm::registry::CompletionRequest;

pub(super) fn output_is_sealed(items: &[StepItem]) -> bool {
    items.iter().any(|item| {
        matches!(item, StepItem::Reasoning(slot)
            if slot.capsule.as_ref().is_some_and(is_sealed_capsule))
    })
}

fn is_sealed_capsule(block: &RequestContentBlock) -> bool {
    matches!(
        block,
        RequestContentBlock::ProviderEncryptedReasoning {
            encrypted_content: Some(capsule), ..
        } if !capsule.is_empty()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HistoricalReplayPolicy {
    Preserve,
    DeclarationBound,
}

impl HistoricalReplayPolicy {
    pub(super) fn resolve(
        model: &ResolvedModel,
        reasoning: ReasoningReplayPolicy,
        default_surface: ApiSurface,
    ) -> Self {
        // A native default already includes the adapter's configuration rules.
        // Some fixed compatible profiles intentionally override Spec.surface.
        let surface = [model.surface, default_surface, model.provider.surface]
            .into_iter()
            .find(|surface| *surface != ApiSurface::Default)
            .unwrap_or(ApiSurface::Default);
        if surface == ApiSurface::Responses || reasoning.requests_encrypted() {
            Self::Preserve
        } else {
            Self::DeclarationBound
        }
    }
}

#[derive(Debug)]
pub(super) struct SealedReplayHistory {
    fingerprints: Vec<String>,
    model_id: String,
    surface: ApiSurface,
}

impl SealedReplayHistory {
    pub(super) fn capture(
        history: &[MessageWithParts],
        model: &ResolvedModel,
    ) -> Result<Self, String> {
        if !history.iter().any(|message| {
            message.parts.iter().any(|part| {
                part.data
                    .get("metadata")
                    .and_then(|metadata| metadata.get("providerReasoning"))
                    .and_then(|reasoning| reasoning.get("encryptedContent"))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|capsule| !capsule.is_empty())
            })
        }) {
            return Ok(Self {
                fingerprints: Vec::new(),
                model_id: model.model_id.clone(),
                surface: model.surface,
            });
        }
        let messages = project_history("", history)
            .into_iter()
            .map(super::ProjectedMessage::into_request_message)
            .collect::<Vec<_>>();
        Ok(Self {
            fingerprints: fingerprints_for(&messages)?,
            model_id: model.model_id.clone(),
            surface: model.surface,
        })
    }

    pub(super) fn validate(&self, messages: &[RequestMessage], stage: &str) -> Result<(), String> {
        if self.fingerprints != fingerprints_for(messages)? {
            return Err(format!(
                "{stage} cannot change sealed reasoning history: preserve assistant output, \
                 encrypted items, their order, role, and input boundaries"
            ));
        }
        Ok(())
    }

    pub(super) fn validate_request(
        &self,
        request: &CompletionRequest,
        stage: &str,
    ) -> Result<(), String> {
        self.validate(&request.messages, stage)?;
        if !self.fingerprints.is_empty()
            && (request.model_id != self.model_id || request.surface != self.surface)
        {
            return Err(format!(
                "{stage} cannot change the target of sealed reasoning replay"
            ));
        }
        if !self.fingerprints.is_empty()
            && ["input", "messages", "model"]
                .iter()
                .any(|key| request.parameters.contains_key(*key))
        {
            return Err(format!(
                "{stage} cannot override sealed reasoning history through request parameters"
            ));
        }
        Ok(())
    }
}

fn fingerprints_for(messages: &[RequestMessage]) -> Result<Vec<String>, String> {
    if messages.iter().any(|message| {
        message.role != Role::Assistant && message.content.iter().any(is_sealed_capsule)
    }) {
        return Err("sealed reasoning history must retain its assistant role".to_owned());
    }
    zuno_llm::registry::sealed_responses_replay_groups(messages)
        .into_iter()
        .map(|(start, end)| {
            serde_json::to_value(&messages[start..=end])
                .map(|value| sha256_json(&value))
                .map_err(|_| "sealed reasoning history could not be fingerprinted".to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zuno_llm::registry::Spec;

    #[test]
    fn native_default_surface_is_used_without_overriding_explicit_routes() {
        for (model_surface, spec_surface, default_surface, expected) in [
            (
                ApiSurface::Default,
                ApiSurface::Default,
                ApiSurface::Responses,
                HistoricalReplayPolicy::Preserve,
            ),
            (
                ApiSurface::Default,
                ApiSurface::Default,
                ApiSurface::Default,
                HistoricalReplayPolicy::DeclarationBound,
            ),
            (
                ApiSurface::Chat,
                ApiSurface::Responses,
                ApiSurface::Responses,
                HistoricalReplayPolicy::DeclarationBound,
            ),
            (
                ApiSurface::Default,
                ApiSurface::Chat,
                ApiSurface::Responses,
                HistoricalReplayPolicy::Preserve,
            ),
            (
                ApiSurface::Responses,
                ApiSurface::Chat,
                ApiSurface::Chat,
                HistoricalReplayPolicy::Preserve,
            ),
        ] {
            let model = ResolvedModel::new(
                Spec::new("fixture").with_surface(spec_surface),
                "fixture-model",
                model_surface,
            );
            assert_eq!(
                HistoricalReplayPolicy::resolve(
                    &model,
                    ReasoningReplayPolicy::default(),
                    default_surface
                ),
                expected,
            );
        }
    }
}
