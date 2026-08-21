//! What both surfaces must be able to trust about the shared composition root.

use super::*;
use zuno_engine::r#loop::run_turn;

use crate::cmd::tool_runtime;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use zuno_catalog::agent::{Agent, AgentMode, AgentSource};
use zuno_llm::sse::StreamIdleTimeout;
use zuno_paths::Env;

fn agent(name: &str) -> Agent {
    Agent {
        name: name.to_owned(),
        description: None,
        mode: AgentMode::All,
        hidden: None,
        model: None,
        variant: None,
        temperature: None,
        top_p: None,
        color: None,
        prompt: None,
        steps: None,
        options: serde_json::Map::new(),
        permission: None,
        source: AgentSource::Native,
    }
}

/// The delegation collaborators a test supplies to reach [`tool_runtime::assemble`].
///
/// A recording host and no catalog facts, because none of these assertions drive a
/// child turn. That this compiles at all is the point the production wiring rests on:
/// `Delegation` is a required field, so `turn.rs` cannot assemble a turn's tools
/// without handing over a real `ChildTurnHost`.
fn test_delegation() -> tool_runtime::Delegation {
    tool_runtime::Delegation {
        host: Arc::new(zuno_tools::task::RecordingHost::new()),
        facts: Arc::new(zuno_tools::task::NoProviders),
        session_model: zuno_agent::model_policy::ModelChoice::new("provider/model"),
        limits: zuno_tools::task::DelegationLimits::default(),
        vision_available: false,
    }
}

fn plan(directory: &str, session: SessionChoice) -> TurnPlan {
    let directory = PathBuf::from(directory);
    let project = zuno_paths::project::ResolvedProject {
        previous: None,
        id: "project-turn-test".to_owned(),
        directory: directory.clone(),
        vcs: None,
    };
    let agent = agent("build");
    let runtime = zuno_runtime::HarnessRuntime::new("test-profile");
    futures::executor::block_on(
        runtime.mount(zuno_engine::driver::AgentDriverComponent::new(Arc::new(
            zuno_engine::driver::DefaultAgentDriver,
        ))),
    )
    .expect("default test driver mounts");
    TurnPlan {
        runtime,
        resolver: Resolver {
            requested_agent: agent.name.clone(),
            system_prompt: String::new(),
            max_steps: DEFAULT_MAX_STEPS,
            requested_provider: "provider".to_owned(),
            requested_model: "model".to_owned(),
            wire_model: "model".to_owned(),
            spec: Spec::new(COMPATIBLE_PROVIDER).with_surface(ApiSurface::Chat),
            reasoning_options: serde_json::Map::new(),
        },
        catalog_models: Vec::new(),
        skills: Arc::new(zuno_catalog::skill::Skills::default()),
        instructions: zuno_config::LoadedInstructions::default(),
        delegation_facts: Arc::new(zuno_tools::task::FixedFacts::new()),
        vision_available: false,
        reasoning_supported: false,
        effort: None,
        directory,
        project,
        config: zuno_config::schema::Config::default(),
        agent,
        provider_id: "provider".to_owned(),
        model_id: "model".to_owned(),
        credential: None,
        session,
        title: None,
        internals: stub_internals(),
        window: TokenWindow {
            context: 0,
            max_output: 0,
        },
        notes: Vec::new(),
    }
}

fn stub_internals() -> Internals {
    let agent = |name: &str| InternalAgent {
        name: name.to_owned(),
        prompt: String::new(),
        model: EngineModel::new(Spec::new(COMPATIBLE_PROVIDER), "model", ApiSurface::Chat),
    };
    Internals {
        title: agent("title"),
        compaction: agent("compaction"),
        summary: agent("summary"),
    }
}

#[test]
fn resolved_prompt_blocks_become_the_text_and_file_parts_the_engine_projects() {
    let input = UserMessageInput {
        session_id: "ses_reference",
        agent: "build",
        provider_id: "provider",
        model_id: "model",
        text: "inspect @note.txt @diagram.png",
        message_id: None,
        now: 1_780_000_000_000,
    };
    let content = vec![
        RequestContentBlock::Text {
            text: "inspect @note.txt @diagram.png".to_owned(),
        },
        RequestContentBlock::Text {
            text: "--- BEGIN REFERENCED FILE: note.txt ---\nreal body\n--- END REFERENCED FILE: note.txt ---".to_owned(),
        },
        RequestContentBlock::Image {
            media_type: "image/png".to_owned(),
            data: "aW1hZ2U=".to_owned(),
        },
    ];

    let parts = request_content_parts(&input, "msg_reference", &content)
        .expect("text and image request blocks are valid user content");

    assert_eq!(
        parts
            .iter()
            .filter(|part| part.kind == zuno_db::message::PartKind::Text)
            .count(),
        2
    );
    let image = parts
        .iter()
        .find(|part| part.kind == zuno_db::message::PartKind::File)
        .expect("the image became a stored file part");
    assert_eq!(image.data["mime"], "image/png");
    assert_eq!(image.data["data"], "aW1hZ2U=");
}

#[test]
fn production_prompt_composition_honours_the_memory_master_switch() {
    let directory = tempfile::TempDir::new().expect("temporary memory paths");
    let paths = zuno_tools::ScopePaths::at(
        directory.path().join("global/MEMORY.md"),
        directory.path().join("project/RULES.md"),
    );
    let mut seeded = zuno_memory::MemoryStore::open(
        zuno_memory::Scope::Project,
        paths.for_scope(zuno_memory::Scope::Project).to_path_buf(),
    )
    .expect("seeded project memory");
    seeded
        .apply_batch(&[zuno_memory::Operation::add(
            "production composition sentinel",
        )])
        .expect("seed memory");
    let base = "SYSTEM\r\n${UNCHANGED}\n终";
    let resolver = || Resolver {
        requested_agent: "build".to_owned(),
        system_prompt: base.to_owned(),
        max_steps: DEFAULT_MAX_STEPS,
        requested_provider: "provider".to_owned(),
        requested_model: "model".to_owned(),
        wire_model: "model".to_owned(),
        spec: Spec::new(COMPATIBLE_PROVIDER),
        reasoning_options: serde_json::Map::new(),
    };

    let mut disabled = resolver();
    let config = serde_json::from_str(r#"{"memory":false}"#).expect("disabled config");
    configure_resident_memory(&mut disabled, &config, paths.clone()).expect("disabled path");
    assert_eq!(disabled.system_prompt.as_bytes(), base.as_bytes());

    let mut enabled = resolver();
    configure_resident_memory(&mut enabled, &zuno_config::schema::Config::default(), paths)
        .expect("enabled path");
    assert!(
        enabled
            .system_prompt
            .contains("production composition sentinel")
    );
    assert_ne!(enabled.system_prompt.as_bytes(), base.as_bytes());
}

/// Two models under one provider, with `title` overridden to the smaller one.
///
/// The provider carries an endpoint in `options.baseURL`. It has to: a provider with no
/// endpoint in either place is one no turn could ever run against, and building specs
/// from it was how a spec with no base URL used to pass unnoticed. This is the same
/// lesson as the top-level `api` key the seam tests no longer send — a fixture must not
/// be servable in ways the real input shape is not, and must not be unservable in ways
/// it would not be either.
fn catalog_with_two_models_and_a_title_override() -> (Catalog, zuno_config::schema::Config) {
    let document = serde_json::from_str(
        r#"{"test":{"id":"test","name":"Test","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{
               "big":{"id":"big","name":"Big","limit":{"context":200000,"output":8192}},
               "small":{"id":"small","name":"Small","limit":{"context":100000,"output":4096}}}}}"#,
    )
    .expect("catalog document");
    let config = serde_json::from_str(
        r#"{"provider":{"test":{"options":{"baseURL":"https://gateway.test/v1"}}},
             "agent":{"title":{"model":"test/small"}}}"#,
    )
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    (catalog, config)
}

#[test]
fn model_selection_splits_only_the_provider_prefix() {
    let document = serde_json::from_str(
        r#"{"anyapi":{"id":"anyapi","name":"AnyAPI","env":[],"models":{"openai/gpt":{"id":"openai/gpt","name":"GPT","limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config = serde_json::from_str(r#"{"provider":{"anyapi":{}}}"#).expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let (provider, model, _) = select_model(
        &catalog,
        Some("anyapi/openai/gpt"),
        &CatalogProvenance::Fetched,
    )
    .expect("nested model id");
    assert_eq!(provider, "anyapi");
    assert_eq!(model, "openai/gpt");
}

#[test]
fn every_declared_wire_transport_selects_its_production_registry_key() {
    let cases = [
        ("anthropic", "@ai-sdk/anthropic", "anthropic"),
        ("amazon-bedrock", "@ai-sdk/amazon-bedrock", "amazon-bedrock"),
        (
            "amazon-bedrock/mantle",
            "@ai-sdk/amazon-bedrock/mantle",
            "amazon-bedrock/mantle",
        ),
        ("google", "@ai-sdk/google", "google"),
        ("google-vertex", "@ai-sdk/google-vertex", "google-vertex"),
        (
            "google-vertex/anthropic",
            "@ai-sdk/google-vertex/anthropic",
            "google-vertex/anthropic",
        ),
        ("openai", "@ai-sdk/openai", "openai"),
        (
            "private-gateway",
            "@ai-sdk/openai-compatible",
            COMPATIBLE_PROVIDER,
        ),
    ];

    for (provider_id, npm, expected) in cases {
        assert_eq!(
            provider_factory_key(provider_id, npm),
            Some(expected),
            "resolved npm metadata `{npm}` selected the wrong production factory"
        );
    }
    assert_eq!(
        provider_factory_key("unknown", "@ai-sdk/not-implemented"),
        None
    );
}

fn named_compatible_cases() -> [(&'static str, &'static str); 15] {
    [
        ("openrouter", "@openrouter/ai-sdk-provider"),
        ("xai", "@ai-sdk/xai"),
        ("mistral", "@ai-sdk/mistral"),
        ("groq", "@ai-sdk/groq"),
        ("deepinfra", "@ai-sdk/deepinfra"),
        ("cerebras", "@ai-sdk/cerebras"),
        ("cohere", "@ai-sdk/cohere"),
        ("togetherai", "@ai-sdk/togetherai"),
        ("perplexity", "@ai-sdk/perplexity"),
        ("vercel", "@ai-sdk/vercel"),
        ("alibaba", "@ai-sdk/alibaba"),
        ("gitlab", "gitlab-ai-provider"),
        ("venice", "venice-ai-sdk-provider"),
        ("azure", "@ai-sdk/azure"),
        ("github-copilot", "@ai-sdk/github-copilot"),
    ]
}

fn production_wire_spec_result(
    provider_id: &str,
    npm: &str,
    model_id: &str,
    endpoint: &str,
    extra_options: serde_json::Value,
) -> Result<Spec, String> {
    let mut options = extra_options
        .as_object()
        .cloned()
        .expect("provider options are an object");
    options.insert("baseURL".to_owned(), serde_json::json!(endpoint));
    let mut models = serde_json::Map::new();
    models.insert(
        model_id.to_owned(),
        serde_json::json!({
            "id": model_id,
            "name": "Production wire replay",
            "limit": {"context": 100000, "output": 8192}
        }),
    );
    let mut providers = serde_json::Map::new();
    providers.insert(
        provider_id.to_owned(),
        serde_json::json!({
            "id": provider_id,
            "name": "Production wire replay",
            "env": [],
            "npm": npm,
            "options": serde_json::Value::Object(options),
            "models": serde_json::Value::Object(models)
        }),
    );
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": serde_json::Value::Object(providers)
    }))
    .expect("production replay config");
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    let model = catalog
        .model(provider_id, model_id)
        .expect("production replay model resolves");
    model_spec(&catalog, model, &Env::empty())
}

fn production_wire_spec(
    provider_id: &str,
    npm: &str,
    model_id: &str,
    endpoint: &str,
    extra_options: serde_json::Value,
) -> Spec {
    production_wire_spec_result(provider_id, npm, model_id, endpoint, extra_options)
        .expect("production replay spec resolves")
}

fn openai_wire_spec(
    provider_id: &str,
    model_id: &str,
    endpoint: &str,
    custom_base_url: bool,
    advertised_endpoint: Option<&str>,
) -> Spec {
    let provider_location = if custom_base_url {
        serde_json::json!({"options": {"baseURL": endpoint}})
    } else {
        serde_json::json!({"api": endpoint})
    };
    let mut provider = provider_location
        .as_object()
        .cloned()
        .expect("provider location is an object");
    provider.insert("id".to_owned(), serde_json::json!(provider_id));
    provider.insert("name".to_owned(), serde_json::json!("OpenAI wire replay"));
    provider.insert("env".to_owned(), serde_json::json!([]));
    provider.insert("npm".to_owned(), serde_json::json!("@ai-sdk/openai"));
    provider.insert(
        "models".to_owned(),
        serde_json::json!({
            model_id: {
                "id": model_id,
                "name": "OpenAI wire replay",
                "limit": {"context": 100000, "output": 8192}
            }
        }),
    );
    let providers =
        serde_json::Map::from_iter([(provider_id.to_owned(), serde_json::Value::Object(provider))]);
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": serde_json::Value::Object(providers)
    }))
    .expect("OpenAI replay config");
    let mut catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );
    if let Some(advertised_endpoint) = advertised_endpoint {
        let mut value = serde_json::to_value(
            catalog
                .model(provider_id, model_id)
                .expect("OpenAI replay model resolves"),
        )
        .expect("OpenAI replay model serializes");
        value["api"]["endpoint"] = serde_json::json!(advertised_endpoint);
        let resolved = serde_json::from_value(value).expect("advertised endpoint resolves");
        assert!(catalog.replace_provider_models(
            provider_id,
            std::collections::BTreeMap::from([(model_id.to_owned(), resolved)]),
        ));
    }
    let model = catalog
        .model(provider_id, model_id)
        .expect("OpenAI replay model resolves");
    model_spec(&catalog, model, &Env::empty()).expect("OpenAI replay spec resolves")
}

fn plugin_resolved_wire_spec(
    catalog_model_id: &str,
    api_model_id: &str,
    advertised_endpoint: &str,
    endpoint: &str,
) -> Spec {
    let document: zuno_llm::catalog::models_dev::CatalogDocument =
        serde_json::from_value(serde_json::json!({
            "github-copilot": {
                "id": "github-copilot",
                "name": "GitHub Copilot",
                "env": [],
                "npm": "@ai-sdk/github-copilot",
                "models": {
                    catalog_model_id: {
                        "id": api_model_id,
                        "name": "Advertised endpoint replay",
                        "limit": {"context": 100000, "output": 8192}
                    }
                }
            }
        }))
        .expect("pinned Copilot catalog metadata");
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "github-copilot": {"options": {"baseURL": endpoint}}
        }
    }))
    .expect("Copilot endpoint override");
    let mut catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));

    // Provider model hooks return the SDK's resolved Model shape. Exercise the
    // same serde boundary as `HandleModelLoader`, including a catalog id that is
    // different from the wire id so the declaration must follow `api.id`.
    let mut plugin_value = serde_json::to_value(
        catalog
            .model("github-copilot", catalog_model_id)
            .expect("base Copilot model resolves"),
    )
    .expect("resolved model serializes");
    plugin_value["api"]["endpoint"] = serde_json::json!(advertised_endpoint);
    let plugin_model: zuno_llm::catalog::ResolvedModel =
        serde_json::from_value(plugin_value).expect("plugin model metadata resolves");
    assert!(
        catalog.replace_provider_models(
            "github-copilot",
            std::collections::BTreeMap::from([(catalog_model_id.to_owned(), plugin_model)]),
        ),
        "the plugin model replaces its pinned catalog provider"
    );

    let model = catalog
        .model("github-copilot", catalog_model_id)
        .expect("plugin-provided Copilot model resolves");
    assert_eq!(model.api.id, api_model_id);
    model_spec(&catalog, model, &Env::empty()).expect("plugin-provided Copilot spec resolves")
}

