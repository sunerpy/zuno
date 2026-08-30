//! The two rule-driven providers, and the refusal.
//!
//! Every case here is asserted as the **endpoint that goes on the wire**, not as an
//! intermediate enum: the Azure walk and the Copilot per-model check are only
//! observable to a user as a URL, so that is what the table pins.

use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;
use zuno_error::{ProviderError, Recovery};
use zuno_llm::event::{Message, RequestContentBlock, Role};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Declined, Provider, ProviderRegistry, Spec,
};
use zuno_provider_compatible::family::{self, Family};
use zuno_provider_compatible::provider::{EXTRA_BODY_OPTION, MODEL_CAPABILITIES_OPTION};
use zuno_provider_compatible::quirks::REASONING_CONTENT_MODELS_OPTION;
use zuno_provider_compatible::surface::{
    MODEL_ENDPOINTS_OPTION, SURFACES_OPTION, USE_COMPLETION_URLS_OPTION,
};
use zuno_provider_compatible::{ChunkStream, CompatibleProvider, HttpRequest, Transport};

/// A transport that fails the test if it is ever used.
///
/// Every assertion in this file is about the request this profile *would* send, so
/// nothing here opens a stream — and a regression that started making a call would
/// panic rather than reach the network.
#[derive(Debug)]
struct NeverSends;

impl Transport for NeverSends {
    fn send(
        &self,
        _request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChunkStream, ProviderError>> + Send + '_>> {
        unreachable!("these tests assert on the built request and never send it")
    }
}

fn provider(spec: Spec) -> CompatibleProvider {
    CompatibleProvider::new(spec, Arc::new(NeverSends), Some("token".to_owned()))
        .expect("a claimed provider id with a base URL")
}

const AZURE_BASE: &str = "https://my-resource.openai.azure.com/openai/v1";
const COPILOT_BASE: &str = "https://api.githubcopilot.com";

fn resource_link_prompt() -> Vec<Message> {
    vec![Message::from_content(
        Role::User,
        vec![
            RequestContentBlock::ResourceLink {
                name: "faq.md".to_owned(),
                uri: "file:///workspace/docs/faq.md".to_owned(),
                title: None,
                description: None,
                media_type: Some("text/markdown".to_owned()),
                size: Some(512),
            },
            RequestContentBlock::Text {
                text: "简单优化下".to_owned(),
            },
        ],
    )]
}

#[test]
fn responses_preserve_multiple_text_blocks_by_default() {
    let built = provider(
        Spec::new("openrouter")
            .with_base_url("https://example.invalid/v1")
            .with_surface(ApiSurface::Responses),
    );
    let body = built.body_for(
        &CompletionRequest::new("gpt-5.6-sol", resource_link_prompt())
            .on_surface(ApiSurface::Responses),
    );

    assert_eq!(
        body["input"][0]["content"],
        json!([
            {
                "type": "input_text",
                "text": "Referenced resource `faq.md`: file:///workspace/docs/faq.md\nMedia type: text/markdown\nSize: 512 bytes"
            },
            {"type": "input_text", "text": "简单优化下"}
        ]),
        "standards-compatible Responses endpoints retain the typed block boundary"
    );
}

#[test]
fn a_declared_single_text_responses_endpoint_coalesces_projected_text() {
    let built = provider(
        Spec::new("openrouter")
            .with_base_url("https://example.invalid/v1")
            .with_surface(ApiSurface::Responses)
            .with_option("responsesTextBlocks", json!("single")),
    );
    let body = built.body_for(
        &CompletionRequest::new("gpt-5.6-sol", resource_link_prompt())
            .on_surface(ApiSurface::Responses),
    );

    assert_eq!(
        body["input"][0]["content"],
        json!([{
            "type": "input_text",
            "text": "Referenced resource `faq.md`: file:///workspace/docs/faq.md\nMedia type: text/markdown\nSize: 512 bytes\n\n简单优化下"
        }]),
        "a provider declaring one text field must receive one stable input_text block"
    );
}

// ---------------------------------------------------------------------------
// Azure — `packages/opencode/src/provider/provider.ts:154-160`
// ---------------------------------------------------------------------------

