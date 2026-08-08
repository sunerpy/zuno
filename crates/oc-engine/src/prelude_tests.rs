//! Given a seeded session, when the prelude runs, then the internals are invoked.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream;
use oc_config::schema::CompactionConfig;
use oc_db::message::{MessageRecord, MessageStore, PartRecord};
use oc_db::{Connection, migration, open};
use oc_error::ProviderError;
use oc_llm::event::{Message, RequestContentBlock, Role, StreamEvent};
use oc_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderStream, Spec,
};

use super::{
    CompactionSkipped, InternalAgent, InternalProviders, Internals, PreludeContext,
    TITLE_INSTRUCTION, TitleSkipped, compact_if_overflowing, estimate_tokens, generate_title,
    run_prelude, summarize, transcript, transcript_owned,
};
use crate::compaction::{CompactionState, TokenWindow, select_boundary};
use crate::r#loop::retained_history;

const SESSION_ID: &str = "ses_prelude_test";
const TITLE_PROMPT: &str = "You are a title generator. You output ONLY a thread title.";
const COMPACTION_PROMPT: &str = "You rewrite a transcript that outgrew its window.";
const SUMMARY_PROMPT: &str = "You summarise what a session accomplished.";

/// A window small enough that a handful of seeded messages overflow it.
const TINY_WINDOW: TokenWindow = TokenWindow {
    context: 1_000,
    max_output: 100,
};

/// A window nothing in these tests comes close to filling.
const ROOMY_WINDOW: TokenWindow = TokenWindow {
    context: 1_000_000,
    max_output: 10_000,
};

#[test]
fn owned_transcript_is_byte_identical_and_moves_large_payload() {
    let connection = seeded("A named session", None);
    let large_text = "x".repeat(2 * 1024 * 1024);
    put_user(&connection, "msg_large", 10, &large_text);
    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate large history");
    let source_pointer = history[0].parts[0].data["text"]
        .as_str()
        .expect("large text")
        .as_ptr();
    let expected = transcript(COMPACTION_PROMPT, &history);

    let actual = transcript_owned(COMPACTION_PROMPT, history);
    let moved_pointer = actual
        .iter()
        .flat_map(|entry| &entry.message.content)
        .find_map(|block| match block {
            RequestContentBlock::Text { text } if text.len() == large_text.len() => {
                Some(text.as_ptr())
            }
            _ => None,
        })
        .expect("large text remains in the transcript");

    assert_eq!(
        actual, expected,
        "owned projection changed transcript bytes"
    );
    assert_eq!(
        moved_pointer, source_pointer,
        "owned projection copied the large payload instead of moving it"
    );
}