fn pinned_wire_spec(provider_id: &str, model_id: &str, endpoint: &str, expected_npm: &str) -> Spec {
    let document: zuno_llm::catalog::models_dev::CatalogDocument = serde_json::from_str(
        include_str!("../../../zuno-llm/tests/fixtures/models-dev-pinned.json"),
    )
    .expect("pinned catalog fixture");
    let mut providers = serde_json::Map::new();
    providers.insert(
        provider_id.to_owned(),
        serde_json::json!({"options": {"baseURL": endpoint}}),
    );
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": serde_json::Value::Object(providers)
    }))
    .expect("pinned provider endpoint override");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let model = catalog
        .model(provider_id, model_id)
        .expect("pinned provider model resolves");
    assert_eq!(model.api.npm, expected_npm);
    model_spec(&catalog, model, &Env::empty()).expect("pinned provider spec resolves")
}

struct ReplayCase<'a> {
    provider_id: &'a str,
    registry_key: &'a str,
    model_id: &'a str,
    cassette: &'a str,
    endpoint_suffix: &'a str,
    expected_body_key: &'a str,
    expected_text: &'a str,
}

async fn replay_selected_production_spec<F>(case: ReplayCase<'_>, build_spec: F)
where
    F: FnOnce(&str) -> Spec,
{
    let ReplayCase {
        provider_id,
        registry_key,
        model_id,
        cassette,
        endpoint_suffix,
        expected_body_key,
        expected_text,
    } = case;
    if zuno_testkit::recordings_root_or_skip(
        &format!("replay_selected_production_spec[{provider_id}/{model_id}]"),
        "the selected production provider spec was NOT replayed",
    )
    .is_none()
    {
        return;
    }
    let scenario = zuno_testkit::Scenario::new(provider_id)
        .on_path(endpoint_suffix)
        .from_oracle_cassette(cassette)
        .expect("recorded provider response loads");
    let mock = zuno_testkit::MockProvider::start(vec![scenario])
        .await
        .expect("loopback provider starts");
    assert!(mock.authored_scenarios().is_empty());

    let endpoint = if registry_key == COMPATIBLE_PROVIDER {
        format!("{}/v1", mock.base_url())
    } else {
        mock.base_url().to_owned()
    };
    let spec = build_spec(&endpoint);
    assert_eq!(
        spec.provider, provider_id,
        "production selection collapsed `{provider_id}` into its factory"
    );
    assert_eq!(spec.factory(), registry_key);
    let credential = Credential::Api {
        key: zuno_auth::Secret::new("production-replay-credential"),
        metadata: None,
    };
    let providers = provider_registry(provider_id, Some(credential));
    assert!(
        providers.is_registered(registry_key),
        "production registry omitted `{registry_key}`"
    );

    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let replay_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &replay_plan.project, now).expect("persist project");
    let session =
        resolve_session(&mut connection, &replay_plan, now).expect("create replay session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id,
            model_id,
            text: "Reply with a short greeting.",
            message_id: None,
            now,
        },
    )
    .expect("persist replay prompt");

    let internal = InternalAgent {
        name: "summary".to_owned(),
        prompt: "Summarize the conversation.".to_owned(),
        model: EngineModel::new(spec.clone(), model_id, spec.surface),
    };
    let internals = Internals {
        title: internal.clone(),
        compaction: internal.clone(),
        summary: internal,
    };
    let registry = RegistryProviders(&providers);
    let compaction = zuno_config::schema::CompactionConfig::default();
    let mut state = CompactionState::default();
    let mut context = PreludeContext {
        connection: &mut connection,
        providers: &registry,
        internals: &internals,
        compaction: &compaction,
        window: TokenWindow {
            context: 100_000,
            max_output: 8_192,
        },
        state: &mut state,
        hooks: &zuno_engine::compaction::NoopCompactionHooks,
    };
    let text = zuno_engine::prelude::summarize(&session.id, &mut context)
        .await
        .expect("production provider stream decodes");
    assert_eq!(text, expected_text);

    let captured = mock.captured().await;
    assert_eq!(captured.len(), 1, "one production stream request expected");
    assert!(
        captured[0].path.ends_with(endpoint_suffix),
        "`{registry_key}` dispatched to {}, expected suffix {endpoint_suffix}",
        captured[0].path
    );
    let body = captured[0].json().expect("production request body is JSON");
    let forbidden_body_key = if expected_body_key == "input" {
        "messages"
    } else {
        "input"
    };
    assert!(
        body.get(expected_body_key).is_some(),
        "`{registry_key}` request body omitted `{expected_body_key}`: {body}"
    );
    assert!(
        body.get(forbidden_body_key).is_none(),
        "`{registry_key}` request body retained `{forbidden_body_key}`: {body}"
    );
    assert!(
        captured[0]
            .served_origin
            .as_ref()
            .is_some_and(zuno_testkit::ResponseOrigin::is_recorded),
        "`{registry_key}` did not decode recorded provider bytes"
    );
    mock.shutdown().await;
}

struct RegistrationCase<'a> {
    registry_key: &'a str,
    npm: &'a str,
    model_id: &'a str,
    cassette: &'a str,
    extra_options: serde_json::Value,
    endpoint_suffix: &'a str,
    expected_body_key: &'a str,
    expected_text: &'a str,
}

async fn replay_production_registration(case: RegistrationCase<'_>) {
    let RegistrationCase {
        registry_key,
        npm,
        model_id,
        cassette,
        extra_options,
        endpoint_suffix,
        expected_body_key,
        expected_text,
    } = case;
    let provider_id = if registry_key == COMPATIBLE_PROVIDER {
        "wire-test"
    } else {
        registry_key
    };
    replay_selected_production_spec(
        ReplayCase {
            provider_id,
            registry_key,
            model_id,
            cassette,
            endpoint_suffix,
            expected_body_key,
            expected_text,
        },
        |endpoint| production_wire_spec(provider_id, npm, model_id, endpoint, extra_options),
    )
    .await;
}

#[tokio::test]
async fn production_compatible_registration_dispatches_and_decodes_recorded_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: COMPATIBLE_PROVIDER,
        npm: "@ai-sdk/openai-compatible",
        model_id: "deepseek-chat",
        cassette: "openai-compatible-chat/deepseek-streams-text",
        extra_options: serde_json::json!({}),
        endpoint_suffix: "/v1/chat/completions",
        expected_body_key: "messages",
        expected_text: "Hello!",
    })
    .await;
}

#[test]
fn every_todo_94_identity_reaches_its_profile_from_resolved_config() {
    for (provider_id, npm) in named_compatible_cases() {
        let spec = production_wire_spec(
            provider_id,
            npm,
            "selection-probe",
            "https://selection.test/v1",
            serde_json::json!({}),
        );
        assert_eq!(
            spec.provider, provider_id,
            "identity collapsed for {provider_id}"
        );
        assert_eq!(spec.factory(), COMPATIBLE_PROVIDER, "{provider_id}");
        let profile = zuno_provider_compatible::family::resolve(&spec)
            .unwrap_or_else(|error| panic!("{provider_id} was not reachable: {error}"));
        assert_eq!(
            profile.provider, provider_id,
            "wrong profile for {provider_id}"
        );
        assert_eq!(
            profile.routes_upstreams,
            matches!(provider_id, "openrouter" | "vercel"),
            "router behavior did not survive selection for {provider_id}"
        );
    }
}

#[test]
fn an_unknown_transport_is_refused_from_resolved_config() {
    let error = production_wire_spec_result(
        "unknown-provider",
        "@ai-sdk/not-implemented",
        "unknown-model",
        "https://unknown.test/v1",
        serde_json::json!({}),
    )
    .expect_err("unknown transports must not fall through to the compatible factory");
    assert!(error.contains("@ai-sdk/not-implemented"), "{error}");
}

#[tokio::test]
async fn production_openrouter_keeps_router_identity_and_dispatches_recorded_sse() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openrouter",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "openai/gpt-4o-mini",
            cassette: "openai-compatible-chat/openrouter-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            production_wire_spec(
                "openrouter",
                "@openrouter/ai-sdk-provider",
                "openai/gpt-4o-mini",
                endpoint,
                serde_json::json!({}),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_azure_selector_dispatches_to_responses() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "azure",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "deployment-a",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| {
            production_wire_spec(
                "azure",
                "@ai-sdk/azure",
                "deployment-a",
                endpoint,
                serde_json::json!({}),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_copilot_rule_dispatches_by_model_id() {
    for (model_id, cassette, endpoint_suffix, expected_body_key) in [
        (
            "gpt-5",
            "openai-responses/gpt-5-5-streams-text",
            "/v1/responses",
            "input",
        ),
        (
            "gpt-5-mini",
            "openai-compatible-chat/deepseek-streams-text",
            "/v1/chat/completions",
            "messages",
        ),
    ] {
        replay_selected_production_spec(
            ReplayCase {
                provider_id: "github-copilot",
                registry_key: COMPATIBLE_PROVIDER,
                model_id,
                cassette,
                endpoint_suffix,
                expected_body_key,
                expected_text: "Hello!",
            },
            |endpoint| {
                production_wire_spec(
                    "github-copilot",
                    "@ai-sdk/github-copilot",
                    model_id,
                    endpoint,
                    serde_json::json!({}),
                )
            },
        )
        .await;
    }
}

#[tokio::test]
async fn production_copilot_advertised_responses_beats_a_heuristic_hostile_id() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "github-copilot",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "mai-code-1-flash-picker",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| {
            plugin_resolved_wire_spec(
                "mai-code-alias",
                "mai-code-1-flash-picker",
                "responses",
                endpoint,
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_copilot_advertised_chat_beats_a_responses_heuristic_id() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "github-copilot",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "gpt-5",
            cassette: "openai-compatible-chat/deepseek-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| plugin_resolved_wire_spec("gpt-5-alias", "gpt-5", "chat", endpoint),
    )
    .await;
}

#[tokio::test]
async fn pinned_groq_transport_selects_compatible_factory_and_dispatches() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "groq",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "allam-2-7b",
            cassette: "openai-compatible-chat/groq-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| pinned_wire_spec("groq", "allam-2-7b", endpoint, "@ai-sdk/groq"),
    )
    .await;
}

#[tokio::test]
async fn pinned_mistral_transport_selects_compatible_factory_and_dispatches() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "mistral",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "codestral-latest",
            cassette: "openai-compatible-chat/togetherai-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| pinned_wire_spec("mistral", "codestral-latest", endpoint, "@ai-sdk/mistral"),
    )
    .await;
}

#[tokio::test]
async fn production_anthropic_registration_dispatches_and_decodes_recorded_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "anthropic",
        npm: "@ai-sdk/anthropic",
        model_id: "claude-haiku-4-5-20251001",
        cassette: "anthropic-messages/streams-text",
        extra_options: serde_json::json!({"maxTokens": 20, "promptCache": false}),
        endpoint_suffix: "/v1/messages",
        expected_body_key: "messages",
        expected_text: "Hello!",
    })
    .await;
}

#[tokio::test]
async fn production_openai_registration_dispatches_and_decodes_recorded_responses_sse() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai",
            registry_key: "openai",
            model_id: "gpt-5.5",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| openai_wire_spec("openai", "gpt-5.5", endpoint, false, None),
    )
    .await;
}

#[tokio::test]
async fn production_custom_openai_base_url_without_advertised_endpoint_dispatches_to_chat() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "private-openai-gateway",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "gateway-model",
            cassette: "openai-compatible-chat/deepseek-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            openai_wire_spec(
                "private-openai-gateway",
                "gateway-model",
                endpoint,
                true,
                None,
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_custom_openai_base_url_honors_advertised_responses() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai-wire",
            registry_key: COMPATIBLE_PROVIDER,
            model_id: "gateway-model",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| {
            openai_wire_spec(
                "openai-wire",
                "gateway-model",
                endpoint,
                true,
                Some("responses"),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_catalog_native_openai_without_override_keeps_responses() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai-wire",
            registry_key: "openai",
            model_id: "gpt-native",
            cassette: "openai-responses/gpt-5-5-streams-text",
            endpoint_suffix: "/v1/responses",
            expected_body_key: "input",
            expected_text: "Hello!",
        },
        |endpoint| openai_wire_spec("openai-wire", "gpt-native", endpoint, false, None),
    )
    .await;
}

#[tokio::test]
async fn production_catalog_native_openai_honors_advertised_chat() {
    replay_selected_production_spec(
        ReplayCase {
            provider_id: "openai-wire",
            registry_key: "openai",
            model_id: "gpt-native-chat",
            cassette: "openai-compatible-chat/deepseek-streams-text",
            endpoint_suffix: "/v1/chat/completions",
            expected_body_key: "messages",
            expected_text: "Hello!",
        },
        |endpoint| {
            openai_wire_spec(
                "openai-wire",
                "gpt-native-chat",
                endpoint,
                false,
                Some("chat"),
            )
        },
    )
    .await;
}

#[tokio::test]
async fn production_bedrock_registration_dispatches_and_decodes_recorded_eventstream() {
    replay_production_registration(RegistrationCase {
        registry_key: "amazon-bedrock",
        npm: "@ai-sdk/amazon-bedrock",
        model_id: "us.amazon.nova-micro-v1:0",
        cassette: "bedrock-converse/streams-text",
        extra_options: serde_json::json!({
            "region": "us-east-1",
            "accessKeyId": "AKIAREPLAY",
            "secretAccessKey": "replay-secret"
        }),
        endpoint_suffix: "/model/us.amazon.nova-micro-v1%3A0/converse-stream",
        expected_body_key: "messages",
        expected_text: "Hello",
    })
    .await;
}

#[tokio::test]
async fn production_bedrock_mantle_registration_dispatches_and_decodes_recorded_eventstream() {
    replay_production_registration(RegistrationCase {
        registry_key: "amazon-bedrock/mantle",
        npm: "@ai-sdk/amazon-bedrock/mantle",
        model_id: "openai.gpt-oss-120b",
        cassette: "bedrock-converse/streams-text",
        extra_options: serde_json::json!({
            "region": "us-east-1",
            "accessKeyId": "AKIAREPLAY",
            "secretAccessKey": "replay-secret"
        }),
        endpoint_suffix: "/model/openai.gpt-oss-120b/converse-stream",
        expected_body_key: "messages",
        expected_text: "Hello",
    })
    .await;
}

