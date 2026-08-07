//! What both surfaces must be able to trust about the shared composition root.

use super::*;

use oc_catalog::agent::{Agent, AgentMode, AgentSource};

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

fn catalog_with_two_models_and_a_title_override() -> (Catalog, oc_config::schema::Config) {
    let document = serde_json::from_str(
        r#"{"test":{"id":"test","name":"Test","env":[],"npm":"@ai-sdk/openai-compatible",
             "models":{
               "big":{"id":"big","name":"Big","limit":{"context":200000,"output":8192}},
               "small":{"id":"small","name":"Small","limit":{"context":100000,"output":4096}}}}}"#,
    )
    .expect("catalog document");
    let config = serde_json::from_str(
        r#"{"provider":{"test":{}},"agent":{"title":{"model":"test/small"}}}"#,
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
    let internals = resolve_internals(&config, &catalog, "test", "big", session_model, &mut notes)
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
    let internals = resolve_internals(&config, &catalog, "test", "big", session_model, &mut notes)
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

    let internals = resolve_internals(&config, &catalog, "test", "big", session_model, &mut notes)
        .expect("a declined override is not a failure");

    assert_eq!(internals.summary.model.model_id, "big");
    assert!(
        notes.iter().any(|note| note.contains("summary")),
        "the downgrade was silent; notes: {notes:?}"
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
