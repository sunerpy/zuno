//! The registry's diagnostics: three outcomes that must not read alike.
//!
//! The QA scenarios for this work are:
//!
//! - **happy** — registering two fake providers and resolving both by key
//!   succeeds;
//! - **failure** — a fallible factory that declines reports "unavailable", while a
//!   missing key reports "not registered", as two distinct messages.
//!
//! The second is the whole reason two registration forms exist. The reference
//! implementation returns `Option` from both paths and logs one warning for both
//! (`.omo/refs/jcode/crates/jcode-base/src/provider/external.rs:219-245`), which
//! tells a user who has simply not logged in that the program is miswired.

use oc_error::{ProviderError, Recoverable, Recovery};
use oc_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, CredentialPresence, Declined, FinishReason,
    Message, Provider, ProviderRegistry, ProviderStream, RegistryError, Role, Spec, StreamEvent,
    Unavailable,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A provider that streams back the text it was sent, so the trait's one I/O
/// method is exercised rather than merely implemented.
#[derive(Debug)]
struct Echo {
    id: &'static str,
    capabilities: Capabilities,
}

impl Echo {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            capabilities: Capabilities::text_only(),
        }
    }

    fn reasoning(id: &'static str) -> Self {
        Self {
            id,
            capabilities: Capabilities {
                reasoning: true,
                tool_calls: true,
                prompt_cache: true,
                attachments: true,
                sampling_params: false,
            },
        }
    }
}

impl Provider for Echo {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let mut events: Vec<Result<StreamEvent, ProviderError>> = request
            .messages
            .iter()
            .map(|message| Ok(StreamEvent::TextDelta(message.text.clone())))
            .collect();
        events.push(Ok(StreamEvent::Finish(FinishReason::Stop)));
        Box::pin(futures::stream::iter(events))
    }
}

/// A credential store stub. The real one lives in `oc-auth`, which `oc-llm`
/// deliberately does not depend on.
#[derive(Debug)]
struct StoredFor(&'static [&'static str]);

impl CredentialPresence for StoredFor {
    fn has_credential(&self, provider: &str) -> bool {
        self.0.contains(&provider)
    }
}

fn drain(stream: ProviderStream<'_>) -> Vec<StreamEvent> {
    futures::executor::block_on(futures::StreamExt::collect::<Vec<_>>(stream))
        .into_iter()
        .map(|event| event.expect("the echo provider never errors"))
        .collect()
}

// ---------------------------------------------------------------------------
// QA scenario: happy path.
// ---------------------------------------------------------------------------

#[test]
fn registry_resolves_two_registered_providers_by_key() {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", |_spec| Arc::new(Echo::reasoning("anthropic")));
    registry.register_fallible("openai", |_spec| Ok(Arc::new(Echo::new("openai"))));

    assert_eq!(registry.len(), 2);
    assert_eq!(registry.registered(), vec!["anthropic", "openai"]);

    let anthropic = registry
        .resolve_key("anthropic")
        .expect("anthropic is registered and always constructs");
    let openai = registry
        .resolve_key("openai")
        .expect("openai is registered and its factory succeeds");

    assert_eq!(anthropic.id(), "anthropic");
    assert_eq!(openai.id(), "openai");
    assert!(anthropic.capabilities().reasoning);
    assert!(!openai.capabilities().reasoning);
}

#[test]
fn registry_resolved_provider_streams_through_the_traits_one_io_method() {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", |_spec| Arc::new(Echo::new("anthropic")));

    let provider = registry.resolve_key("anthropic").expect("registered");
    let request = CompletionRequest::new(
        "claude-sonnet-4-5",
        vec![
            Message::new(Role::System, "be brief"),
            Message::new(Role::User, "hello"),
        ],
    );

    assert_eq!(
        drain(provider.stream(request)),
        vec![
            StreamEvent::TextDelta("be brief".to_owned()),
            StreamEvent::TextDelta("hello".to_owned()),
            StreamEvent::Finish(FinishReason::Stop),
        ]
    );
}