#[tokio::test]
async fn production_google_registration_dispatches_and_decodes_recorded_gemini_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "google",
        npm: "@ai-sdk/google",
        model_id: "gemini-2.5-flash",
        cassette: "gemini/streams-text",
        extra_options: serde_json::json!({}),
        endpoint_suffix: "/models/gemini-2.5-flash:streamGenerateContent",
        expected_body_key: "contents",
        expected_text: "Hello!",
    })
    .await;
}

#[tokio::test]
async fn production_vertex_gemini_registration_dispatches_and_decodes_recorded_gemini_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "google-vertex",
        npm: "@ai-sdk/google-vertex",
        model_id: "gemini-2.5-flash",
        cassette: "gemini/streams-text",
        extra_options: serde_json::json!({"project": "project-a", "location": "us-central1"}),
        endpoint_suffix: "/models/gemini-2.5-flash:streamGenerateContent",
        expected_body_key: "contents",
        expected_text: "Hello!",
    })
    .await;
}

#[tokio::test]
async fn production_vertex_anthropic_registration_dispatches_and_decodes_recorded_anthropic_sse() {
    replay_production_registration(RegistrationCase {
        registry_key: "google-vertex/anthropic",
        npm: "@ai-sdk/google-vertex/anthropic",
        model_id: "claude-haiku-4-5-20251001",
        cassette: "anthropic-messages/streams-text",
        extra_options: serde_json::json!({
            "project": "project-a",
            "location": "us",
            "maxTokens": 20
        }),
        endpoint_suffix: "/claude-haiku-4-5-20251001:streamRawPredict",
        expected_body_key: "messages",
        expected_text: "Hello!",
    })
    .await;
}

/// The catalog a forbidden fetch leaves behind, as [`CatalogSource::load`] builds it.
fn forbidden_fetch() -> CatalogProvenance {
    CatalogProvenance::FetchForbidden {
        origin: "https://models.dev".to_owned(),
        cache: PathBuf::from("/nowhere/cache/zuno/models.json"),
    }
}

/// A config that specifies a provider and a model end to end, as an air-gapped user
/// pointing at a private gateway writes it.
fn self_specified_config() -> zuno_config::schema::Config {
    serde_json::from_str(
        r#"{"provider":{"private":{"name":"Private","id":"private","env":[],
             "npm":"@ai-sdk/openai-compatible","api":"https://gateway.internal/v1",
             "models":{"house-model":{"id":"house-model","name":"House Model",
               "tool_call":true,"limit":{"context":100000,"output":10000},
               "cost":{"input":0,"output":0}}},
             "options":{"apiKey":"k","baseURL":"https://gateway.internal/v1"}}}}"#,
    )
    .expect("config")
}

/// Todo 108's happy path, at the seam that refused it: an empty catalog plus a config
/// that leaves nothing to look up must still select the model.
#[test]
fn a_config_specified_model_selects_with_no_catalog_at_all() {
    let config = self_specified_config();
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );

    let (provider, model, resolved) =
        select_model(&catalog, Some("private/house-model"), &forbidden_fetch())
            .expect("a config that fully specifies the model needs no catalog");

    assert_eq!(provider, "private");
    assert_eq!(model, "house-model");
    assert_eq!(resolved.api.url, "https://gateway.internal/v1");
    assert!(
        provider_factory_key(&resolved.provider_id, &resolved.api.npm).is_some(),
        "the config's transport must survive resolution or the turn is refused later"
    );
}

/// The other half: a model nobody defines still fails immediately, and names the fix.
#[test]
fn a_model_no_config_defines_fails_immediately_and_names_the_fix() {
    let config = self_specified_config();
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );

    let message = select_model(&catalog, Some("private/absent-model"), &forbidden_fetch())
        .expect_err("nothing defines this model");

    for needle in [
        "private/absent-model",
        "provider",
        "ZUNO_DISABLE_MODELS_FETCH",
        "https://models.dev",
        "/nowhere/cache/zuno/models.json",
        "ZUNO_MODELS_PATH",
    ] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}`, so it is actionable rather than \
             surfacing later as an empty model list: {message}"
        );
    }
}

/// With nothing requested and nothing selectable, the policy must still be named.
///
/// "No available model" alone reads as "you configured nothing", which is the
/// mis-diagnosis this whole todo was about.
#[test]
fn an_empty_catalog_with_no_request_still_explains_the_policy() {
    let catalog = Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new(),
    );
    let message = select_model(&catalog, None, &forbidden_fetch())
        .expect_err("an empty catalog offers no default");
    assert!(message.contains("ZUNO_DISABLE_MODELS_FETCH"), "{message}");
    assert!(
        message.contains("/nowhere/cache/zuno/models.json"),
        "{message}"
    );

    // And a catalog that was genuinely loaded must NOT blame the flag.
    let loaded = select_model(&catalog, None, &CatalogProvenance::Fetched)
        .expect_err("an empty catalog offers no default");
    assert!(
        !loaded.contains("ZUNO_DISABLE_MODELS_FETCH"),
        "a loaded catalog that lists nothing is a configuration problem, not a \
         policy one: {loaded}"
    );
}

#[test]
fn new_session_and_user_message_are_persisted_together() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let session = resolve_session(&mut connection, &plan, now).expect("create session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "hello",
            message_id: None,
            now,
        },
    )
    .expect("persist prompt");

    let store = zuno_db::message::MessageStore::new(&connection);
    let messages = store
        .messages_for_session(&session.id)
        .expect("load messages");
    assert_eq!(messages.len(), 1);
    let grouped = store
        .parts_by_message(&[messages[0].id.clone()])
        .expect("load message parts");
    let parts = grouped
        .get(&messages[0].id)
        .expect("parts grouped under the message");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].data["text"], "hello");
}

/// The two tables that name a model do not name it the same way, and only a test
/// that reads the persisted bytes can tell.
///
/// A session row spelled `modelID` has no `id`, which is what the released
/// TypeScript binary decodes (`session.ts:88-93`); it rejects the whole listing with
/// `Expected string, got undefined` and exit 1. Writing and reading the row through
/// this port alone passes with either spelling, so these assert on the stored JSON's
/// **keys** rather than on a round trip.
#[test]
fn a_persisted_session_names_its_model_id_the_way_upstream_reads_it() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let session = resolve_session(&mut connection, &plan, now).expect("create session");

    let stored: String = connection
        .query_row(
            "SELECT model FROM session WHERE id = ?1",
            [&session.id],
            |row| row.get(0),
        )
        .expect("read the persisted model column");
    let model: serde_json::Value = serde_json::from_str(&stored).expect("the column holds JSON");
    let keys = model.as_object().expect("a model object");

    assert_eq!(
        model["id"], "model",
        "upstream's session decoder reads `row.model.id` (session.ts:88-93); a \
         session_list on the released binary dies without it. Stored: {stored}"
    );
    assert!(
        !keys.contains_key("modelID"),
        "`modelID` is the *message* spelling (message.ts:121-125). A session row \
         carrying it is the defect this test exists for. Stored: {stored}"
    );
    assert_eq!(model["providerID"], "provider");
    assert!(
        !keys.contains_key("variant"),
        "`variant` is optional upstream and this port has none to record, so it must \
         be omitted rather than written as null. Stored: {stored}"
    );
}

/// The mirror of the test above, so a later edit cannot "unify" the two shapes.
///
/// A message's model is `{modelID, providerID}` (`message.ts:121-125`). Renaming this
/// to `id` to match the session row would break the sibling boundary in exactly the
/// same way, and nothing else in the suite would notice.
#[test]
fn a_persisted_message_keeps_the_message_spelling_of_its_model() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let session = resolve_session(&mut connection, &plan, now).expect("create session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "hello",
            message_id: None,
            now,
        },
    )
    .expect("persist prompt");

    let store = zuno_db::message::MessageStore::new(&connection);
    let messages = store
        .messages_for_session(&session.id)
        .expect("load messages");
    let model = &messages[0].data["model"];
    let keys = model.as_object().expect("a model object");

    assert_eq!(
        model["modelID"], "model",
        "a message names the model `modelID` (message.ts:121-125): {model}"
    );
    assert_eq!(model["providerID"], "provider");
    assert!(
        !keys.contains_key("id"),
        "the session spelling must not leak into a message: {model}"
    );
}

#[test]
fn an_explicit_session_is_reused_rather_than_created() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let now = 1_780_000_000_000;
    let created = {
        let plan = plan("/workspace", SessionChoice::New);
        ensure_project(&connection, &plan.project, now).expect("persist project");
        resolve_session(&mut connection, &plan, now).expect("create session")
    };

    let reused = resolve_session(
        &mut connection,
        &plan("/workspace", SessionChoice::Existing(created.id.clone())),
        now,
    )
    .expect("reuse the named session");
    assert_eq!(reused.id, created.id);

    let continued = resolve_session(
        &mut connection,
        &plan("/workspace", SessionChoice::Continue),
        now,
    )
    .expect("continue the directory's most recent session");
    assert_eq!(continued.id, created.id);
}

#[test]
fn session_choice_resolves_the_two_flags_into_one_answer() {
    assert_eq!(SessionChoice::resolve(None, false), SessionChoice::New);
    assert_eq!(SessionChoice::resolve(None, true), SessionChoice::Continue);
    assert_eq!(
        SessionChoice::resolve(Some("ses_1"), true),
        SessionChoice::Existing("ses_1".to_owned())
    );
}

#[test]
fn all_three_internals_resolve_with_the_roster_prompt_and_a_reachable_model() {
    // Given: a catalog with two models and a per-agent override for `title` only.
    let (catalog, config) = catalog_with_two_models_and_a_title_override();
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    // When: the internals are resolved.
    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("every internal resolves");

    // Then: the overridden one took its override, the other two inherited the
    // session's model, and all three carry the roster's prompt.
    assert_eq!(internals.title.model.model_id, "small");
    assert_eq!(internals.compaction.model.model_id, "big");
    assert_eq!(internals.summary.model.model_id, "big");
    for internal in [&internals.title, &internals.compaction, &internals.summary] {
        assert!(
            !internal.prompt.trim().is_empty(),
            "`{}` resolved with no prompt, so its request would carry no instructions",
            internal.name
        );
    }
    assert_eq!(
        internals.title.prompt,
        zuno_catalog::agent::builtin::PROMPT_TITLE,
        "the title prompt was rewritten instead of read from the catalog"
    );
}

/// Every name the roster declares internal must resolve here.
///
/// The assertion is over [`zuno_agent::builtin::INTERNAL_NAMES`] and not over three
/// literals, so a fourth internal added to the roster fails this test rather than
/// silently becoming another declared-and-never-invoked entry — which is the exact
/// defect this wiring exists to remove.
#[test]
fn the_resolved_set_is_exactly_what_the_roster_calls_internal() {
    let (catalog, config) = catalog_with_two_models_and_a_title_override();
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();
    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("every internal resolves");

    let resolved: std::collections::BTreeSet<&str> = [
        internals.title.name.as_str(),
        internals.compaction.name.as_str(),
        internals.summary.name.as_str(),
    ]
    .into_iter()
    .collect();
    let declared: std::collections::BTreeSet<&str> =
        zuno_agent::builtin::INTERNAL_NAMES.into_iter().collect();
    assert_eq!(
        resolved, declared,
        "the roster declares internals this composition root does not resolve"
    );
}

#[test]
fn an_internal_pointed_at_another_provider_falls_back_and_says_why() {
    // Given: an override naming a model under a provider whose credential this turn
    // does not wire.
    let (catalog, _) = catalog_with_two_models_and_a_title_override();
    let config: zuno_config::schema::Config =
        serde_json::from_str(r#"{"agent":{"summary":{"model":"elsewhere/some-model"}}}"#)
            .expect("config");
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("a declined override is not a failure");

    assert_eq!(internals.summary.model.model_id, "big");
    assert!(
        notes.iter().any(|note| note.contains("summary")),
        "the downgrade was silent; notes: {notes:?}"
    );
}

/// A catalog holding one provider whose endpoint is placed wherever the caller says.
///
/// `api` is the top-level provider key that feeds `model.api.url`; `endpoint` and
/// `base_url` go into `provider.probe.options`.
fn endpoint_catalog(api: Option<&str>, endpoint: Option<&str>, base_url: Option<&str>) -> Catalog {
    let mut options = serde_json::Map::new();
    if let Some(endpoint) = endpoint {
        options.insert("endpoint".to_owned(), serde_json::json!(endpoint));
    }
    if let Some(base_url) = base_url {
        options.insert("baseURL".to_owned(), serde_json::json!(base_url));
    }
    let mut provider = serde_json::Map::new();
    provider.insert(
        "npm".to_owned(),
        serde_json::json!("@ai-sdk/openai-compatible"),
    );
    if let Some(api) = api {
        provider.insert("api".to_owned(), serde_json::json!(api));
    }
    provider.insert("options".to_owned(), serde_json::Value::Object(options));
    provider.insert(
        "models".to_owned(),
        serde_json::json!({"probe-model": {"id": "probe-model"}}),
    );
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": serde_json::Value::Object(provider)}
    }))
    .expect("config");
    Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    )
}

fn probe_spec(
    api: Option<&str>,
    endpoint: Option<&str>,
    base_url: Option<&str>,
) -> Result<Spec, String> {
    probe_spec_in(api, endpoint, base_url, &Env::empty())
}

/// The same probe, resolved against an explicit environment.
///
/// Separate from [`probe_spec`] so the ladder cases keep asserting against an
/// environment that carries nothing: a placeholder-free URL must resolve identically
/// whatever is set, and a fixture that quietly supplied variables would hide a fix that
/// only worked when they happened to be present.
fn probe_spec_in(
    api: Option<&str>,
    endpoint: Option<&str>,
    base_url: Option<&str>,
    env: &Env,
) -> Result<Spec, String> {
    let catalog = endpoint_catalog(api, endpoint, base_url);
    let model = catalog
        .model("probe", "probe-model")
        .expect("the config declares the model");
    model_spec(&catalog, model, env)
}

/// The whole ladder, rung by rung — `provider.ts:1698-1700` plus `:355-358`.
///
/// Each case gives the winning rung a distinct URL, so a reordering changes the
/// asserted value rather than merely changing which of two identical URLs was used.
/// The first case is the defect: `options.baseURL` alone, the shape the upstream docs
/// show, which reached the transport with no endpoint at all.
#[test]
fn the_endpoint_comes_from_options_before_the_catalog() {
    let cases = [
        (
            (None, None, Some("https://from-base-url/v1")),
            "https://from-base-url/v1",
            "`options.baseURL` alone must be the endpoint",
        ),
        (
            (None, Some("https://from-endpoint/v1"), None),
            "https://from-endpoint/v1",
            "`options.endpoint` alone must be the endpoint",
        ),
        (
            (
                None,
                Some("https://from-endpoint/v1"),
                Some("https://from-base-url/v1"),
            ),
            "https://from-endpoint/v1",
            "`options.endpoint` must beat `options.baseURL`",
        ),
        (
            (Some("https://from-api/v1"), None, None),
            "https://from-api/v1",
            "a catalog-supplied `api.url` must still work when no option names one",
        ),
        (
            (
                Some("https://from-api/v1"),
                None,
                Some("https://from-base-url/v1"),
            ),
            "https://from-base-url/v1",
            "`options.baseURL` must beat the catalog's `api.url`",
        ),
        (
            (
                Some("https://from-api/v1"),
                Some("https://from-endpoint/v1"),
                Some("https://from-base-url/v1"),
            ),
            "https://from-endpoint/v1",
            "`options.endpoint` must beat everything",
        ),
    ];

    for ((api, endpoint, base_url), expected, why) in cases {
        let spec = probe_spec(api, endpoint, base_url).expect("an endpoint resolves");
        assert_eq!(
            spec.base_url.as_deref(),
            Some(expected),
            "{why} (api={api:?}, endpoint={endpoint:?}, baseURL={base_url:?})"
        );
    }
}

/// An empty option is not an endpoint — `provider.ts:1699` tests the string for `!== ""`.
///
/// Without the emptiness test, `"baseURL": ""` would win the ladder and produce a spec
/// whose base URL is the empty string, which the transport would happily prepend
/// nothing to and dial a relative path.
#[test]
fn an_empty_endpoint_option_falls_through_to_the_next_rung() {
    let spec = probe_spec(Some("https://from-api/v1"), Some(""), Some(""))
        .expect("the catalog rung still answers");
    assert_eq!(spec.base_url.as_deref(), Some("https://from-api/v1"));
}

/// A URL naming no variable is returned byte for byte — `provider.ts:1712-1715`.
///
/// The expansion pass runs on every turn, so the case that must not change anything is
/// the case that runs almost always. The awkward inputs are here on purpose: `${}` has
/// an empty name and the oracle's `[^}]+` needs at least one character, `${unclosed` has
/// no terminator, and a bare `$` or `{` is not a placeholder at all. Each would be an
/// easy off-by-one in a hand-rolled scan, and each would corrupt a perfectly ordinary
/// URL that happens to contain a brace.
///
/// The environment binds the **empty name** as well as `SET`, and that is not
/// decoration: with only `SET` bound, dropping the scan's `offset > 0` guard left `${}`
/// looking up a name nothing carried, which fell back to the literal and produced a
/// byte-identical answer — the test passed while the guard was gone. A binding for `""`
/// is what makes `${}` staying literal an observable claim rather than a coincidence.
/// Nothing can export an empty-named variable through a POSIX `environ`, so this only
/// ever comes from a constructed [`Env`]; the guard exists because the oracle's `[^}]+`
/// demands at least one character, not because the input is reachable.
#[test]
fn a_url_naming_no_variable_is_unchanged_by_expansion() {
    let env = Env::from_pairs([("SET", "substituted"), ("", "empty-name")]);
    for url in [
        "https://api.example.com/v1",
        "http://127.0.0.1:8080/v1",
        "https://api.example.com/v1?filter=a{b}c",
        "https://api.example.com/${}/v1",
        "https://api.example.com/${unclosed/v1",
        "https://api.example.com/$SET/v1",
        "https://api.example.com/{SET}/v1",
        "",
    ] {
        assert_eq!(
            expand_variables(url, &env),
            url,
            "expansion must not alter a URL that names no variable"
        );
    }
}

/// A set variable substitutes; an unset one keeps its literal `${VAR}`.
///
/// `?? item` (`provider.ts:1714`) is the whole of the second half. Substituting the
/// empty string for an unset name would turn `https://${REGION}.api.example.com/v1`
/// into `https://.api.example.com/v1` — a *different host*, silently — so this is the
/// one case where the wrong answer is worse than no answer.
#[test]
fn an_unset_variable_keeps_its_placeholder_while_a_set_one_substitutes() {
    let env = Env::empty()
        .with("REGION", "eu-west-1")
        .with("SUFFIX", "example.com")
        .with("BLANK", "");
    let cases = [
        (
            "https://${REGION}.api.example.com/v1",
            "https://eu-west-1.api.example.com/v1",
            "a set variable must substitute",
        ),
        (
            "https://${MISSING}.api.example.com/v1",
            "https://${MISSING}.api.example.com/v1",
            "an unset variable must keep its literal placeholder, not collapse the host",
        ),
        (
            "https://${REGION}.api.${SUFFIX}/v1",
            "https://eu-west-1.api.example.com/v1",
            "every placeholder in one URL must be expanded",
        ),
        (
            "https://${REGION}.api.${MISSING}/v1",
            "https://eu-west-1.api.${MISSING}/v1",
            "one unset variable must not stop the others from expanding",
        ),
        (
            "https://api.${BLANK}example.com/v1",
            "https://api.example.com/v1",
            "a variable set to the empty string substitutes empty — `\"\"` is not \
             nullish in the oracle either",
        ),
    ];

    for (url, expected, why) in cases {
        assert_eq!(expand_variables(url, &env), expected, "{why}");
    }
}

/// Expansion applies to whichever rung the ladder chose, all three of them.
///
/// The defect is one step past todo 109: the ladder was already correct, and every rung
/// it could choose was then dialled literally. A fix that only expanded `model.api.url`
/// — the field whose doc comment promised expansion — would leave the two option rungs
/// broken, so each is asserted separately here.
#[test]
fn every_endpoint_rung_is_expanded_after_it_wins() {
    let env = Env::empty().with("HOST", "gateway.internal");
    let cases = [
        (
            (Some("https://${HOST}/v1"), None, None),
            "the catalog's `api.url` must be expanded",
        ),
        (
            (None, Some("https://${HOST}/v1"), None),
            "`options.endpoint` must be expanded",
        ),
        (
            (None, None, Some("https://${HOST}/v1")),
            "`options.baseURL` must be expanded",
        ),
    ];

    for ((api, endpoint, base_url), why) in cases {
        let spec = probe_spec_in(api, endpoint, base_url, &env).expect("an endpoint resolves");
        assert_eq!(
            spec.base_url.as_deref(),
            Some("https://gateway.internal/v1"),
            "{why}"
        );
    }
}

/// A rung is chosen on its unexpanded text, and expanded only afterwards.
///
/// The oracle tests `options["baseURL"] !== ""` at `:1699-1700` and expands at `:1712`, in
/// that order. `BLANK` is set to the empty string, so `"baseURL": "${BLANK}"` is a
/// non-empty rung that wins the ladder and *then* becomes empty — it does not fall
/// through to the catalog's `api.url`. Moving expansion ahead of
/// [`super::provider_endpoint`] would produce `https://from-api/v1` here, which is why
/// this case exists rather than a second happy-path one.
#[test]
fn a_rung_is_chosen_before_expansion_not_after() {
    let env = Env::empty().with("BLANK", "");
    let spec = probe_spec_in(Some("https://from-api/v1"), None, Some("${BLANK}"), &env)
        .expect("a non-empty rung was available before expansion");
    assert_eq!(
        spec.base_url.as_deref(),
        Some(""),
        "`options.baseURL` was non-empty when the ladder read it, so it must win and \
         then expand to nothing; falling through to `api.url` means expansion ran first"
    );
}

/// Neither an endpoint key nor `apiKey` is ever forwarded as an SDK option.
///
/// [`model_spec`] now forwards **both** bags — the provider's, seeded first, and the
/// model's overlaid on top — so the exclusion has to hold on both, and the keys are
/// planted in both here. `Spec::options` is read by allow-listed key today —
/// `capabilities`, `extraBody`, `useCompletionUrls` — so a stray `baseURL` or `apiKey`
/// there is inert, and inert-today is exactly how it would go unnoticed until someone
/// widened that read and a request body grew a field named after a URL, or one carrying
/// key material. Every other option must still come through, or this becomes a filter
/// that eats configuration.
#[test]
fn the_endpoint_keys_do_not_also_travel_in_the_option_bag() {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config = serde_json::from_str(
        r#"{"provider":{"probe":{"options":{
               "baseURL":"https://from-base-url/v1",
               "endpoint":"https://from-base-url/v1",
               "apiKey":"sk-provider-level",
               "providerKept":true},
             "models":{"probe-model":{"options":{
               "baseURL":"https://model-level/v1",
               "endpoint":"https://model-level-endpoint/v1",
               "apiKey":"sk-model-level",
               "extraBody":{"kept":true}}}}}}}"#,
    )
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let provider = catalog.provider("probe").expect("the provider");
    let model = catalog.model("probe", "probe-model").expect("the model");
    for (label, options) in [("provider", &provider.options), ("model", &model.options)] {
        for key in ["baseURL", "endpoint", "apiKey"] {
            assert!(
                options.contains_key(key),
                "the fixture lost `{key}` from the {label}'s options in the merge, so \
                 the exclusion it is meant to prove is untested"
            );
        }
    }

    let spec = model_spec(&catalog, model, &Env::empty())
        .expect("the provider option supplies the endpoint");

    assert_eq!(
        spec.base_url.as_deref(),
        Some("https://from-base-url/v1"),
        "a model-level endpoint key is not an endpoint source; only the provider's is \
         (`provider.ts:1698-1700` reads `provider.options`)"
    );
    for key in ["endpoint", "baseURL", "apiKey"] {
        assert!(
            !spec.options.contains_key(key),
            "`{key}` reached the SDK option bag: {:?}",
            spec.options
        );
    }
    assert_eq!(
        spec.options.get("extraBody"),
        Some(&serde_json::json!({"kept": true})),
        "every model option that is not resolved elsewhere must still reach the SDK"
    );
    assert_eq!(
        spec.options.get("providerKept"),
        Some(&serde_json::json!(true)),
        "every provider option that is not resolved elsewhere must still reach the SDK"
    );
    assert!(
        !format!("{:?}", spec.options).contains("sk-"),
        "key material reached the option bag: {:?}",
        spec.options
    );
}