#[derive(Debug)]
struct RecordingProvider {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl RecordingProvider {
    fn answering(texts: &[&str]) -> Arc<Self> {
        let responses = texts
            .iter()
            .map(|text| {
                vec![
                    Ok(StreamEvent::TextDelta((*text).to_owned())),
                    Ok(StreamEvent::MessageEnd { stop_reason: None }),
                ]
            })
            .collect::<VecDeque<_>>();
        Arc::new(Self {
            responses: Mutex::new(responses),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn failing(message: &str) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(VecDeque::from([vec![Ok(StreamEvent::Error {
                message: message.to_owned(),
                retry_after: None,
            })]])),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "recording"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::text_only()
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        self.requests.lock().expect("request lock").push(request);
        let events = self
            .responses
            .lock()
            .expect("response lock")
            .pop_front()
            .unwrap_or_default();
        Box::pin(stream::iter(events))
    }
}

struct OneProvider(Arc<RecordingProvider>);

impl InternalProviders for OneProvider {
    fn provider_for(&self, _agent: &InternalAgent) -> Result<Arc<dyn Provider>, String> {
        Ok(Arc::clone(&self.0) as Arc<dyn Provider>)
    }
}

struct NoProvider;

impl InternalProviders for NoProvider {
    fn provider_for(&self, agent: &InternalAgent) -> Result<Arc<dyn Provider>, String> {
        Err(format!("no credential answers for `{}`", agent.name))
    }
}

fn internals() -> Internals {
    let model = |id: &str| {
        crate::r#loop::ResolvedModel::new(Spec::new("recording"), id, ApiSurface::Default)
    };
    Internals {
        title: InternalAgent {
            name: "title".to_owned(),
            prompt: TITLE_PROMPT.to_owned(),
            model: model("small"),
        },
        compaction: InternalAgent {
            name: "compaction".to_owned(),
            prompt: COMPACTION_PROMPT.to_owned(),
            model: model("small"),
        },
        summary: InternalAgent {
            name: "summary".to_owned(),
            prompt: SUMMARY_PROMPT.to_owned(),
            model: model("small"),
        },
    }
}

fn seeded(title: &str, parent: Option<&str>) -> Connection {
    let mut connection = open::open(&oc_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    let parent_column = parent.map_or_else(|| "NULL".to_owned(), |id| format!("'{id}'"));
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-prelude', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, parent_id, slug, directory, title, version, time_created, \
                time_updated) \
             VALUES ('{SESSION_ID}', 'project-prelude', {parent_column}, 'prelude', '/workspace', \
                     '{title}', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn put_user(connection: &Connection, id: &str, created: i64, text: &str) {
    let message = MessageRecord::from_json(serde_json::json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "recording", "modelID": "big" }
    }))
    .expect("valid user message");
    let part = PartRecord::from_json(
        serde_json::json!({
            "id": format!("prt_{id}"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "text",
            "text": text
        }),
        created,
    )
    .expect("valid user part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist user message");
    store
        .put_part_at(&part, created)
        .expect("persist user part");
}

/// A finished assistant message whose measured usage is `used_tokens`.
fn put_assistant(connection: &Connection, id: &str, created: i64, text: &str, used_tokens: u64) {
    let message = MessageRecord::from_json(serde_json::json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created },
        "finish": "stop",
        "agent": "build",
        "mode": "build",
        "modelID": "big",
        "providerID": "recording",
        "cost": 0.0,
        "tokens": {
            "input": used_tokens,
            "output": 0,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 }
        }
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        serde_json::json!({
            "id": format!("prt_{id}"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "text",
            "text": text
        }),
        created,
    )
    .expect("valid assistant part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist assistant part");
}

/// An assistant message calling one tool, and the completed result for it.
fn put_tool_exchange(connection: &Connection, id: &str, created: i64, call_id: &str) {
    let message = MessageRecord::from_json(serde_json::json!({
        "id": id,
        "sessionID": SESSION_ID,
        "role": "assistant",
        "time": { "created": created, "completed": created },
        "finish": "tool-calls",
        "agent": "build",
        "mode": "build",
        "modelID": "big",
        "providerID": "recording",
        "cost": 0.0,
        "tokens": { "input": 10, "output": 1, "reasoning": 0, "cache": { "read": 0, "write": 0 } }
    }))
    .expect("valid assistant message");
    let part = PartRecord::from_json(
        serde_json::json!({
            "id": format!("prt_{id}_tool"),
            "sessionID": SESSION_ID,
            "messageID": id,
            "type": "tool",
            "callID": call_id,
            "tool": "read",
            "state": {
                "status": "completed",
                "input": { "filePath": "/workspace/main.rs" },
                "raw": "{}",
                "output": "fn main() {}",
                "title": "read main.rs"
            }
        }),
        created,
    )
    .expect("valid tool part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist assistant message");
    store
        .put_part_at(&part, created)
        .expect("persist tool part");
}

fn context<'a>(
    connection: &'a mut Connection,
    providers: &'a dyn InternalProviders,
    internals: &'a Internals,
    config: &'a CompactionConfig,
    window: TokenWindow,
    state: &'a mut CompactionState,
) -> PreludeContext<'a> {
    PreludeContext {
        connection,
        providers,
        internals,
        compaction: config,
        window,
        state,
    }
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            RequestContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn title_of(connection: &Connection) -> String {
    oc_db::session::get(connection, SESSION_ID)
        .expect("the session is readable")
        .title
}