#[test]
fn registry_forwards_the_spec_to_a_reusable_factory() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);

    let mut registry = ProviderRegistry::new();
    registry.register_fallible("azure", |spec| {
        CALLS.fetch_add(1, Ordering::Relaxed);
        assert_eq!(spec.provider, "azure");
        assert_eq!(spec.surface, ApiSurface::Chat);
        assert_eq!(spec.api_version.as_deref(), Some("2024-10-21"));
        assert_eq!(
            spec.base_url.as_deref(),
            Some("https://contoso.openai.azure.com")
        );
        Ok(Arc::new(Echo::new("azure")))
    });

    let spec = Spec::new("azure")
        .with_surface(ApiSurface::Chat)
        .with_api_version("2024-10-21")
        .with_base_url("https://contoso.openai.azure.com");

    for _ in 0..3 {
        registry
            .resolve(spec.clone())
            .expect("azure is registered and its factory succeeds");
    }
    assert_eq!(CALLS.load(Ordering::Relaxed), 3);
}

// ---------------------------------------------------------------------------
// QA scenario: failure path. Two distinct messages.
// ---------------------------------------------------------------------------

#[test]
fn registry_reports_unavailable_and_not_registered_as_different_messages() {
    let mut registry = ProviderRegistry::new();
    registry.register_fallible("github-copilot", |_spec| {
        Err(Declined::Unavailable(Unavailable::MissingCredential))
    });

    let unavailable = registry
        .resolve_key("github-copilot")
        .expect_err("the factory declines");
    let not_registered = registry
        .resolve_key("bedrock")
        .expect_err("no factory was ever registered for bedrock");

    let unavailable = unavailable.to_string();
    let not_registered = not_registered.to_string();

    assert_eq!(
        unavailable,
        "provider `github-copilot` is unavailable: no credential is stored for it"
    );
    assert_eq!(
        not_registered,
        "provider `bedrock` is not registered; the composition root must call \
         ProviderRegistry::register() or ProviderRegistry::register_fallible() for `bedrock` \
         at startup"
    );
    assert_ne!(unavailable, not_registered);
    assert!(!unavailable.contains("not registered"));
    assert!(!not_registered.contains("unavailable"));
}

#[test]
fn registry_separates_a_user_state_from_a_wiring_bug() {
    let mut registry = ProviderRegistry::new();
    registry.register_fallible("github-copilot", |_spec| {
        Err(Declined::Unavailable(Unavailable::MissingCredential))
    });

    let unavailable = registry.resolve_key("github-copilot").unwrap_err();
    let not_registered = registry.resolve_key("bedrock").unwrap_err();

    assert!(!unavailable.is_wiring_bug());
    assert!(not_registered.is_wiring_bug());
    assert_eq!(unavailable.provider(), "github-copilot");
    assert_eq!(not_registered.provider(), "bedrock");
    assert_eq!(unavailable.recovery(), Recovery::Reauthenticate);
    assert_eq!(not_registered.recovery(), Recovery::Fail);
}

#[test]
fn registry_renders_its_own_advice_for_every_unavailable_reason() {
    let reasons = [
        (
            Unavailable::MissingCredential,
            "provider `p` is unavailable: no credential is stored for it",
            Recovery::Reauthenticate,
        ),
        (
            Unavailable::UnsupportedPlatform,
            "provider `p` is unavailable: it is not supported on this platform or build",
            Recovery::Fail,
        ),
        (
            Unavailable::IncompleteConfiguration,
            "provider `p` is unavailable: its configuration is incomplete",
            Recovery::Fail,
        ),
    ];

    for (reason, expected, recovery) in reasons {
        let error = RegistryError::Unavailable {
            provider: "p".to_owned(),
            reason,
        };
        assert_eq!(error.to_string(), expected);
        assert_eq!(error.recovery(), recovery);
    }
}