/// No endpoint anywhere fails immediately and names the key that supplies one.
///
/// The pre-fix path composed the whole turn and then said `unrecoverable provider
/// failure (status=None)` from the transport, which names nothing actionable.
#[test]
fn a_provider_with_no_endpoint_anywhere_names_the_key_to_set() {
    let message = probe_spec(None, None, None).expect_err("nothing supplies an endpoint");
    for needle in ["provider.probe.options", "baseURL", "endpoint"] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}`: {message}"
        );
    }
}

/// The endpoint injected into every fixture built by [`options_spec`].
const PROBE_ENDPOINT: &str = "https://gateway.probe/v1";

/// The spec one provider/model option pair produces, endpoint supplied for free.
///
/// `baseURL` is injected into the provider's options rather than left to the caller so
/// no test here can accidentally assert on a `Spec` that [`model_spec`] refused to
/// build — and so the endpoint-exclusion test operates on a `baseURL` that is genuinely
/// load-bearing rather than a decorative one.
fn options_spec(provider_options: &serde_json::Value, model_options: &serde_json::Value) -> Spec {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let mut provider = provider_options
        .as_object()
        .cloned()
        .expect("the provider options are an object");
    provider.insert("baseURL".to_owned(), serde_json::json!(PROBE_ENDPOINT));
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": {
            "options": serde_json::Value::Object(provider),
            "models": {"probe-model": {"options": model_options}}
        }}
    }))
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let model = catalog.model("probe", "probe-model").expect("the model");
    model_spec(&catalog, model, &Env::empty()).expect("the injected endpoint resolves")
}

/// A resolved provider carrying exactly `options`, for the credential precedence table.
fn probe_provider(options: serde_json::Value) -> Catalog {
    let document = serde_json::from_str(
        r#"{"probe":{"id":"probe","name":"Probe","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{"probe-model":{"id":"probe-model","name":"Probe Model",
               "limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": {"options": options}}
    }))
    .expect("config");
    Catalog::resolve(&document, &ResolveInput::new().with_config(&config))
}

/// `options.apiKey` is primary; the stored credential is the fallback — `:1719`.
///
/// The whole table in one test, because the defect was a *precedence* and a precedence
/// is only wrong relative to its alternatives: reading only the option breaks
/// `opencode auth login`, and reading only the credential is the bug this todo fixes.
/// Every expectation names a distinct string, so no row can pass by coincidence.
#[test]
fn an_options_api_key_is_primary_and_the_stored_credential_is_the_fallback() {
    let stored = zuno_auth::Credential::Api {
        key: zuno_auth::Secret::new("sk-from-the-store"),
        metadata: None,
    };
    let cases = [
        (
            serde_json::json!({"apiKey": "sk-from-options"}),
            true,
            Some("sk-from-options"),
            "`options.apiKey` must win when both are present",
        ),
        (
            serde_json::json!({"apiKey": "sk-from-options"}),
            false,
            Some("sk-from-options"),
            "`options.apiKey` alone must be the credential",
        ),
        (
            serde_json::json!({}),
            true,
            Some("sk-from-the-store"),
            "the stored credential must still authenticate when no option names a key",
        ),
        (
            serde_json::json!({}),
            false,
            None,
            "neither source means no credential, which a local endpoint is entitled to",
        ),
        (
            serde_json::json!({"apiKey": ""}),
            true,
            Some(""),
            "an explicitly empty `apiKey` is a key, not a fall-through: `:1719` tests \
             for `=== undefined`, and falling back here would present a real vendor \
             key to an endpoint the user never named",
        ),
    ];

    for (options, present, expected, why) in cases {
        let catalog = probe_provider(options.clone());
        let provider = catalog.provider("probe").expect("the provider resolves");
        assert_eq!(
            provider.options.get("apiKey"),
            options.get("apiKey"),
            "the fixture lost `apiKey` in the merge, so this row proves nothing"
        );

        let resolved = resolved_credential(Some(provider), present.then_some(&stored));

        assert_eq!(
            resolved.as_ref().map(credential_value).as_deref(),
            expected,
            "{why} (options={options})"
        );
    }
}

/// Why [`provider_api_key`]'s string test can never be reached from a config file.
///
/// `ProviderOptions::api_key` is typed `Option<String>`
/// (`zuno-config/src/schema/provider.rs:54`), so a non-string `apiKey` is refused before
/// any provider is resolved and the `as_str` guard is belt over braces — kept because
/// `ResolvedProvider::options` is a free-form JSON map that a future non-config source
/// could populate, and a number silently becoming `Bearer 7` is not an acceptable
/// outcome. Asserted rather than assumed, so a schema change that loosened the field
/// would show up here instead of at a gateway.
#[test]
fn a_non_string_api_key_never_reaches_the_resolved_provider() {
    let refused = serde_json::from_value::<zuno_config::schema::Config>(serde_json::json!({
        "provider": {"probe": {"options": {"apiKey": 7}}}
    }));

    assert!(
        refused.is_err(),
        "the config schema accepted a non-string `apiKey`, so `provider_api_key`'s \
         fall-through is now reachable and needs a case of its own"
    );
}

/// A provider-level option reaches the surface that reads it — `:1676`.
///
/// `useCompletionUrls` is the case worth pinning: it is a *provider* option in the
/// oracle (`provider.ts:265` passes `options?.["useCompletionUrls"]`), it has a reader
/// here, and before this fix setting it where the docs say to set it did nothing at all.
/// The assertion goes through that reader rather than stopping at
/// `spec.options.contains_key`, because "the bag holds the key" and "the code that
/// consults the bag sees it" are different claims and only the second one matters.
#[test]
fn a_provider_level_option_reaches_the_surface_that_reads_it() {
    let spec = options_spec(
        &serde_json::json!({"useCompletionUrls": true}),
        &serde_json::json!({}),
    );

    assert!(
        zuno_provider_compatible::surface::use_completion_urls(&spec),
        "a provider-level `useCompletionUrls` is still inert; options: {:?}",
        spec.options
    );
}

/// The model wins on collision, and the provider's other leaves survive — `:1497`.
///
/// Three claims in one fixture, each with its own witness: `shared` proves the direction,
/// `providerOnly` proves the merge is deep rather than a replace, and `modelOnly` proves
/// the model's own keys are not lost to the seed.
#[test]
fn a_model_option_wins_over_a_provider_option_of_the_same_name() {
    let spec = options_spec(
        &serde_json::json!({
            "extraBody": {"shared": "from-the-provider", "providerOnly": "kept"},
            "providerScalar": "kept"
        }),
        &serde_json::json!({
            "extraBody": {"shared": "from-the-model", "modelOnly": "kept"},
            "providerScalar": "replaced"
        }),
    );

    assert_eq!(
        spec.options.get("extraBody"),
        Some(&serde_json::json!({
            "shared": "from-the-model",
            "providerOnly": "kept",
            "modelOnly": "kept"
        })),
        "the provider/model overlay is not a deep merge with the model winning"
    );
    assert_eq!(
        spec.options.get("providerScalar"),
        Some(&serde_json::json!("replaced")),
        "a provider-level scalar overrode the model's, so the direction is inverted"
    );
}

/// An internal whose own model has no endpoint is declined, not fatal.
///
/// A per-model `provider.api` can give one model in a provider an endpoint and leave
/// another without one. Propagating that with `?` would lose the whole turn because a
/// title agent could not be reached, so it takes the same downgrade-and-say-why path as
/// a cross-provider or unsupported-transport override.
#[test]
fn an_internal_whose_model_has_no_endpoint_falls_back_and_says_why() {
    // Given: `big` carries its own endpoint, `small` carries none, and `title` is
    // pointed at `small`.
    let document = serde_json::from_str(
        r#"{"test":{"id":"test","name":"Test","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{
               "big":{"id":"big","name":"Big","limit":{"context":200000,"output":8192}},
               "small":{"id":"small","name":"Small","limit":{"context":100000,"output":4096}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config = serde_json::from_str(
        r#"{"provider":{"test":{"models":{
             "big":{"provider":{"api":"https://gateway.test/v1"}},
             "small":{}}}},
             "agent":{"title":{"model":"test/small"}}}"#,
    )
    .expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    assert!(
        catalog
            .model("test", "small")
            .expect("small resolves")
            .api
            .url
            .is_empty(),
        "the fixture must leave `small` without an endpoint or it proves nothing"
    );
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    // When: the internals resolve.
    let env = Env::empty();
    let internals = resolve_internals(
        ResolveInternalsInput {
            config: &config,
            catalog: &catalog,
            provider_id: "test",
            model_id: "big",
            session_model,
            env: &env,
            plugin_small_model: None,
        },
        &mut notes,
    )
    .expect("an unreachable override is a downgrade, not a failure");

    // Then: title fell back to the session model and said so.
    assert_eq!(internals.title.model.model_id, "big");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("title") && note.contains("baseURL")),
        "the downgrade was silent or did not name the missing key; notes: {notes:?}"
    );
}

#[test]
fn a_catalog_limit_that_is_absent_or_negative_reads_as_no_window() {
    assert_eq!(token_count(200_000.0), 200_000);
    assert_eq!(token_count(0.0), 0);
    assert_eq!(token_count(-1.0), 0);
    assert_eq!(token_count(f64::NAN), 0);
    assert_eq!(token_count(f64::INFINITY), 0);
}

#[test]
fn production_registry_exposes_all_three_goal_tools() {
    let directory = tempfile::TempDir::new().expect("temporary tool workspace");
    let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
    let selected_agent = agent("build");
    let runtime = tool_runtime::assemble(
        directory.path(),
        None,
        &Env::empty(),
        &zuno_config::schema::Config::default(),
        &selected_agent,
        tool_runtime::ToolSelection {
            provider_id: "provider",
            model_id: "model",
            manifest: Arc::new(zuno_harness::ToolManifest::all()),
            contributions: Arc::new(zuno_harness::ToolContributions::default()),
            question: None,
            todo_store: Arc::new(
                zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("in-memory todo store"),
            ),
            goal_store: Arc::new(
                GoalStore::open_memory(goal_spill.path().to_owned()).expect("in-memory goal store"),
            ),
            mcp_loader: None,
            skills: Arc::new(zuno_catalog::skill::Skills::default()),
            delegation: test_delegation(),
        },
    )
    .expect("production registry assembles");
    let ids = runtime
        .tools
        .iter()
        .map(|tool| tool.id())
        .collect::<Vec<_>>();

    for goal_tool in ["get_goal", "create_goal", "update_goal"] {
        assert!(
            ids.contains(&goal_tool),
            "production registry is missing `{goal_tool}`; visible tools: {ids:?}"
        );
    }
}

#[test]
fn goal_dynamic_context_is_rebuilt_from_authoritative_sql_for_each_request() {
    let spill = tempfile::tempdir().expect("temporary goal spill directory");
    let store =
        Arc::new(GoalStore::open_memory(spill.path().to_owned()).expect("in-memory goal store"));
    let continuation = GoalContinuation::new(Arc::clone(&store), SessionRunRegistry::new());
    let first_goal = store
        .create_goal("ses_goal_context", "first objective", None)
        .expect("create goal");
    let first = continuation
        .injection("ses_goal_context")
        .expect("read first injection")
        .map(|entry| dynamic_context_from_goal_entry(&entry))
        .expect("goal context exists");
    assert_eq!(
        first,
        DynamicContext::new(zuno_goal::render_goal_context(&first_goal))
    );

    let second_goal = store
        .update_objective("ses_goal_context", "second objective from SQL")
        .expect("update objective")
        .expect("goal exists");
    let second = continuation
        .injection("ses_goal_context")
        .expect("read second injection")
        .map(|entry| dynamic_context_from_goal_entry(&entry))
        .expect("goal context exists");
    assert_eq!(
        second,
        DynamicContext::new(zuno_goal::render_goal_context(&second_goal))
    );
    assert_ne!(
        first, second,
        "the second request reused stale goal context"
    );
}

#[test]
fn goal_usage_delta_includes_every_assistant_step_and_token_bucket() {
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    let fixture_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &fixture_plan.project, now).expect("persist project");
    let session =
        resolve_session(&mut connection, &fixture_plan, now).expect("create fixture session");
    let store = zuno_db::message::MessageStore::new(&connection);
    let assistant = |id: &str, created: i64, values: [i64; 5]| {
        zuno_db::message::MessageRecord::from_json(serde_json::json!({
            "id": id,
            "sessionID": session.id,
            "role": "assistant",
            "time": { "created": created, "completed": created + 1 },
            "parentID": "msg_parent",
            "modelID": "model",
            "providerID": "provider",
            "mode": "build",
            "agent": "build",
            "path": { "cwd": "/workspace", "root": "/workspace" },
            "cost": 0,
            "tokens": {
                "input": values[0],
                "output": values[1],
                "reasoning": values[2],
                "cache": { "read": values[3], "write": values[4] }
            },
            "finish": "stop"
        }))
        .expect("valid assistant message")
    };
    store
        .put_message(&assistant("msg_baseline", now, [1, 1, 1, 1, 1]))
        .expect("persist baseline assistant");
    let before = goal_usage(&connection, &session.id).expect("read usage before turn");
    store
        .put_message(&assistant("msg_step_1", now + 2, [1, 2, 3, 4, 5]))
        .expect("persist first assistant step");
    store
        .put_message(&assistant("msg_step_2", now + 4, [10, 20, 30, 40, 50]))
        .expect("persist second assistant step");
    let after = goal_usage(&connection, &session.id).expect("read usage after turn");

    assert_eq!(after.tokens - before.tokens, 165);
}

/// Neither surface may compose a turn or bypass the selected driver.
///
/// The whole point of this module is that `run` and the TUI cannot drift apart in
/// which tools exist, which rules govern them, or how a session is resolved — and
/// the way they would drift is a second composition root or a direct loop call.
#[test]
fn only_this_module_composes_a_turn() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    let composition = [
        "ToolRegistryDispatcher::new",
        ".service::<dyn AgentDriver>()",
        "self\n            .driver\n            .drive(",
    ];
    let mut scanned = 0_usize;
    for entry in std::fs::read_dir(&directory).expect("the command directory is readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Test modules are excluded because they do not compose production turns —
        // and because this file names both needles in its own assertion message.
        if name.ends_with("_tests.rs") {
            continue;
        }
        scanned += 1;
        let source = std::fs::read_to_string(&path).expect("a readable source file");
        for needle in composition {
            let occurrences = source.matches(needle).count();
            let expected = usize::from(name == "turn.rs");
            assert_eq!(
                occurrences, expected,
                "`{name}` mentions `{needle}` {occurrences} time(s); the turn \
                 composition belongs to `turn.rs` and to nothing else, because a \
                 second call site is how two surfaces come to offer different tools"
            );
        }
        assert_eq!(
            source.matches("run_turn(").count(),
            0,
            "`{name}` bypasses the active AgentDriver"
        );
    }
    assert!(
        scanned >= 17,
        "scanned only {scanned} files under {}; the scan is looking in the wrong \
         place and would pass vacuously",
        directory.display()
    );

    let driver =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../zuno-engine/src/driver.rs");
    let driver = std::fs::read_to_string(&driver).expect("the default driver source is readable");
    assert_eq!(
        driver.matches("run_turn(").count(),
        1,
        "only DefaultAgentDriver may own the built-in loop call"
    );
}

/// One value of every [`TurnError`] variant, so a claim about rendering covers all of
/// them rather than the two a bug report happened to name.
///
/// Each carries a distinctive payload — `session-in-the-message`, `agent-in-the-message`
/// — because the assertions below check that the payload survives, and a shared
/// placeholder would let one variant pass on another's text.
fn every_turn_error() -> Vec<TurnError> {
    vec![
        TurnError::NoUserMessage {
            session_id: "ses_in-the-message".to_owned(),
        },
        TurnError::MissingUserField {
            message_id: "msg_in-the-message".to_owned(),
            field: "agent",
        },
        TurnError::AgentNotFound {
            agent: "agent-in-the-message".to_owned(),
        },
        TurnError::ModelNotFound {
            provider_id: "provider-in-the-message".to_owned(),
            model_id: "model-in-the-message".to_owned(),
        },
        TurnError::StepLimit {
            agent: "agent-in-the-message".to_owned(),
            max_steps: 100,
        },
        TurnError::StreamEndedWithoutMessageEnd { step: 3 },
        TurnError::EmptyAssistantMessage {
            provider_id: "empty-provider-in-the-message".to_owned(),
            step: 4,
        },
        TurnError::NestedToolUse { step: 5 },
        TurnError::ToolUseEndWithoutStart { step: 6 },
        TurnError::EventConsumerClosed,
        TurnError::Hook(
            "plugin `fixture-plugin` failed hook `chat.params`: fixture failure".to_owned(),
        ),
        TurnError::Database(zuno_error::DbError::Busy { retry_after: None }),
        TurnError::Provider(ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        }),
        TurnError::Provider(ProviderError::RateLimited { retry_after: None }),
        TurnError::Provider(ProviderError::Transient {
            status: None,
            source: Some(Box::new(std::io::Error::other(
                "error sending request for url (http://gateway.invalid/v1)",
            ))),
        }),
        TurnError::Provider(ProviderError::Auth {
            provider: "test".to_owned(),
            source: None,
        }),
        TurnError::Provider(ProviderError::Refused {
            provider: "test".to_owned(),
            provider_text: None,
        }),
        TurnError::Provider(ProviderError::Fatal {
            status: Some(400),
            source: None,
        }),
        TurnError::ProviderRetryDeadlineExceeded {
            attempt: 2,
            elapsed: std::time::Duration::from_secs(180),
        },
        TurnError::Cache(zuno_llm::cache::CacheViolation::StaticPrefixChanged { turn: 2 }),
    ]
}

/// The category every existing assertion reads still leads the message.
///
/// This is the guarantee the chain walk had to keep. Todo 109's and 110's tests, the
/// compaction suite's `unrecoverable provider failure` check and anything a user has
/// scripted all read the front of the line; appending causes must not move it. Asserted
/// for every variant rather than the ones with known assertions, because the next
/// assertion will be written against whichever variant this test did not cover.
#[test]
fn every_variant_keeps_its_category_at_the_front_of_the_message() {
    for error in every_turn_error() {
        let category = error.to_string();
        let rendered = describe_turn_failure(&error, None);

        assert!(
            rendered.starts_with(&category),
            "the category moved: expected `{rendered}` to start with `{category}`"
        );
    }
}

/// A new [`TurnError`] variant must break this, so its author decides what is rendered.
///
/// `every_turn_error` is a hand-written list, and a hand-written list of an enum's
/// variants silently goes stale — which is exactly how a new failure class would arrive
/// rendering as a bare category again. The match is exhaustive with no wildcard arm, so
/// the compiler refuses to build this file until the new variant is named, and the count
/// refuses to pass until it is also constructed.
#[test]
fn the_variant_table_covers_the_whole_enum() {
    let mut named = std::collections::BTreeSet::new();
    for error in every_turn_error() {
        let name = match &error {
            TurnError::NoUserMessage { .. } => "NoUserMessage",
            TurnError::MissingUserField { .. } => "MissingUserField",
            TurnError::AgentNotFound { .. } => "AgentNotFound",
            TurnError::ModelNotFound { .. } => "ModelNotFound",
            TurnError::StepLimit { .. } => "StepLimit",
            TurnError::StreamEndedWithoutMessageEnd { .. } => "StreamEndedWithoutMessageEnd",
            TurnError::EmptyAssistantMessage { .. } => "EmptyAssistantMessage",
            TurnError::NestedToolUse { .. } => "NestedToolUse",
            TurnError::ToolUseEndWithoutStart { .. } => "ToolUseEndWithoutStart",
            TurnError::EventConsumerClosed => "EventConsumerClosed",
            TurnError::Hook(_) => "Hook",
            TurnError::Database(_) => "Database",
            TurnError::Provider(_) => "Provider",
            TurnError::ProviderRetryDeadlineExceeded { .. } => "ProviderRetryDeadlineExceeded",
            TurnError::Cache(_) => "Cache",
        };
        named.insert(name);
    }

    assert_eq!(
        named.len(),
        15,
        "the table covers only {named:?}; every variant needs a value or the rendering \
         claims above are vacuous for the ones missing"
    );
}

#[test]
fn a_plugin_hook_failure_names_the_plugin_and_hook_on_the_user_surface() {
    let error = TurnError::Hook(
        "plugin `fixture-plugin` failed hook `chat.params`: fixture failure".to_owned(),
    );

    let rendered = describe_turn_failure(&error, None);

    assert!(rendered.contains("fixture-plugin"), "{rendered}");
    assert!(rendered.contains("chat.params"), "{rendered}");
    assert!(rendered.contains("fixture failure"), "{rendered}");
}

/// A wrapped failure names its cause instead of only its class.
///
/// The measured defect: an unreachable endpoint, a dead port, a TLS refusal and an
/// unexpanded `${VAR}` all rendered as `transient provider failure (status=None)`, with
/// the URL one `source()` call away the whole time.
#[test]
fn a_transport_failure_names_the_url_it_could_not_reach() {
    let error = TurnError::Provider(ProviderError::transient(std::io::Error::other(
        "error sending request for url (http://${GW_HOST}/v1/chat/completions)",
    )));

    let rendered = describe_turn_failure(&error, None);

    assert!(
        rendered.contains("http://${GW_HOST}/v1/chat/completions"),
        "the transport error's URL was dropped: {rendered}"
    );
    assert!(
        rendered.starts_with("transient provider failure (status=None)"),
        "the category must still lead: {rendered}"
    );
}

/// The credential the turn presented never reaches the message, even echoed verbatim.
///
/// Todo 110 guaranteed no key material on the auth path by building its message from
/// the provider id alone. Walking the `#[source]` chain renders whatever the gateway put
/// in its 401 body, and a gateway answering `Incorrect API key provided: sk-…` is a real
/// shape — so the guarantee now needs enforcing rather than following from the message's
/// construction.
#[test]
fn a_rejected_credential_is_scrubbed_from_the_body_that_echoed_it() {
    let secret = "sk-SUPERSECRET-DO-NOT-ECHO";
    let error = TurnError::Provider(ProviderError::Auth {
        provider: "test".to_owned(),
        source: Some(Box::new(std::io::Error::other(format!(
            "provider `test` returned HTTP 401: {{\"error\":{{\"message\":\"Incorrect API \
             key provided: {secret}\"}}}}"
        )))),
    });

    let rendered = describe_turn_failure(&error, Some(secret));

    assert!(
        !rendered.contains(secret),
        "the rendered failure echoed the key it presented: {rendered}"
    );
    assert!(
        rendered.contains(REDACTED),
        "the key was dropped without a trace, so the message reads as if the gateway \
         said nothing: {rendered}"
    );
    for needle in ["provider.test.options.apiKey", "zuno auth login test"] {
        assert!(
            rendered.contains(needle),
            "scrubbing cost the advice `{needle}`: {rendered}"
        );
    }
}