#[tokio::test]
async fn a_new_sessions_first_turn_issues_a_tool_free_title_request_and_persists_the_answer() {
    // Given: a session still carrying the placeholder title `create` invented, and
    // one real user turn.
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    put_user(&connection, "msg_001", 10, "Refactor the user service");
    let provider = RecordingProvider::answering(&["Refactoring user service"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    // When: the prelude runs before the turn.
    let outcome = run_prelude(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("the prelude never fails the turn on a readable session");

    // Then: exactly one request went out, it offered no tools, and the answer is now
    // the session's title.
    let requests = provider.requests();
    assert_eq!(
        requests.len(),
        1,
        "a new session's prelude is exactly one request; got {requests:#?}"
    );
    assert!(
        requests[0].tools.is_empty(),
        "the title request offered tools, but every internal denies all of them: {:#?}",
        requests[0].tools
    );
    assert_eq!(
        outcome.title.as_deref(),
        Some("Refactoring user service"),
        "skipped: {:?}",
        outcome.skipped
    );
    assert_eq!(
        title_of(&connection),
        "Refactoring user service",
        "the generated title was not persisted"
    );
}

#[tokio::test]
async fn the_title_request_carries_the_agents_prompt_the_instruction_and_the_opening_exchange() {
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    put_user(&connection, "msg_001", 10, "Debug 500 errors in production");
    let provider = RecordingProvider::answering(&["Debugging production 500 errors"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    generate_title(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("the title is generated");

    let messages = &provider.requests()[0].messages;
    assert_eq!(messages[0].role, Role::System);
    assert_eq!(text_of(&messages[0]), TITLE_PROMPT);
    assert_eq!(messages[1].role, Role::User);
    assert_eq!(text_of(&messages[1]), TITLE_INSTRUCTION);
    assert_eq!(
        text_of(&messages[2]),
        "Debug 500 errors in production",
        "the opening exchange did not reach the title model: {messages:#?}"
    );
}

#[tokio::test]
async fn an_already_titled_session_is_never_retitled() {
    let mut connection = seeded("A title the user chose", None);
    put_user(&connection, "msg_001", 10, "Anything at all");
    let provider = RecordingProvider::answering(&["Something else"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let title = generate_title(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("declining is not a failure");

    assert_eq!(title, None);
    assert!(
        provider.requests().is_empty(),
        "a named session must not spend a request on a title it already has"
    );
    assert_eq!(title_of(&connection), "A title the user chose");
}

#[tokio::test]
async fn a_child_session_inherits_its_parents_naming_and_is_not_titled() {
    let mut connection = seeded(
        "Child session - 2026-08-07T00:00:00.000Z",
        Some("ses_parent"),
    );
    put_user(&connection, "msg_001", 10, "Do the subtask");
    let provider = RecordingProvider::answering(&["Subtask"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let title = generate_title(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("declining is not a failure");

    assert_eq!(title, None);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn a_session_with_two_user_turns_is_past_the_point_its_opening_describes_it() {
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    put_user(&connection, "msg_001", 10, "First question");
    put_assistant(&connection, "msg_002", 20, "First answer", 10);
    put_user(&connection, "msg_003", 30, "Second question");
    let provider = RecordingProvider::answering(&["Too late"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let title = generate_title(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("declining is not a failure");

    assert_eq!(title, None);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn a_thinking_block_is_stripped_and_an_over_long_line_is_truncated() {
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    put_user(&connection, "msg_001", 10, "Explain the plan");
    let long = "x".repeat(200);
    let provider =
        RecordingProvider::answering(&[&format!("<think>deliberating</think>\n\n{long}")]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let title = generate_title(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("the title is generated")
    .expect("a usable line was produced");

    assert_eq!(title.chars().count(), 100);
    assert!(title.ends_with("..."), "{title}");
    assert!(
        !title.contains("deliberating"),
        "the reasoning block reached the title: {title}"
    );
}

#[tokio::test]
async fn a_provider_failure_skips_the_title_and_never_fails_the_turn() {
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    put_user(&connection, "msg_001", 10, "Anything");
    let provider = RecordingProvider::failing("upstream is down");
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let outcome = run_prelude(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("a failed internal must not fail the turn");

    assert_eq!(outcome.title, None);
    assert!(
        outcome
            .skipped
            .iter()
            .any(|reason| reason.contains("title") && reason.contains("upstream is down")),
        "the failure was swallowed instead of reported: {:?}",
        outcome.skipped
    );
    assert!(title_of(&connection).starts_with("New session"));
}

#[tokio::test]
async fn a_session_with_no_provider_for_the_internals_reports_both_and_runs_neither() {
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    put_user(&connection, "msg_001", 10, "Anything");
    put_assistant(&connection, "msg_002", 20, "An answer", 900);
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let outcome = run_prelude(
        SESSION_ID,
        &mut context(
            &mut connection,
            &NoProvider,
            &internals,
            &config,
            TINY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("an unresolvable internal must not fail the turn");

    assert!(!outcome.compacted);
    assert_eq!(outcome.skipped.len(), 2, "{:?}", outcome.skipped);
}

#[tokio::test]
async fn an_overflowing_session_is_compacted_before_the_turn() {
    // Given: a session whose newest finished assistant message measured 900 tokens
    // against a window whose usable budget is 900.
    let mut connection = seeded("A named session", None);
    put_user(&connection, "msg_001", 10, "First question");
    put_assistant(&connection, "msg_002", 20, "First answer", 10);
    put_user(&connection, "msg_003", 30, "Second question");
    put_assistant(&connection, "msg_004", 40, "Second answer", 20);
    put_user(&connection, "msg_005", 50, "Third question");
    put_assistant(&connection, "msg_006", 60, "Third answer", 900);
    let provider = RecordingProvider::answering(&["## Objective\n- Keep going."]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    // When: the prelude runs.
    let compacted = compact_if_overflowing(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            TINY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("the overflowing session compacts");

    // Then: the summary was requested tool-free, and the next request over this
    // session carries the summary instead of the summarised head.
    assert!(compacted);
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());

    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("history is readable");
    let retained = retained_history(&history);
    assert!(
        retained.len() < history.len(),
        "compaction wrote its summary but the request still carries every message"
    );
    let carried = crate::r#loop::project_history("system", retained)
        .into_iter()
        .map(|projected| text_of(&projected.message))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        carried.contains("Keep going"),
        "the summary is not in the retained history: {carried}"
    );
    assert!(
        !carried.contains("First question"),
        "the summarised head is still being sent: {carried}"
    );
}

#[tokio::test]
async fn a_session_within_its_window_is_not_compacted() {
    let mut connection = seeded("A named session", None);
    put_user(&connection, "msg_001", 10, "A question");
    put_assistant(&connection, "msg_002", 20, "An answer", 10);
    let provider = RecordingProvider::answering(&["never requested"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let compacted = compact_if_overflowing(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("a history that fits is not a failure");

    assert!(
        !compacted,
        "a session inside its window must not be compacted"
    );
    assert!(
        provider.requests().is_empty(),
        "a session that fits must not pay for a summary"
    );
}

#[tokio::test]
async fn an_ordinary_turn_reports_nothing_at_all() {
    // Given: a named session, comfortably inside its window — the ordinary case for
    // every turn after the first.
    let mut connection = seeded("A named session", None);
    put_user(&connection, "msg_001", 10, "A question");
    put_assistant(&connection, "msg_002", 20, "An answer", 10);
    let provider = RecordingProvider::answering(&["never requested"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    // When: the prelude runs.
    let outcome = run_prelude(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("the prelude succeeds");

    // Then: nothing is reported and nothing is spent. A line on every ordinary turn is
    // a line the user learns to ignore, which is how a real loss goes unread.
    assert_eq!(outcome.title, None);
    assert!(!outcome.compacted);
    assert!(
        outcome.skipped.is_empty(),
        "an ordinary turn produced a report: {:?}",
        outcome.skipped
    );
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn the_boundary_this_module_selects_never_splits_a_tool_use_and_its_result() {
    // Given: a transcript whose only tool pair straddles two stored messages, which
    // is the arrangement `select_boundary`'s proptest guards.
    let connection = seeded("A named session", None);
    put_user(&connection, "msg_001", 10, "Read the file");
    put_tool_exchange(&connection, "msg_002", 20, "call_1");
    put_user(&connection, "msg_003", 30, "And now explain it");
    put_assistant(&connection, "msg_004", 40, "It is a main function", 900);
    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("history is readable");

    // When: the boundary is selected over this module's transcript.
    let entries = transcript(COMPACTION_PROMPT, &history);
    let boundary = select_boundary(&entries, 1, 40).expect("a boundary exists");

    // Then: no retained `tool_result` has lost its `tool_use`.
    let retained_uses: Vec<&str> = entries[boundary.retained_from..]
        .iter()
        .flat_map(|entry| &entry.message.content)
        .filter_map(|block| match block {
            RequestContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    for block in entries[boundary.retained_from..]
        .iter()
        .flat_map(|entry| &entry.message.content)
    {
        if let RequestContentBlock::ToolResult { tool_use_id, .. } = block {
            assert!(
                retained_uses.contains(&tool_use_id.as_str()),
                "retained a tool result for `{tool_use_id}` whose tool use was \
                 summarised away; retained uses: {retained_uses:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_failed_compaction_leaves_the_full_history_in_place() {
    let mut connection = seeded("A named session", None);
    put_user(&connection, "msg_001", 10, "First question");
    put_assistant(&connection, "msg_002", 20, "First answer", 10);
    put_user(&connection, "msg_003", 30, "Second question");
    put_assistant(&connection, "msg_004", 40, "Second answer", 20);
    put_user(&connection, "msg_005", 50, "Third question");
    put_assistant(&connection, "msg_006", 60, "Third answer", 900);
    let before = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("history is readable")
        .len();
    let provider = RecordingProvider::failing("the summariser is down");
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let outcome = compact_if_overflowing(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            TINY_WINDOW,
            &mut state,
        ),
    )
    .await;

    assert!(matches!(outcome, Err(CompactionSkipped::Reason(_))));
    let history = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("history is readable");
    assert!(
        history.len() > before,
        "a failed attempt still records its marker and errored summary"
    );
    assert_eq!(
        retained_history(&history).len(),
        history.len(),
        "a failed compaction dropped history and substituted nothing"
    );
}

#[tokio::test]
async fn summarize_issues_one_tool_free_request_carrying_the_summary_agents_prompt() {
    let mut connection = seeded("A named session", None);
    put_user(&connection, "msg_001", 10, "Add pagination");
    put_assistant(&connection, "msg_002", 20, "Added limit and offset", 10);
    let provider = RecordingProvider::answering(&["I added pagination to the users endpoint."]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let summary = summarize(
        SESSION_ID,
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await
    .expect("the summary is generated");

    assert_eq!(summary, "I added pagination to the users endpoint.");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(text_of(&requests[0].messages[0]), SUMMARY_PROMPT);
}

#[test]
fn the_estimator_matches_upstreams_four_characters_per_token_rule() {
    // Given: a message whose serialized form has a known length.
    let message = Message::new(Role::User, "hello");
    let serialized = serde_json::to_string(&message).expect("a message serializes");

    // Then: the estimate is that length rounded to the nearest quarter, which is
    // `Token.estimate` (`core/src/util/token.ts:5`).
    let expected = u32::try_from((serialized.len() + 2) / 4).expect("a small length");
    assert_eq!(estimate_tokens(&message), expected);
}

#[tokio::test]
async fn an_unreadable_session_is_the_one_condition_the_prelude_reports_upward() {
    let mut connection = seeded("New session - 2026-08-07T00:00:00.000Z", None);
    let provider = RecordingProvider::answering(&["unused"]);
    let providers = OneProvider(Arc::clone(&provider));
    let internals = internals();
    let config = CompactionConfig::default();
    let mut state = CompactionState::default();

    let failure = generate_title(
        "ses_does_not_exist",
        &mut context(
            &mut connection,
            &providers,
            &internals,
            &config,
            ROOMY_WINDOW,
            &mut state,
        ),
    )
    .await;

    assert!(matches!(failure, Err(TitleSkipped::Database(_))));
}
