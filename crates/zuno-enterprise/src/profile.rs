//! Validated immutable definitions composed with the native provider factories
//! and shared bounded driver. No local project/config/credential discovery.

use crate::{
    Error,
    config::{self, BudgetDefinition, Definition, ModelCredential},
    invalid,
};
use async_trait::async_trait;
use std::{collections::BTreeMap, num::NonZeroU32, sync::Arc, time::Duration};
use zuno_application::runtime::ConfigurationRef;
use zuno_auth::Credential;
use zuno_config::schema::provider::{ProviderSurface, ProviderTransport};
use zuno_engine::{
    budget::{
        BudgetDecision, BudgetPolicyError, TurnAllowance, TurnBudgetPolicy, TurnUsageSnapshot,
    },
    driver::AgentDriver,
    r#loop::{AgentModelResolver, ResolvedAgent, ResolvedModel},
};
use zuno_llm::{
    cache::DynamicContext,
    registry::{ApiSurface, ProviderRegistry, Spec},
};
use zuno_worker::{
    WorkerClient, WorkerExecution,
    gateway::GatewayClient,
    memory::{MemoryContextRefresher, MemoryToolDispatcher},
    runtime::{WorkerError, WorkerServiceFactory, WorkerTurnServices},
    tools::GatewayToolDispatcher,
};

struct Resolver {
    agent: ResolvedAgent,
    model: ResolvedModel,
}
impl AgentModelResolver for Resolver {
    fn resolve_agent(&self, name: &str) -> Option<ResolvedAgent> {
        (name == self.agent.name).then(|| self.agent.clone())
    }
    fn resolve_model(&self, provider: &str, model: &str) -> Option<ResolvedModel> {
        (provider == self.model.catalog_provider_id && model == self.model.catalog_model_id)
            .then(|| self.model.clone())
    }
}
struct Budget {
    limits: BudgetDefinition,
    output_tokens: u32,
}
impl Budget {
    fn decide(&self, snapshot: &TurnUsageSnapshot<'_>, before: bool) -> BudgetDecision {
        let allowance = TurnAllowance {
            default_token_budget: None,
            max_tool_calls: Some(self.limits.tool_calls),
            max_duration: Some(Duration::from_secs(self.limits.duration_seconds.get())),
        };
        if let Some(stop) = allowance.ceiling_reached(snapshot) {
            return BudgetDecision::Stop(stop);
        }
        if (!before || snapshot.step > 1) && !snapshot.turn_usage.accounted {
            return BudgetDecision::stop_usage_unknown(
                "The provider did not report enough usage to enforce this budget.",
            );
        }
        let reserved = if before {
            snapshot
                .estimated_prompt_tokens
                .saturating_add(u64::from(self.output_tokens))
        } else {
            0
        };
        if snapshot.turn_usage.total().saturating_add(reserved) > self.limits.tokens.get()
            || snapshot.turn_usage.total() >= self.limits.tokens.get()
        {
            return BudgetDecision::stop_tokens(
                "The configured turn token allowance is exhausted.",
            );
        }
        BudgetDecision::Continue
    }
}
#[async_trait]
impl TurnBudgetPolicy for Budget {
    async fn before_request(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(self.decide(snapshot, true))
    }
    async fn before_tool_continuation(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(self.decide(snapshot, false))
    }
    async fn after_response(
        &self,
        snapshot: &TurnUsageSnapshot<'_>,
    ) -> Result<BudgetDecision, BudgetPolicyError> {
        Ok(self.decide(snapshot, false))
    }
}