/// An empty credential is a legitimate configuration and must not corrupt the message.
///
/// `provider_api_key` documents why `apiKey: ""` reaches this path: it means "this
/// endpoint takes no key". `str::replace` with an empty pattern inserts its replacement
/// between every character, so an unguarded scrub would turn every failure a keyless
/// local endpoint produces into unreadable noise.
#[test]
fn an_empty_credential_scrubs_nothing() {
    let error = TurnError::Provider(ProviderError::transient(std::io::Error::other(
        "tcp connect error: Connection refused",
    )));

    assert_eq!(
        describe_turn_failure(&error, Some("")),
        describe_turn_failure(&error, None),
        "an empty credential changed the message it appears in"
    );
}

async fn spawn_turn_response_server(
    chunks: Vec<(std::time::Duration, Vec<u8>)>,
    finish: bool,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind turn-response fixture");
    let address = listener.local_addr().expect("turn fixture address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept turn request");
        read_http_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\n\
                  Content-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\
                  Connection: close\r\n\
                  \r\n",
            )
            .await
            .expect("write turn response headers");

        for (delay, chunk) in chunks {
            tokio::time::sleep(delay).await;
            socket
                .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                .await
                .expect("write turn chunk size");
            socket
                .write_all(&chunk)
                .await
                .expect("write turn response chunk");
            socket
                .write_all(b"\r\n")
                .await
                .expect("terminate turn response chunk");
        }

        if finish {
            socket
                .write_all(b"0\r\n\r\n")
                .await
                .expect("finish turn response");
        } else {
            std::future::pending::<()>().await;
        }
    });
    (address, server)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let header_end = loop {
        let mut buffer = [0_u8; 4096];
        let bytes = socket.read(&mut buffer).await.expect("read turn request");
        assert!(bytes > 0, "turn request ended before its headers");
        request.extend_from_slice(&buffer[..bytes]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let content_length = std::str::from_utf8(&request[..header_end])
        .expect("turn request headers are UTF-8")
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric content length")
                })
            })
        })
        .unwrap_or_default();
    while request.len() - header_end < content_length {
        let mut buffer = [0_u8; 4096];
        let bytes = socket
            .read(&mut buffer)
            .await
            .expect("read turn request body");
        assert!(bytes > 0, "turn request ended before its body");
        request.extend_from_slice(&buffer[..bytes]);
    }
    assert!(
        request.starts_with(b"POST /v1/chat/completions "),
        "real turn used an unexpected endpoint"
    );
}

