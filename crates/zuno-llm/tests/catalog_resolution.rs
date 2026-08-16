//! Catalog resolution against a pinned catalog and a real isolated environment.
//!
//! Every test here reads `tests/fixtures/models-dev-pinned.json` and never the
//! network. The fixture is a verbatim subset of a real
//! `https://models.opencode.ai/api.json` response — seven providers chosen because
//! between them they exercise every shape the resolver has to handle:
//!
//! | provider | why it is in the fixture |
//! |---|---|
//! | `deepseek` | `api` + `npm` at provider level, `cache_read` cost |
//! | `mistral` | a `deprecated` model, which must never be listed |
//! | `groq` | a `beta` model (listed) and a `/`-bearing model id |
//! | `inceptron` | an `alpha` model, which needs the experimental flag |
//! | `anyapi` | `experimental.modes`, which expand into extra model ids |
//! | `impossibl` | `cost.tiers` and `context_over_200k` |
//! | `zhipuai` | `interleaved: {field}` and `reasoning_options` |
//!
//! None of the seven has a `custom()` loader in the oracle
//! (`provider.ts:168-963`), so availability is decided purely by the three generic
//! sources this crate implements. That is what makes the differential meaningful:
//! a difference in the model list is a difference in *this* logic, not in a
//! provider-specific autoload rule that belongs to todos 29/30/94/95/96.

use std::collections::BTreeMap;

use zuno_auth::{Credential, Secret};
use zuno_config::schema::Config;
use zuno_llm::catalog::models_dev::CatalogDocument;
use zuno_llm::catalog::{Catalog, ResolveInput};

/// The pinned catalog, compiled in so a test cannot silently read a stale file.
const PINNED: &str = include_str!("fixtures/models-dev-pinned.json");

fn document() -> CatalogDocument {
    serde_json::from_str(PINNED).expect("the pinned fixture is valid models.dev JSON")
}

fn config(json: &str) -> Config {
    serde_json::from_str(json).expect("config parses")
}

fn api_credential() -> Credential {
    Credential::Api {
        key: Secret::new("sk-pinned"),
        metadata: None,
    }
}

#[test]
fn the_pinned_fixture_covers_every_shape_it_claims_to() {
    // A fixture that silently loses a shape turns a real regression into a pass.
    let doc = document();
    assert_eq!(doc.len(), 7, "seven providers");
    assert!(
        doc["mistral"]
            .models
            .values()
            .any(|model| model.status == Some(zuno_llm::catalog::CatalogStatus::Deprecated)),
        "a deprecated model"
    );
    assert!(
        doc["inceptron"]
            .models
            .values()
            .any(|model| model.status == Some(zuno_llm::catalog::CatalogStatus::Alpha)),
        "an alpha model"
    );
    assert!(
        doc["groq"]
            .models
            .values()
            .any(|model| model.status == Some(zuno_llm::catalog::CatalogStatus::Beta)),
        "a beta model"
    );
    assert!(
        doc["anyapi"].models.values().any(|model| model
            .experimental
            .as_ref()
            .is_some_and(|experimental| !experimental.modes.is_empty())),
        "experimental modes"
    );
    assert!(
        doc["impossibl"].models.values().any(|model| model
            .cost
            .as_ref()
            .is_some_and(|cost| cost.tiers.is_some() && cost.context_over_200k.is_some())),
        "cost tiers and the long-context band"
    );
    assert!(
        doc["zhipuai"].models.values().any(|model| model
            .interleaved
            .as_ref()
            .is_some_and(|interleaved| interleaved.field().is_some())),
        "an interleaved field"
    );
    assert!(
        doc["groq"].models.keys().any(|id| id.contains('/')),
        "a slash-bearing model id, which the qualified id must not mangle"
    );
}

// ---------------------------------------------------------------------------
// Availability, one independent source at a time.
//
// Each of the next three tests supplies EXACTLY ONE source and asserts exactly
// one provider appears. Kept separate on purpose: a later refactor that merges
// the three into a single `is_available()` predicate breaks one of them, which is
// the point. Each was verified against the 1.18.12 binary with the same fixture —
// see `tests/catalog_differential.rs`.
// ---------------------------------------------------------------------------

#[test]
fn availability_from_an_env_var_alone() {
    let input = ResolveInput::new().with_env_var("DEEPSEEK_API_KEY", "sk-x");
    let catalog = Catalog::resolve(&document(), &input);
    assert_eq!(
        catalog.model_lines(),
        vec!["deepseek/deepseek-chat", "deepseek/deepseek-reasoner"],
        "an env var alone must make its provider selectable"
    );
    let deepseek = catalog.provider("deepseek").expect("deepseek resolved");
    assert_eq!(
        deepseek.availability.effective_source(),
        Some(&zuno_llm::catalog::AvailabilitySource::EnvVar {
            name: "DEEPSEEK_API_KEY".to_owned()
        })
    );
}

