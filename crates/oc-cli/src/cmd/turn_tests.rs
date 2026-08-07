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
    }
}

#[test]
fn model_selection_splits_only_the_provider_prefix() {
    let document = serde_json::from_str(
        r#"{"anyapi":{"id":"anyapi","name":"AnyAPI","env":[],"models":{"openai/gpt":{"id":"openai/gpt","name":"GPT","limit":{"context":1,"output":1}}}}}"#,
    )
    .expect("catalog document");
    let config = serde_json::from_str(r#"{"provider":{"anyapi":{}}}"#).expect("config");
    let catalog = Catalog::resolve(&document, &ResolveInput::new().with_config(&config));
    let (provider, model, _) =
        select_model(&catalog, Some("anyapi/openai/gpt")).expect("nested model id");
    assert_eq!(provider, "anyapi");
    assert_eq!(model, "openai/gpt");
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