fn chat_delta(text: &str) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-turn-idle",
            "object": "chat.completion.chunk",
            "created": 1_780_000_000,
            "model": "model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": text},
                "finish_reason": null
            }]
        })
    )
    .into_bytes()
}

fn chat_end() -> Vec<u8> {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-turn-idle",
            "object": "chat.completion.chunk",
            "created": 1_780_000_000,
            "model": "model",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        })
    )
    .into_bytes()
}

async fn collect_turn_events(
    mut receiver: tokio::sync::mpsc::Receiver<TurnEvent>,
) -> Vec<TurnEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    events
}

async fn run_compatible_turn(
    chunks: Vec<(std::time::Duration, Vec<u8>)>,
    finish: bool,
    transport_idle: std::time::Duration,
) -> (
    Result<zuno_engine::r#loop::TurnOutcome, TurnError>,
    Vec<TurnEvent>,
    std::time::Duration,
) {
    let (address, server) = spawn_turn_response_server(chunks, finish).await;
    let transport: Arc<dyn Transport> = Arc::new(
        ReqwestTransport::new(COMPATIBLE_PROVIDER)
            .with_idle_timeout(StreamIdleTimeout::new(transport_idle)),
    );
    let provider_idle = StreamIdleTimeout::new(std::time::Duration::from_secs(2));
    let mut providers = ProviderRegistry::new();
    providers.register_fallible(COMPATIBLE_PROVIDER, move |spec| {
        let provider =
            zuno_provider_compatible::CompatibleProvider::new(spec, Arc::clone(&transport), None)?
                .with_idle_timeout(provider_idle);
        Ok(Arc::new(provider) as Arc<dyn Provider>)
    });

    let spec = Spec::new(COMPATIBLE_PROVIDER)
        .with_surface(ApiSurface::Chat)
        .with_base_url(format!("http://{address}/v1"));
    let resolver = Resolver {
        requested_agent: "build".to_owned(),
        system_prompt: String::new(),
        max_steps: DEFAULT_MAX_STEPS,
        requested_provider: "provider".to_owned(),
        requested_model: "model".to_owned(),
        wire_model: "model".to_owned(),
        reasoning_options: serde_json::Map::new(),
        spec,
    };
    let mut connection =
        zuno_db::open::open(&zuno_paths::DbLocation::Memory).expect("open turn fixture database");
    zuno_db::migration::apply(&mut connection).expect("apply turn fixture schema");
    let fixture_plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &fixture_plan.project, now).expect("persist fixture project");
    let session =
        resolve_session(&mut connection, &fixture_plan, now).expect("create fixture session");
    persist_user_message(
        &connection,
        UserMessageInput {
            session_id: &session.id,
            agent: "build",
            provider_id: "provider",
            model_id: "model",
            text: "Show the streamed response.",
            message_id: None,
            now,
        },
    )
    .expect("persist fixture prompt");
    let interrupt = InterruptSignal::new();
    let dispatcher = ToolRegistryDispatcher::new(
        Vec::new(),
        Vec::new(),
        Arc::new(crate::cmd::tool_runtime::HeadlessApproval),
        interrupt.clone(),
        McpToolStatus::Ready,
    );
    let (sender, receiver) = zuno_engine::r#loop::event_channel();

    let started = std::time::Instant::now();
    let (outcome, events) = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(
            run_turn(
                RunTurnRequest::new(session.id, "turn-idle-timeout", DynamicContext::default(),),
                TurnContext::new(
                    &mut connection,
                    &providers,
                    &resolver,
                    &dispatcher,
                    &interrupt,
                ),
                sender,
            ),
            collect_turn_events(receiver)
        )
    })
    .await
    .expect("the real turn must finish inside its one-second test budget");
    let elapsed = started.elapsed();

    if finish {
        server.await.expect("progressing fixture completes");
    } else {
        server.abort();
        let _ = server.await;
    }
    (outcome, events, elapsed)
}