/// The full walk, as a table.
///
/// The rows are the oracle's four branches plus its fallback. Both `azure` and
/// `azure-cognitive-services` are covered because `:265` and `:285` call the same
/// selector.
#[test]
fn azure_selects_its_endpoint_by_the_documented_walk() {
    struct Row {
        surfaces: serde_json::Value,
        use_completion_urls: bool,
        expected: &'static str,
        because: &'static str,
    }

    let rows = [
        Row {
            surfaces: json!(["chat", "responses"]),
            use_completion_urls: false,
            expected: "/responses",
            because: "branch 2: no useChat, responses exists",
        },
        Row {
            surfaces: json!(["chat", "responses"]),
            use_completion_urls: true,
            expected: "/chat/completions",
            because: "branch 1: useChat and chat exists",
        },
        Row {
            surfaces: json!(["responses"]),
            use_completion_urls: true,
            expected: "/responses",
            because: "branch 1 needs chat; without it the walk falls to branch 2",
        },
        Row {
            surfaces: json!(["messages"]),
            use_completion_urls: false,
            expected: "/messages",
            because: "branch 3: only messages exists",
        },
        Row {
            surfaces: json!(["chat"]),
            use_completion_urls: false,
            expected: "/chat/completions",
            because: "branch 4: chat is the last named surface",
        },
        Row {
            surfaces: json!([]),
            use_completion_urls: false,
            expected: "/chat/completions",
            because: "fallback: languageModel, which is chat-completions here",
        },
    ];

    for id in ["azure", "azure-cognitive-services"] {
        for row in &rows {
            let spec = Spec::new(id)
                .with_base_url(AZURE_BASE)
                .with_option(SURFACES_OPTION, row.surfaces.clone())
                .with_option(USE_COMPLETION_URLS_OPTION, json!(row.use_completion_urls));
            let built = provider(spec);
            assert_eq!(
                built.endpoint("gpt-4o", ApiSurface::Default),
                format!("{AZURE_BASE}{}", row.expected),
                "{id}: {}",
                row.because
            );
        }
    }
}

/// Azure's rule does not depend on the model id.
///
/// The oracle's selector accepts `modelID` and never reads it. Asserting the
/// absence of a dependency is what stops a later change from inventing one.
#[test]
fn azure_picks_the_same_endpoint_for_every_model_id() {
    let spec = Spec::new("azure").with_base_url(AZURE_BASE);
    let built = provider(spec);
    let expected = format!("{AZURE_BASE}/responses");
    for model in [
        "gpt-4o",
        "gpt-5",
        "gpt-5-mini",
        "o3",
        "openai.gpt-oss-safeguard-20b",
        "",
    ] {
        assert_eq!(
            built.endpoint(model, ApiSurface::Default),
            expected,
            "azure must not become a model-id rule (model `{model}`)"
        );
    }
}

// ---------------------------------------------------------------------------
// GitHub Copilot — `packages/opencode/src/provider/provider.ts:225-239`
// ---------------------------------------------------------------------------

/// The `gpt-N` version check, per model id.
#[test]
fn copilot_selects_its_endpoint_per_model_id() {
    let built = provider(Spec::new("github-copilot").with_base_url(COPILOT_BASE));

    let responses = [
        ("gpt-5", "N >= 5"),
        ("gpt-5.5", "N >= 5 with a minor version"),
        ("gpt-6", "N >= 5"),
        ("gpt-41", "the regex captures all digits, so 41 >= 5"),
    ];
    for (model, because) in responses {
        assert_eq!(
            built.endpoint(model, ApiSurface::Default),
            format!("{COPILOT_BASE}/responses"),
            "{model}: {because}"
        );
    }

    let chat = [
        ("gpt-5-mini", "explicitly excluded by the oracle"),
        ("gpt-5-mini-2025-08-07", "startsWith(\"gpt-5-mini\")"),
        ("gpt-4o", "N < 5"),
        ("gpt-4.1", "N < 5"),
        (
            "gpt-oss-20b",
            "no digit follows `gpt-`, so the regex misses",
        ),
        ("claude-sonnet-4.5", "not a gpt model at all"),
        ("o3-mini", "not a gpt model at all"),
        ("gemini-2.5-pro", "not a gpt model at all"),
    ];
    for (model, because) in chat {
        assert_eq!(
            built.endpoint(model, ApiSurface::Default),
            format!("{COPILOT_BASE}/chat/completions"),
            "{model}: {because}"
        );
    }
}

