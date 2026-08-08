//! What both surfaces must be able to trust about the shared composition root.

use super::*;

use oc_catalog::agent::{Agent, AgentMode, AgentSource};
use oc_paths::Env;

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

fn plan(directory: &str, session: SessionChoice) -> TurnPlan {
    let directory = PathBuf::from(directory);
    let project = oc_paths::project::ResolvedProject {
        previous: None,
        id: "project-turn-test".to_owned(),
        directory: directory.clone(),
        vcs: None,
    };
    let agent = agent("build");
    TurnPlan {
        resolver: Resolver {
            requested_agent: agent.name.clone(),
            system_prompt: String::new(),
            max_steps: DEFAULT_MAX_STEPS,
            requested_provider: "provider".to_owned(),
            requested_model: "model".to_owned(),
            wire_model: "model".to_owned(),
            spec: Spec::new(COMPATIBLE_PROVIDER).with_surface(ApiSurface::Chat),
        },
        directory,
        project,
        config: oc_config::schema::Config::default(),
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

/// Two models under one provider, with `title` overridden to the smaller one.
///
/// The provider carries an endpoint in `options.baseURL`. It has to: a provider with no
/// endpoint in either place is one no turn could ever run against, and building specs
/// from it was how a spec with no base URL used to pass unnoticed. This is the same
/// lesson as the top-level `api` key the seam tests no longer send — a fixture must not
/// be servable in ways the real input shape is not, and must not be unservable in ways
/// it would not be either.
fn catalog_with_two_models_and_a_title_override() -> (Catalog, oc_config::schema::Config) {
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

/// The catalog a forbidden fetch leaves behind, as [`CatalogSource::load`] builds it.
fn forbidden_fetch() -> CatalogProvenance {
    CatalogProvenance::FetchForbidden {
        origin: "https://models.opencode.ai".to_owned(),
        cache: PathBuf::from("/nowhere/cache/opencode/models.json"),
    }
}

/// A config that specifies a provider and a model end to end, as an air-gapped user
/// pointing at a private gateway writes it.
fn self_specified_config() -> oc_config::schema::Config {
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
        &oc_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );

    let (provider, model, resolved) =
        select_model(&catalog, Some("private/house-model"), &forbidden_fetch())
            .expect("a config that fully specifies the model needs no catalog");

    assert_eq!(provider, "private");
    assert_eq!(model, "house-model");
    assert_eq!(resolved.api.url, "https://gateway.internal/v1");
    assert!(
        supports_compatible_transport(&resolved.api.npm),
        "the config's transport must survive resolution or the turn is refused later"
    );
}

/// The other half: a model nobody defines still fails immediately, and names the fix.
#[test]
fn a_model_no_config_defines_fails_immediately_and_names_the_fix() {
    let config = self_specified_config();
    let catalog = Catalog::resolve(
        &oc_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new().with_config(&config),
    );

    let message = select_model(&catalog, Some("private/absent-model"), &forbidden_fetch())
        .expect_err("nothing defines this model");

    for needle in [
        "private/absent-model",
        "provider",
        "OPENCODE_DISABLE_MODELS_FETCH",
        "https://models.opencode.ai",
        "/nowhere/cache/opencode/models.json",
        "OPENCODE_MODELS_PATH",
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
        &oc_llm::catalog::models_dev::CatalogDocument::new(),
        &ResolveInput::new(),
    );
    let message = select_model(&catalog, None, &forbidden_fetch())
        .expect_err("an empty catalog offers no default");
    assert!(
        message.contains("OPENCODE_DISABLE_MODELS_FETCH"),
        "{message}"
    );
    assert!(
        message.contains("/nowhere/cache/opencode/models.json"),
        "{message}"
    );

    // And a catalog that was genuinely loaded must NOT blame the flag.
    let loaded = select_model(&catalog, None, &CatalogProvenance::Fetched)
        .expect_err("an empty catalog offers no default");
    assert!(
        !loaded.contains("OPENCODE_DISABLE_MODELS_FETCH"),
        "a loaded catalog that lists nothing is a configuration problem, not a \
         policy one: {loaded}"
    );
}

#[test]
fn new_session_and_user_message_are_persisted_together() {
    let mut connection =
        oc_db::open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
    oc_db::migration::apply(&mut connection).expect("apply schema");
    let plan = plan("/workspace", SessionChoice::New);
    let now = 1_780_000_000_000;
    ensure_project(&connection, &plan.project, now).expect("persist project");
    let session = resolve_session(&mut connection, &plan, now).expect("create session");
    persist_user_message(
        &connection,
        &session.id,
        "build",
        "provider",
        "model",
        "hello",
        now,
    )
    .expect("persist prompt");

    let store = oc_db::message::MessageStore::new(&connection);
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

#[test]
fn an_explicit_session_is_reused_rather_than_created() {
    let mut connection =
        oc_db::open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
    oc_db::migration::apply(&mut connection).expect("apply schema");
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
    let internals = resolve_internals(
        &config,
        &catalog,
        "test",
        "big",
        session_model,
        &Env::empty(),
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
        oc_catalog::agent::builtin::PROMPT_TITLE,
        "the title prompt was rewritten instead of read from the catalog"
    );
}

/// Every name the roster declares internal must resolve here.
///
/// The assertion is over [`oc_agent::builtin::INTERNAL_NAMES`] and not over three
/// literals, so a fourth internal added to the roster fails this test rather than
/// silently becoming another declared-and-never-invoked entry — which is the exact
/// defect this wiring exists to remove.
#[test]
fn the_resolved_set_is_exactly_what_the_roster_calls_internal() {
    let (catalog, config) = catalog_with_two_models_and_a_title_override();
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();
    let internals = resolve_internals(
        &config,
        &catalog,
        "test",
        "big",
        session_model,
        &Env::empty(),
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
        oc_agent::builtin::INTERNAL_NAMES.into_iter().collect();
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
    let config: oc_config::schema::Config =
        serde_json::from_str(r#"{"agent":{"summary":{"model":"elsewhere/some-model"}}}"#)
            .expect("config");
    let session_model = catalog.model("test", "big").expect("the session model");
    let mut notes = Vec::new();

    let internals = resolve_internals(
        &config,
        &catalog,
        "test",
        "big",
        session_model,
        &Env::empty(),
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
    let config: oc_config::schema::Config = serde_json::from_value(serde_json::json!({
        "provider": {"probe": serde_json::Value::Object(provider)}
    }))
    .expect("config");
    Catalog::resolve(
        &oc_llm::catalog::models_dev::CatalogDocument::new(),
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
    let config: oc_config::schema::Config = serde_json::from_str(
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
    let config: oc_config::schema::Config = serde_json::from_value(serde_json::json!({
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
    let config: oc_config::schema::Config = serde_json::from_value(serde_json::json!({
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
    let stored = oc_auth::Credential::Api {
        key: oc_auth::Secret::new("sk-from-the-store"),
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

        assert_eq!(resolved.as_deref(), expected, "{why} (options={options})");
    }
}

/// Why [`provider_api_key`]'s string test can never be reached from a config file.
///
/// `ProviderOptions::api_key` is typed `Option<String>`
/// (`oc-config/src/schema/provider.rs:54`), so a non-string `apiKey` is refused before
/// any provider is resolved and the `as_str` guard is belt over braces — kept because
/// `ResolvedProvider::options` is a free-form JSON map that a future non-config source
/// could populate, and a number silently becoming `Bearer 7` is not an acceptable
/// outcome. Asserted rather than assumed, so a schema change that loosened the field
/// would show up here instead of at a gateway.
#[test]
fn a_non_string_api_key_never_reaches_the_resolved_provider() {
    let refused = serde_json::from_value::<oc_config::schema::Config>(serde_json::json!({
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
        oc_provider_compatible::surface::use_completion_urls(&spec),
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
    let config: oc_config::schema::Config = serde_json::from_str(
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
    let internals = resolve_internals(
        &config,
        &catalog,
        "test",
        "big",
        session_model,
        &Env::empty(),
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

/// Neither surface may compose a turn of its own.
///
/// The whole point of this module is that `run` and the TUI cannot drift apart in
/// which tools exist, which rules govern them, or how a session is resolved — and
/// the way they would drift is a second call site. A source scan is crude and it is
/// also the only check that fails when someone reintroduces one, because a duplicate
/// composition compiles, passes clippy, and passes every behavioural test twice.
#[test]
fn only_this_module_composes_a_turn() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cmd");
    let composition = ["ToolRegistryDispatcher::new", "run_turn("];
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
    }
    assert!(
        scanned >= 17,
        "scanned only {scanned} files under {}; the scan is looking in the wrong \
         place and would pass vacuously",
        directory.display()
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
        TurnError::NestedToolUse { step: 4 },
        TurnError::ToolUseEndWithoutStart { step: 5 },
        TurnError::EventConsumerClosed,
        TurnError::Database(oc_error::DbError::Busy { retry_after: None }),
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
        TurnError::Cache(oc_llm::cache::CacheViolation::StaticPrefixChanged { turn: 2 }),
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
            TurnError::NestedToolUse { .. } => "NestedToolUse",
            TurnError::ToolUseEndWithoutStart { .. } => "ToolUseEndWithoutStart",
            TurnError::EventConsumerClosed => "EventConsumerClosed",
            TurnError::Database(_) => "Database",
            TurnError::Provider(_) => "Provider",
            TurnError::Cache(_) => "Cache",
        };
        named.insert(name);
    }

    assert_eq!(
        named.len(),
        12,
        "the table covers only {named:?}; every variant needs a value or the rendering \
         claims above are vacuous for the ones missing"
    );
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
    for needle in ["provider.test.options.apiKey", "auth login test"] {
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