fn visible_text(events: &[TurnEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::Provider {
                event: StreamEvent::TextDelta(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_stalled_provider_ends_a_real_turn_with_partial_text_and_a_visible_idle_error() {
    let idle = std::time::Duration::from_millis(75);
    let (outcome, events, elapsed) = run_compatible_turn(
        vec![(std::time::Duration::ZERO, chat_delta("PARTIAL_T166"))],
        false,
        idle,
    )
    .await;

    assert_eq!(
        visible_text(&events),
        "PARTIAL_T166",
        "text emitted before the stall must remain on the user-visible event surface"
    );
    let error = outcome.expect_err("a held-open provider socket must end the turn");
    let rendered = describe_turn_failure(&error, None);
    assert!(rendered.contains("idle timeout"), "{rendered}");
    assert!(
        rendered.contains(zuno_llm::sse::STREAM_IDLE_TIMEOUT_ENV),
        "{rendered}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "the stalled turn exceeded its user-visible bound: {elapsed:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            TurnEvent::Provider {
                event: StreamEvent::RetryRollback { .. },
                ..
            }
        )),
        "the turn retried a partial response instead of failing visibly"
    );
}

#[tokio::test]
async fn a_slow_but_progressing_real_turn_outlives_one_transport_idle_window() {
    let interval = std::time::Duration::from_millis(60);
    let idle = std::time::Duration::from_millis(150);
    let (outcome, events, elapsed) = run_compatible_turn(
        vec![
            (std::time::Duration::ZERO, chat_delta("SLOW_")),
            (interval, chat_delta("BUT_")),
            (interval, chat_delta("STILL_")),
            (interval, chat_delta("MOVING")),
            (std::time::Duration::ZERO, chat_end()),
        ],
        true,
        idle,
    )
    .await;

    outcome.expect("progress inside each idle window must complete the real turn");
    assert_eq!(visible_text(&events), "SLOW_BUT_STILL_MOVING");
    assert!(
        elapsed > idle,
        "the fixture did not outlive one idle window: {elapsed:?} <= {idle:?}"
    );
}

/// Two providers, each with two models, both reachable with the credentials present.
fn two_provider_catalog() -> Catalog {
    let document: zuno_llm::catalog::models_dev::CatalogDocument = serde_json::from_str(
        r#"{"amazon-bedrock":{"id":"amazon-bedrock","name":"Bedrock","env":[],
             "npm":"@ai-sdk/amazon-bedrock",
             "models":{"claude":{"id":"claude","name":"Claude","limit":{"context":1,"output":1}},
                       "nova":{"id":"nova","name":"Nova","limit":{"context":1,"output":1}}}},
           "myopenai":{"id":"myopenai","name":"My OpenAI","env":[],
             "npm":"@ai-sdk/openai-compatible","api":"https://gateway.internal/v1",
             "models":{"gpt-5":{"id":"gpt-5","name":"GPT-5","limit":{"context":1,"output":1}},
                       "o4":{"id":"o4","name":"O4","limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config: zuno_config::schema::Config =
        serde_json::from_str(r#"{"provider":{"amazon-bedrock":{},"myopenai":{}}}"#)
            .expect("config");
    Catalog::resolve(&document, &ResolveInput::new().with_config(&config))
}

/// The owner's defect: `/model` listed one provider while `zuno models` listed ten.
///
/// The picker's data came from the resolved plan's single provider, so no second vendor's
/// model could reach the surface however the view rendered it. Asserting on *distinct
/// provider prefixes* rather than on a count is what makes this fail for that cause: a
/// list that grew but stayed inside one provider still fails here.
#[test]
fn the_picker_enumeration_spans_every_provider_the_catalog_holds() {
    let catalog = two_provider_catalog();

    let offered = picker_model_ids(&catalog);
    let providers = offered
        .iter()
        .filter_map(|qualified| qualified.split_once('/').map(|(provider, _)| provider))
        .collect::<std::collections::BTreeSet<_>>();

    assert!(
        providers.len() >= 2,
        "the picker was offered {} provider(s) from a catalog holding 2: {offered:?}",
        providers.len()
    );
    assert_eq!(
        offered,
        catalog.model_lines(),
        "the picker must enumerate through the same function `zuno models` prints from, \
         or the two surfaces can disagree again"
    );
    // Pins the defect's mechanism, not just its symptom: the session provider's own slice
    // is what used to be handed over, and it can never span two providers.
    let session_slice = catalog
        .provider("amazon-bedrock")
        .expect("the fixture resolves bedrock")
        .models
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        session_slice.len() < offered.len(),
        "the fixture cannot distinguish one provider's slice from the whole catalog"
    );
}

/// Every assertion below calls [`tool_runtime::assemble`] — the one function
/// `zuno run`, `zuno serve` and the TUI all reach — and never a registry built here.
///
/// That distinction is the whole point. `task` and `skill` were fully implemented and
/// tested in `zuno-tools` while unregistered in the production assembly, and every one
/// of those tests passed the entire time, because each built its own registry. A slot
/// missing from the composition root is only observable from the composition root.
mod production_registry {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        _goal_spill: tempfile::TempDir,
        ids: Vec<String>,
    }

    fn assemble_with(skills: zuno_catalog::skill::Skills) -> Fixture {
        let directory = tempfile::TempDir::new().expect("temporary tool workspace");
        let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
        let selected_agent = agent("build");
        let runtime = tool_runtime::assemble(
            directory.path(),
            None,
            &Env::empty(),
            &zuno_config::schema::Config::default(),
            &selected_agent,
            tool_runtime::ToolSelection {
                provider_id: "provider",
                model_id: "model",
                manifest: Arc::new(zuno_harness::ToolManifest::all()),
                contributions: Arc::new(zuno_harness::ToolContributions::default()),
                question: None,
                todo_store: Arc::new(
                    zuno_db::Pool::open(&zuno_paths::DbLocation::Memory)
                        .expect("in-memory todo store"),
                ),
                goal_store: Arc::new(
                    GoalStore::open_memory(goal_spill.path().to_owned())
                        .expect("in-memory goal store"),
                ),
                mcp_loader: None,
                skills: Arc::new(skills),
                delegation: test_delegation(),
            },
        )
        .expect("production registry assembles");
        let ids = runtime
            .tools
            .iter()
            .map(|tool| tool.id().to_owned())
            .collect();
        Fixture {
            _directory: directory,
            _goal_spill: goal_spill,
            ids,
        }
    }

    fn assemble() -> Fixture {
        assemble_with(zuno_catalog::skill::Skills::default())
    }

    #[test]
    fn advertises_the_skill_tool_so_a_skill_body_can_be_loaded_on_demand() {
        let fixture = assemble();

        assert!(
            fixture.ids.iter().any(|id| id == zuno_tools::SKILL_WIRE_ID),
            "the production registry has no `{}`, so no discovered skill can be loaded; \
             visible tools: {:?}",
            zuno_tools::SKILL_WIRE_ID,
            fixture.ids
        );
    }

    #[test]
    fn advertises_the_task_tool_so_work_can_be_delegated_to_a_subagent() {
        let fixture = assemble();

        assert!(
            fixture.ids.iter().any(|id| id == zuno_tools::TASK_WIRE_ID),
            "the production registry has no `{}`, so the model cannot delegate at all; \
             visible tools: {:?}",
            zuno_tools::TASK_WIRE_ID,
            fixture.ids
        );
    }

    /// The `skill` tool must answer from the same set the prompt advertised.
    ///
    /// One load shared by both consumers, so a name in `<available_skills>` is
    /// necessarily a name the tool can load. Two loads would let them disagree.
    #[tokio::test]
    async fn the_skill_tool_answers_from_the_very_set_the_prompt_was_built_from() {
        use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

        let skills = zuno_catalog::skill::Skills::from_loaded([zuno_catalog::skill::Skill {
            name: "wired".to_owned(),
            description: Some("proves the registry holds this exact set".to_owned()),
            location: "/skills/wired/SKILL.md".to_owned(),
            content: "the body the model must receive".to_owned(),
        }]);
        let advertised = skills.render(zuno_catalog::skill::Form::Verbose);
        let directory = tempfile::TempDir::new().expect("temporary tool workspace");
        let goal_spill = tempfile::TempDir::new().expect("temporary goal spill directory");
        let selected_agent = agent("build");
        let runtime = tool_runtime::assemble(
            directory.path(),
            None,
            &Env::empty(),
            &zuno_config::schema::Config::default(),
            &selected_agent,
            tool_runtime::ToolSelection {
                provider_id: "provider",
                model_id: "model",
                manifest: Arc::new(zuno_harness::ToolManifest::all()),
                contributions: Arc::new(zuno_harness::ToolContributions::default()),
                question: None,
                todo_store: Arc::new(
                    zuno_db::Pool::open(&zuno_paths::DbLocation::Memory)
                        .expect("in-memory todo store"),
                ),
                goal_store: Arc::new(
                    GoalStore::open_memory(goal_spill.path().to_owned())
                        .expect("in-memory goal store"),
                ),
                mcp_loader: None,
                skills: Arc::new(skills),
                delegation: test_delegation(),
            },
        )
        .expect("production registry assembles");

        assert!(advertised.contains("<name>wired</name>"));
        let tool = runtime
            .tools
            .iter()
            .find(|tool| tool.id() == zuno_tools::SKILL_WIRE_ID)
            .expect("the assembled registry advertises `skill`");
        let output = tool
            .invoke(
                serde_json::json!({"name": "wired"}),
                ToolContext::new(
                    "ses_registry",
                    "msg_registry",
                    "call_registry",
                    "build",
                    Arc::new(AllowAll),
                    Arc::new(NeverInterrupted),
                ),
            )
            .await
            .expect("the advertised skill loads through the assembled tool");

        assert_eq!(output.output, "the body the model must receive");
    }
}

/// The skill catalogue must reach the system prompt, bounded, and say when it trims.
mod skill_prompt {
    use super::*;

    fn skill(name: &str, description: &str) -> zuno_catalog::skill::Skill {
        zuno_catalog::skill::Skill {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            location: format!("/skills/{name}/SKILL.md"),
            content: "body".to_owned(),
        }
    }

    fn resolver() -> Resolver {
        Resolver {
            requested_agent: "build".to_owned(),
            system_prompt: "AGENT PROMPT".to_owned(),
            max_steps: DEFAULT_MAX_STEPS,
            requested_provider: "provider".to_owned(),
            requested_model: "model".to_owned(),
            wire_model: "model".to_owned(),
            spec: Spec::new(COMPATIBLE_PROVIDER),
            reasoning_options: serde_json::Map::new(),
        }
    }

    #[test]
    fn a_discovered_skill_reaches_the_prompt_without_displacing_the_agents_own() {
        let skills = zuno_catalog::skill::Skills::from_loaded([skill("deploy", "Ship it.")]);
        let mut resolver = resolver();
        let mut notes = Vec::new();

        announce_skills(&mut resolver, &skills, &mut notes);

        assert!(
            resolver.system_prompt.starts_with("AGENT PROMPT"),
            "the agent's own prompt must stay first: {}",
            resolver.system_prompt
        );
        assert!(resolver.system_prompt.contains("<name>deploy</name>"));
        assert!(
            resolver
                .system_prompt
                .contains("<description>Ship it.</description>")
        );
        assert!(
            resolver.system_prompt.contains("/skills/deploy/SKILL.md"),
            "the location is what leaves `read` as a fallback"
        );
        assert!(notes.is_empty(), "nothing was trimmed: {notes:?}");
    }

    #[test]
    fn an_empty_catalogue_leaves_the_prompt_byte_identical() {
        let mut resolver = resolver();
        let before = resolver.system_prompt.clone();
        let mut notes = Vec::new();

        announce_skills(
            &mut resolver,
            &zuno_catalog::skill::Skills::default(),
            &mut notes,
        );

        assert_eq!(resolver.system_prompt.as_bytes(), before.as_bytes());
        assert!(notes.is_empty());
    }

    /// A corpus past the budget is trimmed, bounded, and **reported**.
    ///
    /// The report is the point. A skill silently absent from the prompt is a skill the
    /// model will never use and the user will never learn was dropped.
    #[test]
    fn a_corpus_past_the_budget_is_trimmed_and_the_trim_is_reported() {
        let padding = "d".repeat(2_000);
        let skills = zuno_catalog::skill::Skills::from_loaded(
            (0..64).map(|at| skill(&format!("skill-{at:03}"), &padding)),
        );
        let mut resolver = resolver();
        let mut notes = Vec::new();

        announce_skills(&mut resolver, &skills, &mut notes);

        assert!(
            resolver.system_prompt.len() < "AGENT PROMPT".len() + SKILL_PROMPT_BUDGET + 1,
            "the prompt exceeded the budget: {} bytes",
            resolver.system_prompt.len()
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("did not fit"), "{}", notes[0]);
        assert!(
            notes[0].contains("were not advertised to the model"),
            "{}",
            notes[0]
        );
    }
}

/// The `AGENTS.md`-class rules must reach the system prompt, or they govern nothing.
///
/// [`zuno_config::Instructions`] was a complete, tested port with **zero** production
/// callers: a user could write `AGENTS.md` at either level, or list files in
/// `instructions`, and none of it was ever sent. These assertions run the real
/// discovery against real files, so they cover the seam and the semantics together.
mod instruction_prompt {
    use super::*;
    use std::path::Path;

    fn env_for(root: &Path) -> Env {
        Env::empty()
            .with(
                zuno_paths::env::HOME,
                root.join("home").to_string_lossy().into_owned(),
            )
            .with(
                zuno_paths::env::XDG_CONFIG_HOME,
                root.join("home/.config").to_string_lossy().into_owned(),
            )
    }

    fn write(path: &Path, body: impl AsRef<[u8]>) {
        std::fs::create_dir_all(path.parent().expect("a parent directory")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    fn options(
        root: &Path,
        directory: PathBuf,
        instructions: Vec<String>,
    ) -> zuno_config::InstructionOptions {
        zuno_config::InstructionOptions::new(
            directory,
            Some(root.join("repo")),
            &env_for(root),
            instructions,
        )
    }

    fn resolver() -> Resolver {
        Resolver {
            requested_agent: "build".to_owned(),
            system_prompt: "AGENT PROMPT".to_owned(),
            max_steps: DEFAULT_MAX_STEPS,
            requested_provider: "provider".to_owned(),
            requested_model: "model".to_owned(),
            wire_model: "model".to_owned(),
            spec: Spec::new(COMPATIBLE_PROVIDER),
            reasoning_options: serde_json::Map::new(),
        }
    }

    async fn inject(options: &zuno_config::InstructionOptions) -> (Resolver, Vec<String>) {
        let loaded = zuno_config::Instructions::discover(options).load().await;
        let mut resolver = resolver();
        let mut notes = Vec::new();
        announce_instructions(&mut resolver, &loaded, &mut notes);
        (resolver, notes)
    }

    #[tokio::test]
    async fn the_global_rule_file_reaches_the_system_prompt() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let global = root.path().join("home/.config/zuno/AGENTS.md");
        write(&global, "GLOBAL_RULE_MARKER");
        std::fs::create_dir_all(root.path().join("repo")).expect("mkdir repo");

        let (resolver, notes) =
            inject(&options(root.path(), root.path().join("repo"), Vec::new())).await;

        assert!(
            resolver.system_prompt.starts_with("AGENT PROMPT"),
            "the agent's own prompt must stay first: {}",
            resolver.system_prompt
        );
        assert!(
            resolver.system_prompt.contains("GLOBAL_RULE_MARKER"),
            "the global rule file never reached the prompt: {}",
            resolver.system_prompt
        );
        assert!(
            resolver
                .system_prompt
                .contains(&format!("Instructions from: {}", global.display())),
            "the oracle's header must name the source: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[tokio::test]
    async fn the_project_cascade_reaches_the_prompt_at_every_level() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "ROOT_RULE_MARKER");
        write(&repo.join("sub/AGENTS.md"), "SUB_RULE_MARKER");

        let (resolver, notes) = inject(&options(root.path(), repo.join("sub"), Vec::new())).await;

        let sub_at = resolver
            .system_prompt
            .find("SUB_RULE_MARKER")
            .expect("the nearest level must reach the prompt");
        let root_at = resolver
            .system_prompt
            .find("ROOT_RULE_MARKER")
            .expect("the worktree level must reach the prompt too, not only the nearest");
        assert!(
            sub_at < root_at,
            "the cascade renders deepest first: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// The cascade's sharp edge, preserved deliberately.
    ///
    /// The first *filename* found anywhere on the chain claims the whole chain, so a
    /// nearer `CLAUDE.md` loses to a further `AGENTS.md` and is not loaded at all. That
    /// is the oracle's behaviour (`instruction.ts:122-132`), it surprises people, and
    /// this exists so nobody "fixes" it into a per-level merge.
    #[tokio::test]
    async fn a_nearer_claude_md_is_not_loaded_once_a_further_agents_md_exists() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), "ROOT_RULE_MARKER");
        write(&repo.join("sub/CLAUDE.md"), "SUB_CLAUDE_MARKER");

        let (resolver, notes) = inject(&options(root.path(), repo.join("sub"), Vec::new())).await;

        assert!(
            resolver.system_prompt.contains("ROOT_RULE_MARKER"),
            "{}",
            resolver.system_prompt
        );
        assert!(
            !resolver.system_prompt.contains("SUB_CLAUDE_MARKER"),
            "`AGENTS.md` anywhere on the chain claims it, so this `CLAUDE.md` must not be \
             loaded: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[tokio::test]
    async fn configured_instruction_entries_reach_the_prompt() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("docs/house-style.md"), "CONFIGURED_RULE_MARKER");
        write(
            &root.path().join("home/tilde-rules.md"),
            "TILDE_RULE_MARKER",
        );

        let (resolver, notes) = inject(&options(
            root.path(),
            repo,
            vec!["docs/*.md".to_owned(), "~/tilde-rules.md".to_owned()],
        ))
        .await;

        assert!(
            resolver.system_prompt.contains("CONFIGURED_RULE_MARKER"),
            "an `instructions` glob never reached the prompt: {}",
            resolver.system_prompt
        );
        assert!(
            resolver.system_prompt.contains("TILDE_RULE_MARKER"),
            "a `~/`-relative `instructions` entry never reached the prompt: {}",
            resolver.system_prompt
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    /// The common case — no rule file anywhere — must cost nothing and say nothing.
    ///
    /// Byte equality, not "contains": a stray `\n\n` for an absent file would ride in
    /// front of every request for the life of the session and invalidate a prompt cache
    /// that had no reason to move.
    #[tokio::test]
    async fn a_project_with_no_rule_file_leaves_the_prompt_byte_identical() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let (resolver, notes) = inject(&options(root.path(), repo, Vec::new())).await;

        assert_eq!(
            resolver.system_prompt.as_bytes(),
            b"AGENT PROMPT",
            "an absent instruction file must add no bytes at all"
        );
        assert!(
            notes.is_empty(),
            "a missing rule file is the normal case and must be silent: {notes:?}"
        );
    }

    /// An unreadable file is reported, once, and never silently skipped.
    ///
    /// A rule the user wrote and believes is in force, that the agent never received,
    /// is the worst of the three outcomes — worse than a hard failure, which they would
    /// at least notice. The count matters as much as the text: this is surfaced from a
    /// load that happens once per host, not once per turn.
    #[tokio::test]
    async fn an_unreadable_rule_file_is_reported_exactly_once() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        write(&repo.join("AGENTS.md"), [0xff_u8, 0xfe, 0x00, 0x9c]);

        let (resolver, notes) = inject(&options(root.path(), repo.clone(), Vec::new())).await;

        assert_eq!(
            resolver.system_prompt.as_bytes(),
            b"AGENT PROMPT",
            "an unreadable file must not contribute bytes"
        );
        assert_eq!(
            notes.len(),
            1,
            "an unreadable rule file must be reported once — no more, and never zero: \
             {notes:?}"
        );
        assert!(
            notes[0].contains(&repo.join("AGENTS.md").display().to_string()),
            "the report must name the file the user has to fix: {}",
            notes[0]
        );
        assert!(
            notes[0].contains("could not be read"),
            "the report must say what went wrong: {}",
            notes[0]
        );
    }

    /// Past the budget a whole file is dropped and named — never cut mid-rule.
    #[tokio::test]
    async fn an_oversized_rule_file_is_dropped_whole_and_the_drop_is_reported() {
        let root = tempfile::TempDir::new().expect("temporary instruction root");
        let repo = root.path().join("repo");
        let oversized = repo.join("AGENTS.md");
        write(
            &oversized,
            format!(
                "OVERSIZED_RULE_MARKER{}",
                "r".repeat(INSTRUCTION_PROMPT_BUDGET)
            ),
        );
        write(&root.path().join("home/small.md"), "SMALL_RULE_MARKER");

        let (resolver, notes) =
            inject(&options(root.path(), repo, vec!["~/small.md".to_owned()])).await;

        assert!(
            !resolver.system_prompt.contains("OVERSIZED_RULE_MARKER"),
            "a file past the budget must be dropped whole, not truncated into a rule \
             that says something else"
        );
        assert!(
            resolver.system_prompt.len() <= "AGENT PROMPT".len() + 2 + INSTRUCTION_PROMPT_BUDGET,
            "the prompt exceeded the budget: {} bytes",
            resolver.system_prompt.len()
        );
        assert!(
            resolver.system_prompt.contains("SMALL_RULE_MARKER"),
            "one oversized file must not starve the rest: {}",
            resolver.system_prompt
        );
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(&oversized.display().to_string()),
            "the report must name the file: {}",
            notes[0]
        );
        assert!(
            notes[0].contains("none of its rules are in force"),
            "the report must say the rules are not in effect, not merely that bytes were \
             trimmed: {}",
            notes[0]
        );
        assert!(
            !notes[0].contains("OVERSIZED_RULE_MARKER"),
            "instruction contents are user-authored and must never be echoed: {}",
            notes[0]
        );
    }
}

/// Instruction files must be injected once, and between memory and the skills.
///
/// Two independent failures this pins. **Absence**: the injection deleted compiles,
/// lints and passes every behavioural test above, because those call
/// [`announce_instructions`] directly rather than through the composition root — the
/// exact shape of the original defect, where the whole module had no caller.
/// **Order**: the oracle assembles `[...environment, ...instructions, ...skills]`
/// (`session/prompt.ts:1257-1269`), and moving this call past `announce_skills` would
/// silently invert precedence between a user's rule and a skill's description.
#[test]
fn instruction_files_are_injected_once_between_memory_and_the_skill_catalogue() {
    let turn = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/turn.rs"),
    )
    .expect("turn.rs is readable");

    let memory_at = turn
        .find("configure_resident_memory(\n            &mut plan.resolver,")
        .expect("the resident-memory call site moved; this test's anchors need updating");
    let instructions_at = turn
        .find("announce_instructions(&mut plan.resolver")
        .expect(
            "`turn.rs` no longer injects instruction files, so a user's `AGENTS.md` reaches \
             no request and nothing reports it",
        );
    let skills_at = turn
        .find("announce_skills(&mut plan.resolver")
        .expect("the skill-catalogue call site moved; this test's anchors need updating");

    assert!(
        memory_at < instructions_at && instructions_at < skills_at,
        "instruction files must be assembled after memory and before the skill \
         catalogue, mirroring the oracle's segment order"
    );
    assert_eq!(
        turn.matches("announce_instructions(&mut plan.resolver")
            .count(),
        1,
        "instruction files must be injected at exactly one site: a second call would \
         charge the user for every rule file twice on every request"
    );
    assert!(
        turn.contains("instructions,\n            delegation_facts,"),
        "`TurnPlan` no longer carries the loaded instruction files, so the read would \
         have to repeat per turn"
    );
}

/// The three wirings this task closed, asserted at their production call sites.
///
/// # Why a source scan and not a behavioural assertion
///
/// The same reason [`only_this_module_composes_a_turn`] is one, and the defect class
/// is identical: each of these was **absent**, and absence produced no error, no
/// warning, and no failing test — the model was simply told less than the build could
/// do. Reaching these through behaviour needs a resolved catalog, a credential and a
/// live provider, which is why nothing covered them for as long as it did.
///
/// A scan is crude and it is also the only check that fails the moment someone deletes
/// one of these lines, because deleting any of them compiles, passes clippy, and
/// passes every other test in this workspace.
#[test]
fn the_headless_surfaces_wire_every_capability_the_tui_has() {
    let cmd = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    let read = |name: &str| {
        std::fs::read_to_string(cmd.join(name)).unwrap_or_else(|_| panic!("{name} is readable"))
    };

    let turn = read("turn.rs");
    assert!(
        turn.contains("announce_skills(&mut plan.resolver"),
        "`turn.rs` no longer injects the skill catalogue into the system prompt, so \
         discovery runs and the model is told about none of it"
    );
    assert!(
        turn.contains("skills: Arc::clone(&plan.skills)"),
        "`turn.rs` no longer hands the loaded skills to the tool assembly, so the \
         `skill` tool would answer from a different set than the prompt advertised"
    );
    assert!(
        turn.contains("delegation: super::tool_runtime::Delegation {"),
        "`turn.rs` no longer supplies a delegation host; `task` cannot be registered"
    );

    for surface in ["run.rs", "serve.rs"] {
        let source = read(surface);
        assert!(
            source.contains("mcp_runtime::McpRuntime::from_config"),
            "`{surface}` no longer builds an MCP runtime, so the same configuration \
             that gives the TUI its MCP tools gives this surface none"
        );
        assert!(
            source.contains("mcp.shutdown()"),
            "`{surface}` no longer closes its MCP transports, leaving a remote \
             server's session open on the far side"
        );
    }
    assert!(
        read("run.rs").contains("TurnHost::open_with_mcp"),
        "`zuno run` must reach the constructor that takes a catalog"
    );
    assert!(
        read("serve.rs").contains("TurnHost::open_with_runtime_and_mcp"),
        "`zuno serve` must reach the constructor that takes a catalog"
    );
}

/// A placeholder title is not a name, so no surface should be handed one.
///
/// `session.title` is `NOT NULL` and `create` fills it with `New session - <instant>`, so
/// the raw column is never empty and a surface reading it directly would print that string
/// as though the user had chosen it — then replace it a second later with the generated
/// name. The filter is what makes "unnamed" expressible, and it reuses the generator's own
/// predicate so the two cannot disagree about which titles are real.
#[test]
fn turn_a_placeholder_session_title_reads_as_no_title_at_all() {
    for placeholder in [
        format!(
            "{}2026-08-07T00:00:00.000Z",
            zuno_db::session::PARENT_TITLE_PREFIX
        ),
        format!(
            "{}2026-08-07T00:00:00.000Z",
            zuno_db::session::CHILD_TITLE_PREFIX
        ),
        String::new(),
    ] {
        assert!(
            zuno_db::session::is_default_title(&placeholder),
            "`{placeholder}` must be recognised as a placeholder, or the sidebar will \
             display it as a chosen name"
        );
    }

    assert!(
        !zuno_db::session::is_default_title("Refactoring user service"),
        "a generated name was mistaken for a placeholder, so a named session would render \
         as unnamed"
    );
}

// ---------------------------------------------------------------------------
// Generation controls: the catalog's output limit and the agent's sampling
//
// # How these differ from the tests that let the effort defect through
//
// Those tests hand-wrote the body key they then asserted (`reasoning_effort`), so
// they proved `extraBody` is an identity function and said nothing about what a
// session emits — production spelled the key `reasoningEffort` and the two never
// met. Every test below writes only **user-facing configuration** — a provider
// block, a model's `limit.output`, an `agent` entry — and asserts on the **body a
// real provider builds**. No test here names an intermediate value, so none can be
// satisfied by a fixture that production never produces.
// ---------------------------------------------------------------------------

/// A resolved catalog model from a user-shaped config, plus the catalog itself.
///
/// The `limit.output` and `agent` blocks are the only inputs, because they are the
/// only things a user writes.
fn generation_catalog(
    model_id: &str,
    output_limit: Option<u64>,
    provider_options: serde_json::Value,
) -> Catalog {
    let mut model = serde_json::Map::from_iter([
        ("id".to_owned(), serde_json::json!(model_id)),
        ("name".to_owned(), serde_json::json!("Generation fixture")),
        // Declared for every fixture model, not just the reasoning ones: a model
        // whose catalog entry omits it resolves to no reasoning controls whatever
        // level is chosen, which would make a variant assertion pass for the wrong
        // reason.
        ("reasoning".to_owned(), serde_json::json!(true)),
        (
            "variants".to_owned(),
            serde_json::json!({
                "high": {"reasoningEffort": "high"},
                "low": {"reasoningEffort": "low"}
            }),
        ),
    ]);
    if let Some(output) = output_limit {
        model.insert(
            "limit".to_owned(),
            serde_json::json!({"context": 100_000, "output": output}),
        );
    }
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {
            "stub": {
                "id": "stub",
                "name": "Generation fixture",
                "env": [],
                "npm": "@ai-sdk/openai-compatible",
                "options": provider_options,
                "models": { model_id: serde_json::Value::Object(model) },
            }
        }
    }))
    .expect("generation fixture config");
    Catalog::resolve(
        &zuno_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    )
}