/// A model's own declared endpoint is consulted before the version check.
#[test]
fn a_copilot_model_declaring_its_endpoint_overrides_the_version_check() {
    let built = provider(
        Spec::new("github-copilot")
            .with_base_url(COPILOT_BASE)
            .with_option(
                MODEL_ENDPOINTS_OPTION,
                json!({"gpt-5": "chat", "gpt-4o": "responses"}),
            ),
    );
    assert_eq!(
        built.endpoint("gpt-5", ApiSurface::Default),
        format!("{COPILOT_BASE}/chat/completions"),
        "a declared `chat` endpoint wins over `N >= 5`"
    );
    assert_eq!(
        built.endpoint("gpt-4o", ApiSurface::Default),
        format!("{COPILOT_BASE}/responses"),
        "a declared `responses` endpoint wins over `N < 5`"
    );
}

/// A request that pins a surface is honoured over either rule.
#[test]
fn a_request_pinned_surface_overrides_both_rules() {
    for id in ["azure", "github-copilot"] {
        let built = provider(Spec::new(id).with_base_url("https://endpoint.test/v1"));
        assert_eq!(
            built.endpoint("gpt-5", ApiSurface::Chat),
            "https://endpoint.test/v1/chat/completions"
        );
        assert_eq!(
            built.endpoint("gpt-4o", ApiSurface::Responses),
            "https://endpoint.test/v1/responses"
        );
    }
}

// ---------------------------------------------------------------------------
// Refusal
// ---------------------------------------------------------------------------

/// `amazon-bedrock` configured against this profile is refused, and the message
/// points at the crate that carries it.
#[test]
fn amazon_bedrock_is_refused_with_a_pointer_at_the_bedrock_crate() {
    let error = family::resolve(&Spec::new("amazon-bedrock"))
        .expect_err("bedrock is not OpenAI-compatible on the wire");

    assert_eq!(error.carried_by, Some(Family::Bedrock));
    let rendered = error.to_string();
    assert!(rendered.contains("unsupported"), "{rendered}");
    assert!(rendered.contains("zuno-provider-bedrock"), "{rendered}");
    assert!(rendered.contains("EventStream"), "{rendered}");

    // The same refusal reaches a caller through the registry, as a terminal error.
    let mut registry = ProviderRegistry::new();
    registry.register_fallible("amazon-bedrock", |spec| {
        let credential = |_: &str| Some("token".to_owned());
        zuno_provider_compatible::factory(Arc::new(NeverSends), credential)(spec)
    });
    let failure = registry
        .resolve(Spec::new("amazon-bedrock").with_base_url("https://bedrock.test"))
        .expect_err("must not construct");
    assert!(
        !failure.is_wiring_bug(),
        "the provider IS wired here; the id is simply the wrong family"
    );
    let chained = render_chain(&failure);
    assert!(chained.contains("zuno-provider-bedrock"), "{chained}");
    assert_eq!(
        ProviderError::from(failure).recovery(),
        Recovery::Fail,
        "a misrouted provider must never be retried"
    );
}

/// Every family that is not this one is refused, and names its own crate.
#[test]
fn each_foreign_family_is_refused_and_names_its_own_crate() {
    let cases = [
        ("anthropic", Family::Anthropic, "zuno-provider-anthropic"),
        ("amazon-bedrock", Family::Bedrock, "zuno-provider-bedrock"),
        ("google", Family::Google, "zuno-provider-google"),
        ("google-vertex", Family::Google, "zuno-provider-google"),
        (
            "google-vertex-anthropic",
            Family::Google,
            "zuno-provider-google",
        ),
        ("openai", Family::OpenAi, "zuno-provider-openai"),
    ];
    for (id, family, crate_name) in cases {
        let error = family::resolve(&Spec::new(id)).expect_err("wrong family");
        assert_eq!(error.carried_by, Some(family), "{id}");
        let rendered = error.to_string();
        assert!(rendered.contains("unsupported"), "{id}: {rendered}");
        assert!(rendered.contains(crate_name), "{id}: {rendered}");
    }
}