#[test]
fn registry_keeps_a_failing_factorys_own_classification() {
    let mut registry = ProviderRegistry::new();
    registry.register_fallible("bedrock", |_spec| {
        Err(Declined::Failed(ProviderError::Transient {
            status: Some(503),
            source: None,
        }))
    });

    let error = registry.resolve_key("bedrock").unwrap_err();
    assert!(!error.is_wiring_bug());
    assert_eq!(error.recovery(), Recovery::Retry { after: None });
    assert_eq!(error.to_string(), "provider `bedrock` failed to construct");

    let lifted = ProviderError::from(error);
    assert!(matches!(
        lifted,
        ProviderError::Transient {
            status: Some(503),
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Acceptance criterion: a credentialed provider with no factory names the key and
// the composition root.
// ---------------------------------------------------------------------------

#[test]
fn registry_names_the_key_and_the_composition_root_for_a_credentialed_provider() {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", |_spec| Arc::new(Echo::new("anthropic")));

    let credentials = StoredFor(&["anthropic", "github-copilot", "bedrock"]);
    let candidates = ["anthropic", "github-copilot", "bedrock", "openai"];

    let unwired = registry.unwired(&credentials, &candidates);

    assert_eq!(unwired.len(), 2, "unwired reported {unwired:?}");
    assert_eq!(unwired[0].provider(), "bedrock");
    assert_eq!(unwired[1].provider(), "github-copilot");

    for error in &unwired {
        let rendered = error.to_string();
        assert!(
            rendered.contains(error.provider()),
            "the diagnostic must name the provider key: {rendered}"
        );
        assert!(
            rendered.contains("composition root"),
            "the diagnostic must name what has to be fixed: {rendered}"
        );
        assert!(error.is_wiring_bug());
    }
}

#[test]
fn registry_unwired_ignores_an_uncredentialed_and_an_already_wired_provider() {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", |_spec| Arc::new(Echo::new("anthropic")));
    registry.register_fallible("github-copilot", |_spec| {
        Err(Declined::Unavailable(Unavailable::MissingCredential))
    });

    let credentials = StoredFor(&["anthropic", "github-copilot"]);
    let unwired = registry.unwired(&credentials, &["anthropic", "github-copilot", "bedrock"]);

    assert!(
        unwired.is_empty(),
        "a wired provider is not a wiring bug even when its factory declines, and an \
         unregistered provider with no credential is not one either: {unwired:?}"
    );
}

// ---------------------------------------------------------------------------
// Registry mechanics.
// ---------------------------------------------------------------------------

#[test]
fn registry_re_registering_a_key_replaces_the_earlier_factory() {
    let mut registry = ProviderRegistry::new();
    registry.register("openai", |_spec| Arc::new(Echo::reasoning("openai")));
    assert!(
        registry
            .resolve_key("openai")
            .unwrap()
            .capabilities()
            .reasoning
    );

    registry.register("openai", |_spec| Arc::new(Echo::new("openai-fake")));
    let replaced = registry.resolve_key("openai").unwrap();
    assert_eq!(replaced.id(), "openai-fake");
    assert!(!replaced.capabilities().reasoning);
    assert_eq!(registry.len(), 1);
}

#[test]
fn registry_empty_registers_nothing_and_resolves_nothing() {
    let registry = ProviderRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(registry.registered().is_empty());
    assert!(!registry.is_registered("anthropic"));
    assert!(
        registry
            .resolve_key("anthropic")
            .unwrap_err()
            .is_wiring_bug()
    );
}

#[test]
fn registry_clone_shares_its_factories() {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", |_spec| Arc::new(Echo::new("anthropic")));

    let clone = registry.clone();
    assert_eq!(clone.registered(), registry.registered());
    assert_eq!(clone.resolve_key("anthropic").unwrap().id(), "anthropic");
}

#[test]
fn registry_debug_lists_the_registered_keys_in_a_stable_order() {
    let mut registry = ProviderRegistry::new();
    for key in ["openai", "anthropic", "bedrock"] {
        registry.register(key, move |_spec| Arc::new(Echo::new("fake")));
    }
    assert_eq!(
        format!("{registry:?}"),
        r#"ProviderRegistry { registered: ["anthropic", "bedrock", "openai"] }"#
    );
}

#[test]
fn registry_not_registered_lifted_into_the_taxonomy_still_says_how_to_fix_it() {
    let registry = ProviderRegistry::new();
    let error = registry.resolve_key("bedrock").unwrap_err();

    let lifted = ProviderError::from(error);
    assert!(matches!(lifted, ProviderError::Fatal { status: None, .. }));
    assert_eq!(lifted.recovery(), Recovery::Fail);

    let cause = std::error::Error::source(&lifted).expect("the registry error is chained");
    let rendered = cause.to_string();
    assert!(rendered.contains("bedrock"), "{rendered}");
    assert!(rendered.contains("composition root"), "{rendered}");
}

#[test]
fn registry_missing_credential_lifted_into_the_taxonomy_asks_for_reauthentication() {
    let mut registry = ProviderRegistry::new();
    registry.register_fallible("github-copilot", |_spec| {
        Err(Declined::Unavailable(Unavailable::MissingCredential))
    });

    let lifted = ProviderError::from(registry.resolve_key("github-copilot").unwrap_err());
    let ProviderError::Auth { ref provider, .. } = lifted else {
        panic!("a missing credential must become an Auth failure, got {lifted:?}");
    };
    assert_eq!(provider, "github-copilot");
    assert_eq!(lifted.recovery(), Recovery::Reauthenticate);
}

// ---------------------------------------------------------------------------
// The spec carries what the three parameterized oracle loaders need.
// ---------------------------------------------------------------------------

#[test]
fn registry_spec_expresses_every_parameterized_provider_in_the_oracle() {
    let azure = Spec::new("azure")
        .with_base_url("https://contoso.openai.azure.com")
        .with_api_version("2024-10-21")
        .with_surface(ApiSurface::Chat);
    assert_eq!(azure.surface, ApiSurface::Chat);
    assert_eq!(azure.api_version.as_deref(), Some("2024-10-21"));

    let mantle = Spec::new("amazon-bedrock-mantle").with_region("us-west-2");
    assert_eq!(mantle.region.as_deref(), Some("us-west-2"));
    assert_eq!(
        CompletionRequest::new("openai.gpt-oss-safeguard-20b", Vec::new())
            .on_surface(ApiSurface::Chat)
            .surface,
        ApiSurface::Chat,
    );
    assert_eq!(
        CompletionRequest::new("anthropic.claude-sonnet-4-5", Vec::new())
            .on_surface(ApiSurface::Responses)
            .surface,
        ApiSurface::Responses,
    );

    let vertex = Spec::new("google-vertex-anthropic")
        .with_project("acme-prod")
        .with_region("us")
        .with_base_url("https://aiplatform.us.rep.googleapis.com/v1");
    assert_eq!(vertex.project.as_deref(), Some("acme-prod"));

    let anthropic = Spec::new("anthropic").with_header(
        "anthropic-beta",
        "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14",
    );
    assert_eq!(
        anthropic.headers.get("anthropic-beta").map(String::as_str),
        Some("interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14")
    );

    let compatible = Spec::new("openai-compatible")
        .with_base_url("https://api.example.test/v1")
        .with_option("resourceName", serde_json::Value::String("acme".to_owned()));
    assert_eq!(
        compatible.options.get("resourceName"),
        Some(&serde_json::Value::String("acme".to_owned()))
    );

    assert_eq!(Spec::new("anthropic").surface, ApiSurface::Default);
}