/// One agent from user-shaped config, through the real deserializer and merge.
///
/// Deliberately not [`agent`]: that helper constructs the struct field by field,
/// which would let a test pass while the config schema dropped the key on the way
/// in. Going through `AgentConfig` means the JSON a user writes is the input.
fn configured_agent(definition: serde_json::Value) -> zuno_catalog::agent::Agent {
    let config: zuno_config::schema::Config = serde_json::from_value(serde_json::json!({
        "agent": { "tuned": definition }
    }))
    .expect("agent fixture config");
    zuno_catalog::agent::resolve(
        config.agent.as_ref().expect("the agent map deserializes"),
        &[],
    )
    .into_iter()
    .find(|entry| entry.name == "tuned")
    .expect("the configured agent resolves")
}

/// The Chat body a real provider sends for `model_id`, resolved as production does.
fn generation_body(
    catalog: &Catalog,
    model_id: &str,
    agent: &zuno_catalog::agent::Agent,
) -> serde_json::Value {
    let model = catalog
        .model("stub", model_id)
        .expect("the generation fixture model resolves");
    let spec = with_agent_options(
        model_spec(catalog, model, &Env::empty()).expect("the generation fixture spec resolves"),
        agent,
    );
    let provider = zuno_provider_compatible::CompatibleProvider::new(
        spec,
        Arc::new(zuno_provider_compatible::ReqwestTransport::new(
            "generation",
        )),
        None,
    )
    .expect("the generation fixture provider builds");
    let mut request = zuno_llm::registry::CompletionRequest::new(
        model.api.id.clone(),
        vec![zuno_llm::event::Message {
            role: zuno_llm::event::Role::User,
            content: vec![zuno_llm::event::RequestContentBlock::Text {
                text: "Say hello.".to_owned(),
            }],
        }],
    )
    .with_tools(vec![zuno_llm::registry::ToolSchema {
        name: "read".to_owned(),
        description: "Read a file".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }]);
    request.parameters =
        session_reasoning_options(turn_effort(None, agent, "stub", model_id), "stub", model);
    provider.body_for(&request)
}

#[test]
fn a_models_declared_output_limit_reaches_the_request_body() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "capped", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(16_384)),
        "the catalog's `limit.output` never reached the body, so every request runs on \
         the vendor's own default: measured against a capture stub, upstream sends \
         `max_tokens` where this build sent nothing"
    );
}

#[test]
fn an_output_limit_above_the_ceiling_is_clamped_rather_than_forwarded() {
    let catalog = generation_catalog(
        "huge",
        Some(1_000_000),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "huge", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(32_000)),
        "a catalog row claiming a million output tokens was forwarded verbatim; \
         `ProviderTransform.maxOutputTokens` clamps to 32_000"
    );
}

#[test]
fn a_model_declaring_no_output_limit_still_sends_a_cap() {
    let catalog = generation_catalog(
        "uncapped",
        None,
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "uncapped", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(32_000)),
        "an absent `limit.output` deserialises to 0, and `max_tokens: 0` asks the model \
         for an empty completion"
    );
}

#[test]
fn a_configured_output_limit_outranks_the_catalogs_own() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1",
            "maxTokens": 2_048
        }),
    );
    let body = generation_body(&catalog, "capped", &agent("build"));

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(2_048)),
        "the catalog default overwrote an explicitly configured cap, so a user lowering \
         `maxTokens` to control cost had no way to do it"
    );
}

#[test]
fn an_agents_sampling_declarations_reach_the_request_body() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let tuned = configured_agent(serde_json::json!({
        "model": "stub/capped",
        "temperature": 0.21,
        "top_p": 0.87
    }));
    let body = generation_body(&catalog, "capped", &tuned);

    assert_eq!(
        body.get("temperature"),
        Some(&serde_json::json!(0.21)),
        "`agent.tuned.temperature` was parsed, merged, and listed, and then the request \
         went out on the provider's default"
    );
    assert_eq!(
        body.get("top_p"),
        Some(&serde_json::json!(0.87)),
        "`top_p` is the config spelling and `topP` the option spelling; a request \
         missing the field means the rename was dropped rather than applied"
    );
}

#[test]
fn an_agents_option_bag_reaches_the_provider_and_can_override_the_cap() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let tuned = configured_agent(serde_json::json!({
        "model": "stub/capped",
        "options": {
            "maxTokens": 4_096,
            "toolChoice": "required",
            "extraBody": {"service_tier": "flex"}
        }
    }));
    let body = generation_body(&catalog, "capped", &tuned);

    assert_eq!(
        body.get("max_tokens"),
        Some(&serde_json::json!(4_096)),
        "`agent.tuned.options` never reached the provider, so an agent could not raise \
         or lower the cap the catalog set"
    );
    assert_eq!(
        body.get("tool_choice"),
        Some(&serde_json::json!("required")),
        "a configured `toolChoice` was accepted and dropped, leaving the model free to \
         answer without calling the tool the agent required"
    );
    assert_eq!(
        body.get("service_tier"),
        Some(&serde_json::json!("flex")),
        "`extraBody` inside an agent's options is the documented channel for a \
         provider-specific body key, and it did not arrive"
    );
}

#[test]
fn no_tool_choice_is_sent_when_none_was_configured() {
    let catalog = generation_catalog(
        "capped",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let body = generation_body(&catalog, "capped", &agent("build"));

    assert_eq!(
        body.get("tool_choice"),
        None,
        "`auto` is what OpenAI documents as the default when tools are present, so \
         sending it unprompted changes the bytes without changing the behaviour — and \
         asks it of endpoints that reject a value they do not implement"
    );
}

#[test]
fn an_agents_variant_selects_the_models_declared_reasoning_options() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let reasoner = configured_agent(serde_json::json!({
        "model": "stub/reasoner",
        "variant": "high"
    }));
    let body = generation_body(&catalog, "reasoner", &reasoner);

    assert_eq!(
        body.get("reasoning_effort"),
        Some(&serde_json::json!("high")),
        "`agent.reasoner.variant` was accepted and never resolved, so an agent \
         configured to think hard reasoned at the provider's default"
    );
}

#[test]
fn a_variant_is_ignored_on_a_model_the_agent_did_not_declare() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let elsewhere = configured_agent(serde_json::json!({
        "model": "stub/other-model",
        "variant": "high"
    }));
    let body = generation_body(&catalog, "reasoner", &elsewhere);

    assert_eq!(
        body.get("reasoning_effort"),
        None,
        "a variant names a level the agent's OWN model declares; carried onto a model \
         switched to by hand it selects a level that name does not mean on this model"
    );
}

#[test]
fn a_session_chosen_effort_outranks_the_agents_variant() {
    let catalog = generation_catalog(
        "reasoner",
        Some(16_384),
        serde_json::json!({
            "baseURL": "https://example.invalid/v1"
        }),
    );
    let reasoner = configured_agent(serde_json::json!({
        "model": "stub/reasoner",
        "variant": "high"
    }));

    assert_eq!(
        turn_effort(
            Some(zuno_llm::effort::ReasoningEffort::Low),
            &reasoner,
            "stub",
            "reasoner"
        ),
        Some(zuno_llm::effort::ReasoningEffort::Low),
        "the effort picker is a live user action and the agent's variant a configured \
         default, so the picker must win"
    );
    let _ = catalog;
}

#[test]
fn the_generation_controls_are_wired_into_the_turns_own_resolution() {
    let turn = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd/turn.rs"),
    )
    .expect("turn.rs is readable");

    assert!(
        turn.contains("spec: with_agent_options(model_spec("),
        "`TurnPlan::resolve` no longer overlays the agent's options onto the resolved \
         spec, so `temperature`, `top_p` and `options` are parsed, listed, and dropped \
         — the defect this pair of tests exists to catch. A behavioural test alone \
         cannot see it, because it calls the helper the turn stopped calling."
    );
    assert!(
        turn.contains("turn_effort(options.effort, &agent,"),
        "`TurnPlan::resolve` no longer consults the agent's `variant`, so an agent \
         configured to reason at one level runs at the provider's default"
    );
    assert!(
        turn.contains("generation::MAX_TOKENS, json!(output_ceiling(model))"),
        "`model_spec` no longer defaults the output cap from the catalog, so every \
         request runs uncapped"
    );
}