struct Installed {
    definition: Definition,
    providers: Arc<ProviderRegistry>,
    resolver: Arc<Resolver>,
}
pub struct ConfiguredWorkerFactory {
    installed: Vec<Installed>,
    state: WorkerClient,
    gateway: Arc<GatewayClient>,
    driver: Arc<dyn AgentDriver>,
    children: crate::children::ConfiguredChildren,
}
impl ConfiguredWorkerFactory {
    pub async fn new(
        definitions: Vec<Definition>,
        credentials: &BTreeMap<String, ModelCredential>,
        state: WorkerClient,
        gateway: Arc<GatewayClient>,
        driver: Arc<dyn AgentDriver>,
    ) -> Result<Self, Error> {
        let children = crate::children::ConfiguredChildren::new(&definitions)?;
        let mut installed = Vec::new();
        for definition in definitions {
            definition.validate()?;
            let credential = definition
                .model
                .credential
                .as_ref()
                .map(|id| {
                    credentials
                        .get(id)
                        .ok_or_else(|| invalid("the model credential binding is not installed"))
                })
                .transpose()?;
            let (providers, spec) = providers(&definition, credential).await?;
            // Construct once during startup to reject unsupported provider configuration
            // before advertising this definition to the scheduler.
            providers
                .resolve(spec.clone())
                .map_err(|_| invalid("the native provider cannot construct this model"))?;
            let surface = spec.surface;
            let resolver = Resolver {
                agent: ResolvedAgent::new(&definition.agent.name, &definition.agent.system_prompt)
                    .with_max_steps(definition.agent.max_steps),
                model: ResolvedModel::new(spec, &definition.model.model_id, surface),
            };
            installed.push(Installed {
                definition,
                providers: Arc::new(providers),
                resolver: Arc::new(resolver),
            });
        }
        if installed.is_empty() || !driver.supports_advance() {
            return Err(invalid(
                "the Worker requires a bounded driver and installed definitions",
            ));
        }
        Ok(Self {
            installed,
            state,
            gateway,
            driver,
            children,
        })
    }
}
#[async_trait]
impl WorkerServiceFactory for ConfiguredWorkerFactory {
    fn configurations(&self) -> Vec<ConfigurationRef> {
        self.installed
            .iter()
            .map(|entry| entry.definition.reference())
            .collect()
    }
    async fn resolve(
        &self,
        execution: &WorkerExecution,
    ) -> Result<WorkerTurnServices, WorkerError> {
        let entry = self
            .installed
            .iter()
            .find(|entry| entry.definition.reference() == execution.job.configuration)
            .ok_or(WorkerError::Configuration)?;
        let selected = entry.definition.selection();
        if execution.input.agent.as_deref() != Some(selected.agent.as_str())
            || execution.input.model.as_ref() != Some(&selected.model)
        {
            return Err(WorkerError::Configuration);
        }
        let memory = Arc::new(self.state.memory(execution));
        let mut dispatcher: Arc<dyn zuno_engine::r#loop::ToolDispatcher> =
            Arc::new(MemoryToolDispatcher::new(
                Arc::new(GatewayToolDispatcher::new(
                    self.state.clone(),
                    execution.clone(),
                    self.gateway.clone(),
                )),
                memory.clone(),
                execution.clone(),
            ));
        if let Some((maximum_depth, targets)) = self.children.targets(&entry.definition.reference())
        {
            dispatcher = Arc::new(
                zuno_worker::child::ChildToolDispatcher::new(
                    dispatcher,
                    self.state.clone(),
                    execution.clone(),
                    targets,
                    maximum_depth,
                )
                .map_err(|_| WorkerError::Configuration)?
                .with_workspace_gateway(self.gateway.clone()),
            );
        }
        Ok(WorkerTurnServices {
            configuration: entry.definition.reference(),
            providers: entry.providers.clone(),
            resolver: entry.resolver.clone(),
            dispatcher,
            driver: self.driver.clone(),
            budget: Arc::new(Budget {
                limits: entry.definition.budget.clone(),
                output_tokens: entry.definition.model.max_output_tokens.get(),
            }),
            dynamic_context: DynamicContext::default(),
            dynamic_context_refresher: Some(Arc::new(MemoryContextRefresher {
                service: memory,
                session_id: execution.job.session_id.to_string(),
                base: DynamicContext::default(),
            })),
            executor_directory: "/workspace".to_owned(),
            steps_per_advance: NonZeroU32::MIN,
            context_limit: Some(entry.definition.model.context_tokens.get()),
        })
    }
}