/// An unknown id is reported as unsupported rather than attempted.
#[test]
fn an_unknown_provider_id_is_never_silently_attempted() {
    let declined = CompatibleProvider::new(
        Spec::new("some-vendor-nobody-wired").with_base_url("https://endpoint.test/v1"),
        Arc::new(NeverSends),
        None,
    )
    .expect_err("an unknown id must not be routed here by default");

    let Declined::Failed(error) = declined else {
        panic!("an unknown id is a misconfiguration, not an availability state");
    };
    let chained = render_chain(&error);
    assert!(chained.contains("unsupported"), "{chained}");
    assert!(
        chained.contains(r#"transport = "openai-compatible""#),
        "the message must say how to opt in: {chained}"
    );
    assert_eq!(error.recovery(), Recovery::Fail);
}

// ---------------------------------------------------------------------------
// The reasoning-content echo, end to end through the provider
// ---------------------------------------------------------------------------

/// A model that requires the protocol gets its reasoning echoed and `thinking`
/// forced; one that does not gets neither.
#[test]
fn reasoning_content_is_echoed_only_for_a_model_that_requires_it() {
    let built = provider(Spec::new("deepseek").with_base_url("https://api.deepseek.com/v1"));
    let history = vec![
        Message::new(Role::User, "will it rain in Paris?"),
        Message::from_content(
            Role::Assistant,
            vec![
                RequestContentBlock::SignedThinking {
                    thinking: "check the forecast".to_owned(),
                    signature: String::new(),
                },
                RequestContentBlock::Text {
                    text: "No.".to_owned(),
                },
            ],
        ),
        Message::new(Role::User, "and tomorrow?"),
    ];

    let required = built.body_for(&CompletionRequest::new(
        "deepseek-v4-flash",
        history.clone(),
    ));
    assert_eq!(
        required["messages"][1]["reasoning_content"],
        json!("check the forecast")
    );
    assert_eq!(required["thinking"], json!({"type": "enabled"}));

    let not_required = built.body_for(&CompletionRequest::new("deepseek-chat", history.clone()));
    assert!(
        not_required["messages"][1]
            .get("reasoning_content")
            .is_none()
    );
    assert!(not_required.get("thinking").is_none());
}

/// The forced `thinking` opt-in is written after `extra_body`, so a stale option
/// cannot disable it.
#[test]
fn extra_body_cannot_disable_the_forced_thinking_opt_in() {
    let built = provider(
        Spec::new("deepseek")
            .with_base_url("https://api.deepseek.com/v1")
            .with_option(EXTRA_BODY_OPTION, json!({"thinking": {"type": "disabled"}})),
    );
    let body = built.body_for(&CompletionRequest::new(
        "deepseek-v4-pro",
        vec![Message::new(Role::User, "hi")],
    ));
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
}

/// Config can extend the protocol table to a model that ships after this release.
#[test]
fn config_can_declare_a_new_model_as_requiring_the_protocol() {
    let built = provider(
        Spec::new("openrouter")
            .with_base_url("https://openrouter.ai/api/v1")
            .with_option(REASONING_CONTENT_MODELS_OPTION, json!(["glm-5"])),
    );
    let history = vec![Message::from_content(
        Role::Assistant,
        vec![RequestContentBlock::SignedThinking {
            thinking: "reasoning".to_owned(),
            signature: String::new(),
        }],
    )];
    let body = built.body_for(&CompletionRequest::new("z-ai/glm-5-air", history));
    assert_eq!(body["messages"][0]["reasoning_content"], json!("reasoning"));
}

/// Sampling parameters are stripped by capability, for whatever model the catalog
/// marked, without this crate knowing any model's name.
#[test]
fn sampling_params_are_stripped_by_declared_capability() {
    let built = provider(
        Spec::new("groq")
            .with_base_url("https://api.groq.com/openai/v1")
            .with_option(
                MODEL_CAPABILITIES_OPTION,
                json!({"a-reasoning-model": {"sampling_params": false}}),
            ),
    )
    .with_sampling(zuno_provider_compatible::Sampling {
        temperature: Some(0.7),
        top_p: Some(0.95),
        ..zuno_provider_compatible::Sampling::default()
    });

    let permitted = built.body_for(&CompletionRequest::new(
        "llama-3.3-70b-versatile",
        vec![Message::new(Role::User, "hi")],
    ));
    assert_eq!(permitted["temperature"], json!(0.7));
    assert_eq!(permitted["top_p"], json!(0.95));

    let stripped = built.body_for(&CompletionRequest::new(
        "a-reasoning-model",
        vec![Message::new(Role::User, "hi")],
    ));
    assert!(stripped.get("temperature").is_none());
    assert!(stripped.get("top_p").is_none());

    assert_eq!(
        built.capabilities(),
        Capabilities {
            reasoning: true,
            tool_calls: true,
            prompt_cache: false,
            attachments: false,
            sampling_params: true,
        },
        "the provider-level default stays permissive; only the model narrows"
    );
}

/// Render an error together with its whole source chain.
fn render_chain(error: &dyn std::error::Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        rendered.push_str(": ");
        rendered.push_str(&next.to_string());
        source = next.source();
    }
    rendered
}

