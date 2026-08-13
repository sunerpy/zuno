use oc_auth::Credential;
use oc_llm::catalog::availability::AvailabilitySource;
use oc_llm::catalog::resolved::{ResolvedModel, ResolvedProvider};
pub(crate) use oc_plugin_sdk::GeneratedClientArrival;
use serde_json::{Map, Value, json};

use crate::{ChatContext, HookInvocation, ProviderSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SdkGeneration {
    Legacy,
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookModelBoundary {
    None,
    ModelSelection,
    LegacyContext,
    LegacyModel,
    V2ProviderAndModel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsModelProjection {
    None,
    ModelSelection,
    LegacySdk,
    V2Sdk,
    LegacyCatalogHttp,
    V2ModelHttp,
    V2ProviderHttp,
    Unbacked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsModelArrival {
    Hook(HookModelBoundary),
    AuthLoader,
    ProviderModels,
    PluginTools,
    AuthMethods,
    WorkspaceRegistration,
    GeneratedClient(GeneratedClientArrival),
}

impl JsModelArrival {
    pub(crate) const fn projection(self) -> JsModelProjection {
        match self {
            Self::Hook(HookModelBoundary::None)
            | Self::PluginTools
            | Self::AuthMethods
            | Self::WorkspaceRegistration => JsModelProjection::None,
            Self::Hook(HookModelBoundary::ModelSelection)
            | Self::GeneratedClient(
                GeneratedClientArrival::EventSubscribe
                | GeneratedClientArrival::GlobalEvent
                | GeneratedClientArrival::V2AgentList
                | GeneratedClientArrival::V2CommandList
                | GeneratedClientArrival::V2EventSubscribe
                | GeneratedClientArrival::V2SessionContext
                | GeneratedClientArrival::V2SessionCreate
                | GeneratedClientArrival::V2SessionEvents
                | GeneratedClientArrival::V2SessionGet
                | GeneratedClientArrival::V2SessionHistory
                | GeneratedClientArrival::V2SessionList
                | GeneratedClientArrival::V2SessionMessage
                | GeneratedClientArrival::V2SessionMessages,
            ) => JsModelProjection::ModelSelection,
            Self::Hook(HookModelBoundary::LegacyContext | HookModelBoundary::LegacyModel)
            | Self::AuthLoader => JsModelProjection::LegacySdk,
            Self::Hook(HookModelBoundary::V2ProviderAndModel) | Self::ProviderModels => {
                JsModelProjection::V2Sdk
            }
            Self::GeneratedClient(GeneratedClientArrival::ConfigProviders) => {
                JsModelProjection::Unbacked
            }
            Self::GeneratedClient(GeneratedClientArrival::ProviderList) => {
                JsModelProjection::LegacyCatalogHttp
            }
            Self::GeneratedClient(GeneratedClientArrival::V2ModelList) => {
                JsModelProjection::V2ModelHttp
            }
            Self::GeneratedClient(
                GeneratedClientArrival::V2ProviderList | GeneratedClientArrival::V2ProviderGet,
            ) => JsModelProjection::V2ProviderHttp,
        }
    }
}

impl HookModelBoundary {
    pub(crate) const fn classify(hook: &HookInvocation<'_>) -> Self {
        match hook {
            HookInvocation::ChatMessage { .. } => Self::ModelSelection,
            HookInvocation::ChatParams { .. }
            | HookInvocation::ChatHeaders { .. }
            | HookInvocation::CompactionAutocontinue { .. } => Self::LegacyContext,
            HookInvocation::ChatSystemTransform { .. } => Self::LegacyModel,
            HookInvocation::ProviderSmallModel { .. } => Self::V2ProviderAndModel,
            HookInvocation::Dispose
            | HookInvocation::Event { .. }
            | HookInvocation::Config { .. }
            | HookInvocation::Tool { .. }
            | HookInvocation::Auth { .. }
            | HookInvocation::Provider { .. }
            | HookInvocation::PermissionAsk { .. }
            | HookInvocation::CommandExecuteBefore { .. }
            | HookInvocation::ToolExecuteBefore { .. }
            | HookInvocation::ShellEnv { .. }
            | HookInvocation::ToolExecuteAfter { .. }
            | HookInvocation::ChatMessagesTransform { .. }
            | HookInvocation::SessionCompacting { .. }
            | HookInvocation::TextComplete { .. }
            | HookInvocation::ToolDefinition { .. } => Self::None,
        }
    }
}

pub(crate) struct SdkValue(Value);

impl SdkValue {
    pub(crate) fn into_json(self) -> Value {
        self.0
    }
}

pub(crate) fn model_value(model: &ResolvedModel, generation: SdkGeneration) -> SdkValue {
    let api = json!({
        "id": model.api.id,
        "url": model.api.url,
        "npm": model.api.npm,
    });
    let capabilities = match generation {
        SdkGeneration::Legacy => json!({
            "temperature": model.capabilities.temperature,
            "reasoning": model.capabilities.reasoning,
            "attachment": model.capabilities.attachment,
            "toolcall": model.capabilities.toolcall,
            "input": model.capabilities.input,
            "output": model.capabilities.output,
        }),
        SdkGeneration::V2 => json!({
            "temperature": model.capabilities.temperature,
            "reasoning": model.capabilities.reasoning,
            "attachment": model.capabilities.attachment,
            "toolcall": model.capabilities.toolcall,
            "input": model.capabilities.input,
            "output": model.capabilities.output,
            "interleaved": model.capabilities.interleaved,
        }),
    };
    let limit = match generation {
        SdkGeneration::Legacy => json!({
            "context": model.limit.context,
            "output": model.limit.output,
        }),
        SdkGeneration::V2 => json!({
            "context": model.limit.context,
            "input": model.limit.input,
            "output": model.limit.output,
        }),
    };
    let mut value = json!({
        "id": model.id,
        "providerID": model.provider_id,
        "api": api,
        "name": model.name,
        "capabilities": capabilities,
        "cost": model.cost,
        "limit": limit,
        "status": model.status,
        "options": model.options,
        "headers": model.headers,
    });
    if generation == SdkGeneration::V2 {
        let fields = value
            .as_object_mut()
            .expect("an SDK model projection is always an object");
        fields.insert("family".to_owned(), Value::String(model.family.clone()));
        fields.insert(
            "release_date".to_owned(),
            Value::String(model.release_date.clone()),
        );
        fields.insert("variants".to_owned(), json!(model.variants));
    }
    SdkValue(value)
}

pub(crate) fn provider_value(
    provider: &ResolvedProvider,
    generation: SdkGeneration,
    source: ProviderSource,
    key: Option<&str>,
) -> SdkValue {
    let models = provider
        .models
        .iter()
        .map(|(id, model)| (id.clone(), model_value(model, generation).into_json()))
        .collect::<Map<_, _>>();
    let mut value = json!({
        "id": provider.id,
        "name": provider.name,
        "source": source_name(source),
        "env": provider.env,
        "options": provider.options,
        "models": models,
    });
    if let Some(key) = key {
        value
            .as_object_mut()
            .expect("an SDK provider projection is always an object")
            .insert("key".to_owned(), Value::String(key.to_owned()));
    }
    SdkValue(value)
}

pub(crate) fn chat_context_value(context: &ChatContext<'_>) -> SdkValue {
    SdkValue(json!({
        "sessionID": context.session_id,
        "agent": context.agent,
        "model": model_value(context.model, SdkGeneration::Legacy).into_json(),
        "provider": {
            "source": source_name(context.provider.source),
            "info": provider_value(
                &context.provider.info,
                SdkGeneration::Legacy,
                context.provider.source,
                None,
            ).into_json(),
            "options": context.provider.options,
        },
        "message": context.message,
    }))
}

pub(crate) fn plugin_model(
    value: Value,
    generation: SdkGeneration,
) -> Result<ResolvedModel, serde_json::Error> {
    serde_json::from_value(plugin_model_value(value, generation))
}

pub(crate) fn plugin_provider(
    value: Value,
    generation: SdkGeneration,
    canonical: &ResolvedProvider,
) -> Result<ResolvedProvider, serde_json::Error> {
    let mut value = value;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("source");
        fields.remove("key");
        fields.insert("availability".to_owned(), json!(canonical.availability));
        if let Some(models) = fields.get_mut("models").and_then(Value::as_object_mut) {
            for model in models.values_mut() {
                *model = plugin_model_value(model.take(), generation);
            }
        }
    }
    let mut provider: ResolvedProvider = serde_json::from_value(value)?;
    for (id, model) in &mut provider.models {
        let Some(original) = canonical.models.get(id) else {
            continue;
        };
        model.api.endpoint = original.api.endpoint;
        if generation == SdkGeneration::Legacy {
            model.family.clone_from(&original.family);
            model.release_date.clone_from(&original.release_date);
            model.variants.clone_from(&original.variants);
            model.capabilities.interleaved = original.capabilities.interleaved.clone();
            model.limit.input = original.limit.input;
        }
    }
    Ok(provider)
}

pub(crate) fn provider_source(provider: &ResolvedProvider) -> ProviderSource {
    match provider.availability.effective_source() {
        Some(AvailabilitySource::EnvVar { .. }) => ProviderSource::Env,
        Some(AvailabilitySource::ConfigBlock) => ProviderSource::Config,
        Some(AvailabilitySource::StoredApiKey) => ProviderSource::Api,
        Some(AvailabilitySource::StoredOauth | AvailabilitySource::StoredWellKnown) | None => {
            ProviderSource::Custom
        }
    }
}

pub(crate) fn credential_key(credential: Option<&Credential>) -> Option<&str> {
    match credential {
        Some(Credential::Api { key, .. } | Credential::WellKnown { key, .. }) => Some(key.expose()),
        Some(Credential::Oauth { .. }) | None => None,
    }
}

fn plugin_model_value(mut value: Value, generation: SdkGeneration) -> Value {
    let Some(fields) = value.as_object_mut() else {
        return value;
    };
    if !fields.contains_key("provider_id")
        && let Some(provider_id) = fields.remove("providerID")
    {
        fields.insert("provider_id".to_owned(), provider_id);
    }
    if generation == SdkGeneration::Legacy {
        fields
            .entry("family".to_owned())
            .or_insert_with(|| Value::String(String::new()));
        fields
            .entry("release_date".to_owned())
            .or_insert_with(|| Value::String(String::new()));
        fields
            .entry("variants".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(capabilities) = fields
            .get_mut("capabilities")
            .and_then(Value::as_object_mut)
        {
            capabilities
                .entry("interleaved".to_owned())
                .or_insert(Value::Bool(false));
        }
    }
    value
}

const fn source_name(source: ProviderSource) -> &'static str {
    match source {
        ProviderSource::Env => "env",
        ProviderSource::Config => "config",
        ProviderSource::Custom => "custom",
        ProviderSource::Api => "api",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_generated_client_model_arrival_has_an_explicit_projection() {
        let projections = GeneratedClientArrival::ALL
            .map(|arrival| JsModelArrival::GeneratedClient(arrival).projection());
        assert_eq!(
            projections,
            [
                JsModelProjection::Unbacked,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::LegacyCatalogHttp,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::V2ModelHttp,
                JsModelProjection::V2ProviderHttp,
                JsModelProjection::V2ProviderHttp,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
                JsModelProjection::ModelSelection,
            ]
        );
    }

    #[test]
    fn every_non_hook_resource_arrival_has_an_explicit_projection() {
        assert_eq!(
            [
                JsModelArrival::AuthLoader.projection(),
                JsModelArrival::ProviderModels.projection(),
                JsModelArrival::PluginTools.projection(),
                JsModelArrival::AuthMethods.projection(),
                JsModelArrival::WorkspaceRegistration.projection(),
            ],
            [
                JsModelProjection::LegacySdk,
                JsModelProjection::V2Sdk,
                JsModelProjection::None,
                JsModelProjection::None,
                JsModelProjection::None,
            ]
        );
    }
}