#[test]
fn availability_from_stored_auth_alone() {
    let mut credentials = BTreeMap::new();
    credentials.insert("deepseek".to_owned(), api_credential());
    let input = ResolveInput::new().with_credentials(credentials);
    let catalog = Catalog::resolve(&document(), &input);
    assert_eq!(
        catalog.model_lines(),
        vec!["deepseek/deepseek-chat", "deepseek/deepseek-reasoner"],
        "a stored api credential alone must make its provider selectable"
    );
    assert_eq!(
        catalog
            .provider("deepseek")
            .expect("deepseek resolved")
            .availability
            .effective_source(),
        Some(&zuno_llm::catalog::AvailabilitySource::StoredApiKey)
    );
}

#[test]
fn availability_from_a_config_block_alone() {
    // No credential of any kind. `provider.ts:1588-1595` — declaring the block is
    // enough. Verified: `{"provider":{"groq":{}}}` under an isolated HOME listed
    // groq's models.
    let cfg = config(r#"{"provider":{"groq":{}}}"#);
    let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
    assert_eq!(
        catalog.model_lines(),
        vec!["groq/allam-2-7b", "groq/canopylabs/orpheus-v1-english"],
        "a bare config block alone must make its provider selectable"
    );
    assert_eq!(
        catalog
            .provider("groq")
            .expect("groq resolved")
            .availability
            .effective_source(),
        Some(&zuno_llm::catalog::AvailabilitySource::ConfigBlock)
    );
}

#[test]
fn a_stored_oauth_credential_alone_is_not_availability() {
    // `provider.ts:1540` gates on `type === "api"`. Verified against 1.18.12:
    // auth.json holding {"mistral":{"type":"oauth",…}} listed nothing.
    let mut credentials = BTreeMap::new();
    credentials.insert(
        "mistral".to_owned(),
        Credential::Oauth {
            refresh: Secret::new("r"),
            access: Secret::new("a"),
            expires: u64::MAX,
            account_id: None,
            enterprise_url: None,
        },
    );
    let input = ResolveInput::new().with_credentials(credentials);
    let catalog = Catalog::resolve(&document(), &input);
    assert!(
        catalog.model_lines().is_empty(),
        "an oauth credential needs its provider's own flow; the generic path \
         must not guess on its behalf"
    );
}

#[test]
fn the_three_sources_compose_without_double_counting() {
    let cfg = config(r#"{"provider":{"deepseek":{}}}"#);
    let mut credentials = BTreeMap::new();
    credentials.insert("deepseek".to_owned(), api_credential());
    let input = ResolveInput::new()
        .with_config(&cfg)
        .with_credentials(credentials)
        .with_env_var("DEEPSEEK_API_KEY", "sk-x");
    let catalog = Catalog::resolve(&document(), &input);
    let availability = &catalog
        .provider("deepseek")
        .expect("deepseek resolved")
        .availability;
    assert_eq!(availability.sources.len(), 3, "all three fired");
    assert_eq!(
        availability.effective_source(),
        Some(&zuno_llm::catalog::AvailabilitySource::ConfigBlock),
        "config is applied last, so it is the effective source"
    );
}

// ---------------------------------------------------------------------------
// Status filtering.
// ---------------------------------------------------------------------------

#[test]
fn an_alpha_model_needs_the_experimental_flag_and_a_deprecated_one_is_never_listed() {
    let cfg = config(r#"{"provider":{"inceptron":{},"mistral":{}}}"#);
    let base = ResolveInput::new().with_config(&cfg);

    let without = Catalog::resolve(&document(), &base);
    assert!(
        without.provider("inceptron").is_none(),
        "inceptron's only model is alpha, so the provider disappears"
    );
    assert_eq!(
        without
            .provider("mistral")
            .expect("mistral has one non-deprecated model")
            .models
            .keys()
            .collect::<Vec<_>>(),
        vec!["codestral-latest"],
        "devstral-2512 is deprecated"
    );

    let cfg2 = config(r#"{"provider":{"inceptron":{},"mistral":{}}}"#);
    let with = Catalog::resolve(
        &document(),
        &ResolveInput::new()
            .with_config(&cfg2)
            .with_experimental_models(true),
    );
    assert_eq!(
        with.provider("inceptron")
            .expect("alpha is listed with the flag")
            .models
            .keys()
            .collect::<Vec<_>>(),
        vec!["moonshotai/Kimi-K2.6-Fast"]
    );
    assert_eq!(
        with.provider("mistral")
            .expect("mistral still resolves")
            .models
            .len(),
        1,
        "the experimental flag does not resurrect a deprecated model"
    );
}

// ---------------------------------------------------------------------------
// Config merge, against the fixture.
// ---------------------------------------------------------------------------

#[test]
fn a_whitelist_narrows_and_a_blacklist_removes() {
    let cfg = config(
        r#"{"provider":{
             "deepseek":{"whitelist":["deepseek-chat"]},
             "groq":{"blacklist":["allam-2-7b"]}}}"#,
    );
    let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
    assert_eq!(
        catalog.model_lines(),
        vec![
            "deepseek/deepseek-chat",
            "groq/canopylabs/orpheus-v1-english"
        ]
    );
}

#[test]
fn a_config_provider_the_catalog_has_never_heard_of_resolves() {
    let cfg = config(
        r#"{"provider":{"t26gateway":{
             "name":"T26 Gateway","npm":"@ai-sdk/openai-compatible",
             "api":"https://gateway.example/v1",
             "options":{"apiKey":"sk-gw","baseURL":"https://gateway.example/v1"},
             "models":{
               "fast":{"name":"Fast","limit":{"context":32000,"output":4000}},
               "slow":{}}}}}"#,
    );
    let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
    assert_eq!(
        catalog.model_lines(),
        vec!["t26gateway/fast", "t26gateway/slow"]
    );
    let fast = catalog
        .model("t26gateway", "fast")
        .expect("the declared model resolved");
    assert_eq!(fast.name, "Fast");
    assert_eq!(fast.limit.context, 32_000.0);
    assert_eq!(fast.api.npm, "@ai-sdk/openai-compatible");
    assert_eq!(fast.api.url, "https://gateway.example/v1");
    let slow = catalog
        .model("t26gateway", "slow")
        .expect("the bare model resolved");
    assert_eq!(slow.name, "slow", "a bare model is named after its key");
    assert!(slow.capabilities.toolcall, "tool_call defaults true");
}