async fn providers(
    definition: &Definition,
    binding: Option<&ModelCredential>,
) -> Result<(ProviderRegistry, Spec), Error> {
    let model = &definition.model;
    let secret = match binding.and_then(|binding| binding.api_key_file.as_ref()) {
        Some(path) => Some(config::secret(path).await?),
        None => None,
    };
    let key = secret.as_ref().map(|secret| secret.expose().to_owned());
    let credential = secret.map(|key| Credential::Api {
        key,
        metadata: None,
    });
    let factory = match model.transport {
        ProviderTransport::Anthropic => "anthropic",
        ProviderTransport::Bedrock => "amazon-bedrock-converse",
        ProviderTransport::BedrockMantle => "amazon-bedrock",
        ProviderTransport::BedrockRuntime => "amazon-bedrock-runtime",
        ProviderTransport::Google => "google",
        ProviderTransport::GoogleVertex => "google-vertex",
        ProviderTransport::GoogleVertexAnthropic => "google-vertex/anthropic",
        ProviderTransport::Openai => "openai",
        ProviderTransport::OpenaiCompatible | ProviderTransport::Openrouter => "openai-compatible",
    };
    let surface = match model.surface {
        Some(ProviderSurface::Chat) => ApiSurface::Chat,
        Some(ProviderSurface::Responses) => ApiSurface::Responses,
        Some(ProviderSurface::Messages) => ApiSurface::Messages,
        None => ApiSurface::Default,
    };
    let mut spec = Spec::new(&model.provider_id)
        .with_factory(factory)
        .with_surface(surface)
        .with_option(
            zuno_llm::registry::generation::MAX_TOKENS,
            serde_json::json!(model.max_output_tokens.get()),
        );
    spec.base_url = model.base_url.clone();
    spec.region = model.region.clone();
    spec.project = model.project.clone();
    spec.api_version = model.api_version.clone();
    let mut providers = ProviderRegistry::new();
    let root = binding.and_then(|binding| binding.root_certificate.as_ref());
    if root.is_some() && factory != "openai-compatible" {
        return Err(invalid(
            "custom model CA files require the compatible transport; native cloud transports use platform trust",
        ));
    }
    match model.transport {
        ProviderTransport::OpenaiCompatible | ProviderTransport::Openrouter => {
            spec.options.insert(
                zuno_provider_compatible::family::TRANSPORT_OPTION.to_owned(),
                serde_json::json!(zuno_provider_compatible::family::OPENAI_COMPATIBLE_TRANSPORT),
            );
            let mut client = zuno_network::client_builder()
                .https_only(true)
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none());
            if let Some(path) = root {
                client = client.add_root_certificate(
                    reqwest::Certificate::from_pem(&config::read_file(path, 65536).await?)
                        .map_err(|_| invalid("invalid model CA certificate"))?,
                );
            }
            let client = client
                .build()
                .map_err(|_| invalid("model HTTP client could not be constructed"))?;
            let transport = Arc::new(
                zuno_provider_compatible::transport::ReqwestTransport::with_client(
                    &model.provider_id,
                    client,
                ),
            );
            providers.register_fallible(
                factory,
                zuno_provider_compatible::factory(transport, move |_| key.clone()),
            );
        }
        ProviderTransport::Anthropic => providers.register_fallible(
            factory,
            zuno_provider_anthropic::factory(move |_| credential.clone()),
        ),
        ProviderTransport::Openai => providers.register_fallible(
            factory,
            zuno_provider_openai::factory(move |_| credential.clone(), None),
        ),
        ProviderTransport::Google => providers.register_fallible(
            factory,
            zuno_provider_google::google_factory(move |_| key.clone()),
        ),
        ProviderTransport::GoogleVertex => providers.register_fallible(
            factory,
            zuno_provider_google::vertex_gemini_factory(move |_| key.clone()),
        ),
        ProviderTransport::GoogleVertexAnthropic => providers.register_fallible(
            factory,
            zuno_provider_google::vertex_anthropic_factory(move |_| key.clone()),
        ),
        family => {
            let bearer = key.map(zuno_provider_bedrock::BedrockBearerToken::new);
            providers.register_fallible(factory, move |spec| match family {
                ProviderTransport::Bedrock => zuno_provider_bedrock::factory_with_bearer(
                    spec,
                    bearer.clone(),
                )
                .map_err(|error| {
                    zuno_llm::registry::Declined::Failed(zuno_error::ProviderError::fatal(error))
                }),
                ProviderTransport::BedrockMantle => {
                    zuno_provider_bedrock::mantle_factory_with_bearer(spec, bearer.clone()).map_err(
                        |error| {
                            zuno_llm::registry::Declined::Failed(zuno_error::ProviderError::fatal(
                                error,
                            ))
                        },
                    )
                }
                ProviderTransport::BedrockRuntime => {
                    zuno_provider_bedrock::runtime_factory_with_bearer(spec, bearer.clone())
                        .map_err(|error| {
                            zuno_llm::registry::Declined::Failed(zuno_error::ProviderError::fatal(
                                error,
                            ))
                        })
                }
                _ => unreachable!("Bedrock branch selected above"),
            });
        }
    }
    Ok((providers, spec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use zuno_engine::budget::{BudgetStopKind, ProviderRequestUsage};

    fn budget() -> Budget {
        Budget {
            limits: BudgetDefinition {
                tokens: NonZeroU64::new(1000).unwrap(),
                tool_calls: NonZeroU32::new(4).unwrap(),
                duration_seconds: NonZeroU64::new(30).unwrap(),
            },
            output_tokens: 100,
        }
    }
    fn snapshot() -> TurnUsageSnapshot<'static> {
        TurnUsageSnapshot {
            session_id: "session",
            turn_id: "turn",
            step: 2,
            turn_usage: ProviderRequestUsage {
                input_tokens: 400,
                output_tokens: 100,
                accounted: true,
                ..Default::default()
            },
            last_request: ProviderRequestUsage::default(),
            estimated_prompt_tokens: 200,
            context_limit: Some(8192),
            elapsed_seconds: 1,
            tool_calls_dispatched: 1,
        }
    }
    #[test]
    fn resumed_usage_and_wait_time_keep_the_original_limits() {
        let policy = budget();
        let mut state = snapshot();
        assert!(matches!(
            policy.decide(&state, true),
            BudgetDecision::Continue
        ));
        state.turn_usage.input_tokens = 900;
        assert!(
            matches!(policy.decide(&state,true),BudgetDecision::Stop(stop) if stop.kind==BudgetStopKind::TokenBudget)
        );
        state = snapshot();
        state.elapsed_seconds = 30;
        assert!(
            matches!(policy.decide(&state,false),BudgetDecision::Stop(stop) if stop.kind==BudgetStopKind::TimeBudget)
        );
        state = snapshot();
        state.turn_usage.accounted = false;
        assert!(
            matches!(policy.decide(&state,false),BudgetDecision::Stop(stop) if stop.kind==BudgetStopKind::UsageUnknown)
        );
    }
}