// ---------------------------------------------------------------------------
// Generation controls read from the option bag
//
// Every case builds its provider from a `Spec` option bag — the only thing the
// composition root writes — and asserts on `body_for`, the same value
// `http_request` posts. Setting `RequestBody` fields directly would prove the
// builder writes what it is told and say nothing about whether the provider ever
// tells it, which is the shape of defect these cover.
// ---------------------------------------------------------------------------

fn generation_provider(options: serde_json::Value) -> CompatibleProvider {
    let mut spec = Spec::new("openrouter").with_base_url("https://example.invalid/v1");
    for (name, value) in options.as_object().expect("options are an object") {
        spec = spec.with_option(name.clone(), value.clone());
    }
    provider(spec)
}

fn generation_request() -> CompletionRequest {
    CompletionRequest::new("some-model", vec![Message::new(Role::User, "Say hello.")]).with_tools(
        vec![zuno_llm::registry::ToolSchema {
            name: "read".to_owned(),
            description: "Read a file".to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
        }],
    )
}

#[test]
fn the_option_bag_supplies_every_chat_generation_control() {
    let body = generation_provider(json!({
        "maxTokens": 16_384,
        "temperature": 0.3,
        "topP": 0.9,
        "toolChoice": "required"
    }))
    .body_for(&generation_request());

    assert_eq!(
        body["max_tokens"],
        json!(16_384),
        "`RequestBody::max_tokens` existed with no production assignment, so the field \
         was written, documented, tested — and never set"
    );
    assert_eq!(body["temperature"], json!(0.3));
    assert_eq!(body["top_p"], json!(0.9));
    assert_eq!(
        body["tool_choice"],
        json!("required"),
        "`tool_choice` was in PROTECTED_KEYS, so `extraBody` could not supply it \
         either: there was no way at all to require a tool call"
    );
}

#[test]
fn the_snake_case_spellings_are_accepted_too() {
    let body = generation_provider(json!({
        "max_tokens": 4_096,
        "top_p": 0.5,
        "tool_choice": "none"
    }))
    .body_for(&generation_request());

    assert_eq!(body["max_tokens"], json!(4_096));
    assert_eq!(body["top_p"], json!(0.5));
    assert_eq!(body["tool_choice"], json!("none"));
}

#[test]
fn a_responses_surface_spells_the_cap_as_max_output_tokens() {
    let mut spec = Spec::new("azure")
        .with_base_url("https://example.invalid/v1")
        .with_surface(ApiSurface::Responses);
    spec = spec
        .with_option("maxTokens", json!(16_384))
        .with_option(SURFACES_OPTION, json!(["responses"]));
    let body = provider(spec).body_for(&generation_request());

    assert_eq!(
        body["max_output_tokens"],
        json!(16_384),
        "one configured cap, two wire names: the surface the endpoint resolves to \
         decides, not the request's own surface hint"
    );
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn a_zero_or_negative_cap_is_dropped_rather_than_sent() {
    for cap in [json!(0), json!(-1)] {
        let body = generation_provider(json!({"maxTokens": cap})).body_for(&generation_request());
        assert!(
            body.get("max_tokens").is_none(),
            "`max_tokens: {cap}` asks the model for an empty completion, or is refused \
             outright; neither is what the author meant"
        );
    }
}

#[test]
fn an_empty_option_bag_leaves_the_body_byte_identical_to_the_pre_change_shape() {
    let body = generation_provider(json!({})).body_for(&generation_request());
    let keys: Vec<&String> = body
        .as_object()
        .expect("the body is an object")
        .keys()
        .collect();

    assert_eq!(
        keys,
        vec!["messages", "model", "stream", "stream_options", "tools"],
        "a provider configured with nothing must send exactly what it sent before \
         these fields existed: {body}"
    );
}

#[test]
fn a_tool_choice_is_withheld_when_the_model_refuses_tools() {
    let mut spec = Spec::new("openrouter").with_base_url("https://example.invalid/v1");
    spec = spec
        .with_option("toolChoice", json!("required"))
        .with_option(
            MODEL_CAPABILITIES_OPTION,
            json!({"some-model": {"tool_calls": false}}),
        );
    let body = provider(spec).body_for(&generation_request());

    assert!(
        body.get("tools").is_none() && body.get("tool_choice").is_none(),
        "a `tool_choice` with no `tools` to choose from is refused by both surfaces, \
         so sending one to a model that takes no tools trades a silent drop for a \
         hard failure: {body}"
    );
}