#[test]
fn experimental_modes_expand_into_selectable_model_ids() {
    let cfg = config(r#"{"provider":{"anyapi":{}}}"#);
    let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
    assert_eq!(
        catalog.model_lines(),
        vec![
            "anyapi/anthropic/claude-opus-4-6",
            "anyapi/anthropic/claude-opus-4-6-fast",
            "anyapi/openai/gpt-5.4",
            "anyapi/openai/gpt-5.4-fast",
        ],
        "each mode becomes its own selectable id, interleaved by collation"
    );
}

#[test]
fn config_variants_merge_and_disabled_ones_vanish() {
    let cfg = config(
        r#"{"provider":{"zhipuai":{"models":{"glm-5":{"variants":{
             "thinking":{"reasoningEffort":"high"},
             "plain":{"disabled":true}}}}}}}"#,
    );
    let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
    let model = catalog.model("zhipuai", "glm-5").expect("glm-5 resolved");
    assert_eq!(
        model.variants["thinking"]["reasoningEffort"],
        serde_json::json!("high")
    );
    assert!(!model.variants.contains_key("plain"));
    assert!(
        !model.variants["thinking"].contains_key("disabled"),
        "the disabled flag never reaches the SDK"
    );
}

#[test]
fn the_long_context_cost_band_and_tiers_survive_parsing() {
    // Not exposed on ResolvedModel — the oracle flattens cost to four numbers
    // (`provider.ts:1489-1496`) — but the document must still parse, or impossibl
    // would take the whole catalog down with it.
    let doc = document();
    let model = &doc["impossibl"].models["google/gemini-2.5-pro"];
    let cost = model.cost.as_ref().expect("impossibl prices its models");
    assert!(cost.tiers.as_ref().is_some_and(|tiers| !tiers.is_empty()));
    assert!(cost.context_over_200k.is_some());

    let cfg = config(r#"{"provider":{"impossibl":{}}}"#);
    let catalog = Catalog::resolve(&doc, &ResolveInput::new().with_config(&cfg));
    let resolved = catalog
        .model("impossibl", "google/gemini-2.5-pro")
        .expect("resolved");
    assert_eq!(resolved.cost.input, cost.input, "the base price is carried");
}

#[test]
fn a_slash_bearing_model_id_qualifies_without_mangling() {
    let cfg = config(r#"{"provider":{"groq":{}}}"#);
    let catalog = Catalog::resolve(&document(), &ResolveInput::new().with_config(&cfg));
    let model = catalog
        .model("groq", "canopylabs/orpheus-v1-english")
        .expect("resolved");
    assert_eq!(
        model.qualified_id(),
        "groq/canopylabs/orpheus-v1-english",
        "the separator is not escaped; the oracle prints it raw"
    );
}

#[test]
fn resolution_is_deterministic_across_repeated_calls() {
    // The resolver holds no state and consults no clock. Two resolutions of the
    // same inputs must be byte-identical, or a differential is meaningless.
    let cfg = config(r#"{"provider":{"deepseek":{},"groq":{},"zhipuai":{}}}"#);
    let doc = document();
    let first = Catalog::resolve(&doc, &ResolveInput::new().with_config(&cfg));
    let second = Catalog::resolve(&doc, &ResolveInput::new().with_config(&cfg));
    assert_eq!(first, second);
    assert_eq!(first.model_lines(), second.model_lines());
}
