use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use proptest::prelude::*;
use serde_json::json;
use zuno_config::schema::CompactionConfig;
use zuno_db::message::{MessageStore, PartKind};
use zuno_db::{Connection, migration, open};
use zuno_engine::compaction::{
    AutoContinueHookInput, CompactedTranscript, CompactionCache, CompactionHookInput,
    CompactionHooks, CompactionOutcome, CompactionPolicy, CompactionPrompt, CompactionRequest,
    CompactionState, CompactionStopReason, CompactionTrigger, TokenWindow, TranscriptEntry,
    run_compaction, select_boundary,
};
use zuno_error::{ProviderError, Recovery};
use zuno_llm::cache::{CacheTracker, LockedTools, McpToolStatus, StaticSystemPrompt};
use zuno_llm::event::{Message, RequestContentBlock, Role, StreamEvent};
use zuno_llm::registry::{Capabilities, CompletionRequest, Provider, ProviderStream};

const SESSION_ID: &str = "ses_compaction_test";
const SUMMARY: &str = "## Objective\n- Finish the Rust compactor.\n\n## Important Details\n- Never split tool pairs.\n\n## Work State\n### Completed\n- Boundary selected.\n\n### Active\n- Compaction tests.\n\n### Blocked\n- (none)\n\n## Next Move\n1. Run the next turn.\n2. Verify the result.\n\n## Relevant Files\n- crates/zuno-engine/src/compaction.rs: compactor.";

#[derive(Debug)]
struct CassetteProvider {
    responses: Mutex<VecDeque<Vec<Result<StreamEvent, ProviderError>>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl CassetteProvider {
    fn new(responses: Vec<Vec<Result<StreamEvent, ProviderError>>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("request lock").clone()
    }
}

impl Provider for CassetteProvider {
    fn id(&self) -> &str {
        "cassette"
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
            .expect("one cassette response per request");
        Box::pin(stream::iter(events))
    }
}

#[derive(Debug)]
struct RecordingHooks {
    auto_continue: bool,
    compacting_calls: Mutex<Vec<String>>,
    auto_continue_calls: Mutex<Vec<String>>,
}

impl RecordingHooks {
    fn new(auto_continue: bool) -> Self {
        Self {
            auto_continue,
            compacting_calls: Mutex::new(Vec::new()),
            auto_continue_calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl CompactionHooks for RecordingHooks {
    async fn compacting(
        &self,
        input: &CompactionHookInput<'_>,
        output: &mut CompactionPrompt,
    ) -> Result<(), String> {
        self.compacting_calls
            .lock()
            .expect("compacting calls lock")
            .push(input.session_id.to_owned());
        output
            .context
            .push("Plugin context: preserve the accepted boundary.".to_owned());
        Ok(())
    }

    async fn auto_continue(&self, input: &AutoContinueHookInput<'_>) -> Result<bool, String> {
        self.auto_continue_calls
            .lock()
            .expect("auto-continue calls lock")
            .push(input.session_id.to_owned());
        Ok(self.auto_continue)
    }
}

fn seeded() -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open memory database");
    migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-compaction', '/workspace', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-compaction', 'compaction', '/workspace', \
               'compaction', '1', 1, 1);"
        ))
        .expect("seed project and session");
    connection
}

fn entry(
    id: impl Into<String>,
    role: Role,
    text: impl Into<String>,
    tokens: u32,
) -> TranscriptEntry {
    TranscriptEntry::new(id, Message::new(role, text), tokens)
}

fn tool_use(id: impl Into<String>, call_id: impl Into<String>, tokens: u32) -> TranscriptEntry {
    let call_id = call_id.into();
    TranscriptEntry::new(
        id,
        Message::from_content(
            Role::Assistant,
            vec![RequestContentBlock::ToolUse {
                id: call_id,
                name: "read".to_owned(),
                input: json!({ "filePath": "README.md" }),
                thought_signature: None,
            }],
        ),
        tokens,
    )
}

fn tool_result(id: impl Into<String>, call_id: impl Into<String>, tokens: u32) -> TranscriptEntry {
    TranscriptEntry::new(
        id,
        Message::from_content(
            Role::Tool,
            vec![RequestContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: "file contents".to_owned(),
                is_error: Some(false),
            }],
        ),
        tokens,
    )
}

fn pair_ids(entries: &[TranscriptEntry]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut uses = BTreeSet::new();
    let mut results = BTreeSet::new();
    for entry in entries {
        for block in &entry.message.content {
            match block {
                RequestContentBlock::ToolUse { id, .. } => {
                    uses.insert(id.clone());
                }
                RequestContentBlock::ToolResult { tool_use_id, .. } => {
                    results.insert(tool_use_id.clone());
                }
                RequestContentBlock::Text { .. }
                | RequestContentBlock::SignedThinking { .. }
                | RequestContentBlock::ProviderEncryptedReasoning { .. }
                | RequestContentBlock::Image { .. } => {}
            }
        }
    }
    (uses, results)
}

fn valid_transcript(tool_turns: &[bool]) -> Vec<TranscriptEntry> {
    let mut entries = vec![entry("system", Role::System, "Initial context", 1)];
    for (turn, uses_tool) in tool_turns.iter().copied().enumerate() {
        entries.push(entry(
            format!("user-{turn}"),
            Role::User,
            format!("request {turn}"),
            (turn % 7 + 1) as u32,
        ));
        if uses_tool {
            entries.push(tool_use(
                format!("assistant-tool-{turn}"),
                format!("call-{turn}"),
                (turn % 5 + 1) as u32,
            ));
            entries.push(tool_result(
                format!("tool-{turn}"),
                format!("call-{turn}"),
                (turn % 11 + 1) as u32,
            ));
        }
        entries.push(entry(
            format!("assistant-{turn}"),
            Role::Assistant,
            format!("answer {turn}"),
            (turn % 3 + 1) as u32,
        ));
    }
    entries
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn compaction_boundary_never_splits_a_tool_use_and_result_pair(
        tool_turns in prop::collection::vec(any::<bool>(), 1..48),
        tail_turns in 0_u32..12,
        preserve_recent_tokens in 1_u32..256,
    ) {
        let entries = valid_transcript(&tool_turns);
        if let Some(boundary) = select_boundary(&entries, tail_turns, preserve_recent_tokens) {
            let retained = &entries[boundary.retained_from..];
            let (uses, results) = pair_ids(retained);
            prop_assert_eq!(uses, results);
            prop_assert!(boundary.retained_from >= boundary.initial_context_end);
        }
    }
}

#[test]
fn compaction_boundary_walks_back_when_the_raw_split_lands_on_a_tool_result() {
    let entries = vec![
        entry("system", Role::System, "Initial context", 1),
        entry("user-old", Role::User, "Inspect the file", 40),
        tool_use("assistant-tool", "call-read", 10),
        tool_result("tool-result", "call-read", 10),
        entry("user-recent", Role::User, "What next?", 10),
        entry("assistant-recent", Role::Assistant, "Run tests", 10),
    ];

    let boundary = select_boundary(&entries, 2, 30).expect("there is old history to summarize");

    assert_eq!(
        boundary.raw_retained_from, 3,
        "raw split starts at ToolResult"
    );
    assert_eq!(boundary.retained_from, 2, "boundary walks back to ToolUse");
    assert_eq!(
        pair_ids(&entries[boundary.retained_from..]).0,
        BTreeSet::from(["call-read".to_owned()])
    );
    assert_eq!(
        pair_ids(&entries[boundary.retained_from..]).1,
        BTreeSet::from(["call-read".to_owned()])
    );
    eprintln!(
        "FAILURE_QA transcript=[system,user-old,tool_use(call-read),tool_result(call-read),user-recent,assistant-recent] raw_boundary={} adjusted_boundary={}",
        boundary.raw_retained_from, boundary.retained_from
    );
}

#[test]
fn compaction_policy_honors_all_five_configuration_fields() {
    let config = CompactionConfig {
        auto: Some(false),
        prune: Some(true),
        tail_turns: Some(7),
        preserve_recent_tokens: Some(3_456),
        reserved: Some(12_000),
    };
    let policy = CompactionPolicy::resolve(
        &config,
        TokenWindow {
            context: 100_000,
            max_output: 8_000,
        },
    );

    assert!(!policy.auto);
    assert!(policy.prune);
    assert_eq!(policy.tail_turns, 7);
    assert_eq!(policy.preserve_recent_tokens, 3_456);
    assert_eq!(policy.reserved, 12_000);
    assert_eq!(policy.usable_tokens, 88_000);
    assert!(!policy.should_compact(CompactionTrigger::Threshold {
        used_tokens: 99_999
    }));
    assert!(policy.should_compact(CompactionTrigger::ContextLimit {
        used_tokens: Some(100_001),
        limit_tokens: Some(100_000),
    }));
}

#[tokio::test]
async fn compaction_summarizes_two_hundred_messages_with_the_small_model_and_resets_cache() {
    let mut connection = seeded();
    let mut entries = vec![entry("system", Role::System, "Initial project context", 20)];
    for turn in 0..100 {
        entries.push(entry(
            format!("user-{turn:03}"),
            Role::User,
            format!("user request {turn}"),
            20,
        ));
        entries.push(entry(
            format!("assistant-{turn:03}"),
            Role::Assistant,
            format!("assistant response {turn}"),
            20,
        ));
    }
    entries.pop();
    assert_eq!(entries.len(), 200);

    let provider = Arc::new(CassetteProvider::new(vec![
        vec![
            Ok(StreamEvent::TextDelta(SUMMARY.to_owned())),
            Ok(StreamEvent::MessageEnd { stop_reason: None }),
        ],
        vec![
            Ok(StreamEvent::TextDelta("next turn succeeded".to_owned())),
            Ok(StreamEvent::MessageEnd { stop_reason: None }),
        ],
    ]));
    let hooks = RecordingHooks::new(true);
    let config = CompactionConfig {
        auto: Some(true),
        prune: Some(false),
        tail_turns: Some(2),
        preserve_recent_tokens: Some(200),
        reserved: Some(20_000),
    };
    let request = CompactionRequest::new(
        SESSION_ID,
        "happy",
        "build",
        "cassette",
        "small-cassette-model",
        entries.clone(),
        &config,
        TokenWindow {
            context: 120_000,
            max_output: 4_096,
        },
        CompactionTrigger::Threshold {
            used_tokens: 110_000,
        },
    );
    let mut state = CompactionState::default();
    let mut tracker = CacheTracker::new();
    tracker
        .record(
            &StaticSystemPrompt::new("Initial project context"),
            &[Message::new(Role::User, "baseline")],
        )
        .expect("establish cache baseline");
    let mut locked_tools = LockedTools::new();
    let first = locked_tools.tools_for_request(&["read".to_owned()], McpToolStatus::Ready);
    assert_eq!(first.tools(), ["read"]);

    let outcome = {
        let mut cache = CompactionCache::new(&mut tracker, &mut locked_tools);
        run_compaction(
            &mut connection,
            provider.as_ref(),
            &hooks,
            &mut state,
            &mut cache,
            request,
        )
        .await
        .expect("compaction succeeds")
    };
    let CompactionOutcome::Compacted(CompactedTranscript {
        summary,
        messages,
        boundary,
        auto_continue,
        ..
    }) = outcome
    else {
        panic!("threshold should compact");
    };
    assert_eq!(summary, SUMMARY);
    assert!(auto_continue);
    assert!(boundary.retained_from > boundary.initial_context_end);
    assert_eq!(
        messages.first().expect("initial context").role,
        Role::System
    );
    assert!(messages.iter().any(|message| message.role == Role::User));
    assert_eq!(
        tracker.turn(),
        0,
        "cache tracker resets after durable summary"
    );
    let relocked = locked_tools.tools_for_request(&["goal".to_owned()], McpToolStatus::Ready);
    assert_eq!(relocked.tools(), ["goal"], "locked tool list was cleared");
    assert_eq!(locked_tools.rebuild_count(), 0);

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model_id, "small-cassette-model");
    let prompt = requests[0]
        .messages
        .last()
        .expect("summary prompt message")
        .content
        .iter()
        .find_map(|block| match block {
            RequestContentBlock::Text { text } => Some(text.as_str()),
            RequestContentBlock::SignedThinking { .. }
            | RequestContentBlock::ProviderEncryptedReasoning { .. }
            | RequestContentBlock::ToolUse { .. }
            | RequestContentBlock::ToolResult { .. }
            | RequestContentBlock::Image { .. } => None,
        })
        .expect("text summary prompt");
    for section in [
        "## Objective",
        "## Important Details",
        "## Work State",
        "## Next Move",
        "## Relevant Files",
    ] {
        assert!(prompt.contains(section), "missing prompt section {section}");
    }
    assert!(prompt.contains("Plugin context: preserve the accepted boundary."));

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate compaction records");
    assert!(
        hydrated
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| {
                part.kind == PartKind::Compaction
                    && part.data["tail_start_id"] == entries[boundary.retained_from].id
            })
    );
    assert!(hydrated.iter().any(|message| {
        message.info.role.as_str() == "assistant"
            && message.info.data["summary"] == true
            && message
                .parts
                .iter()
                .any(|part| part.data["text"] == SUMMARY)
    }));

    let mut next_messages = messages;
    next_messages.push(Message::new(Role::User, "Continue with the next step."));
    let mut next = provider.stream(CompletionRequest::new("main-cassette-model", next_messages));
    let mut next_text = String::new();
    while let Some(event) = next.next().await {
        if let StreamEvent::TextDelta(delta) = event.expect("next cassette event") {
            next_text.push_str(&delta);
        }
    }
    assert_eq!(next_text, "next turn succeeded");
    eprintln!(
        "HAPPY_QA transcript_messages={} summary_model=small-cassette-model retained_from={} next_turn={next_text}",
        entries.len(),
        boundary.retained_from
    );
}

#[tokio::test]
async fn compaction_failure_marks_the_summary_message_and_never_reenters() {
    let mut connection = seeded();
    let entries = valid_transcript(&[false, true, false, true]);
    let provider = CassetteProvider::new(vec![vec![Err(ProviderError::Fatal {
        status: Some(400),
        source: None,
    })]]);
    let hooks = RecordingHooks::new(true);
    let config = CompactionConfig {
        auto: Some(true),
        tail_turns: Some(1),
        preserve_recent_tokens: Some(20),
        reserved: Some(20_000),
        ..CompactionConfig::default()
    };
    let request = CompactionRequest::new(
        SESSION_ID,
        "failure",
        "build",
        "cassette",
        "small-cassette-model",
        entries.clone(),
        &config,
        TokenWindow {
            context: 100_000,
            max_output: 4_096,
        },
        CompactionTrigger::ContextLimit {
            used_tokens: Some(101_000),
            limit_tokens: Some(100_000),
        },
    );
    let mut state = CompactionState::default();
    let mut tracker = CacheTracker::new();
    let mut locked_tools: LockedTools<String> = LockedTools::new();
    let mut cache = CompactionCache::new(&mut tracker, &mut locked_tools);

    let first = run_compaction(
        &mut connection,
        &provider,
        &hooks,
        &mut state,
        &mut cache,
        request.clone(),
    )
    .await
    .expect("provider failure is converted to a terminal outcome");
    assert!(matches!(
        first,
        CompactionOutcome::Stopped {
            reason: CompactionStopReason::Provider,
            recovery: Recovery::Fail,
            ..
        }
    ));
    assert!(state.is_failed());
    assert_eq!(state.context_limit_attempts(), 1);

    let second = run_compaction(
        &mut connection,
        &provider,
        &hooks,
        &mut state,
        &mut cache,
        request,
    )
    .await
    .expect("latched failure stops without touching the provider");
    assert!(matches!(
        second,
        CompactionOutcome::Stopped {
            reason: CompactionStopReason::AlreadyFailed,
            recovery: Recovery::Fail,
            ..
        }
    ));
    assert_eq!(
        provider.requests().len(),
        1,
        "failed compaction is not re-entered"
    );

    let hydrated = MessageStore::new(&connection)
        .hydrate_session(SESSION_ID)
        .expect("hydrate failure marker");
    let failed = hydrated
        .iter()
        .find(|message| {
            message
                .info
                .data
                .get("summary")
                .is_some_and(|summary| summary == true)
        })
        .expect("summary assistant marks the failure");
    assert_eq!(failed.info.data["finish"], "error");
    assert_eq!(failed.info.data["error"]["name"], "CompactionError");
    assert!(
        failed.info.data["error"]["data"]["message"]
            .as_str()
            .expect("failure message")
            .contains("unrecoverable provider failure")
    );
    assert_eq!(tracker.turn(), 0);
    assert!(
        hooks
            .auto_continue_calls
            .lock()
            .expect("auto-continue calls lock")
            .is_empty()
    );
    eprintln!(
        "FAILURE_QA provider_requests={} session_mark=CompactionError second_attempt=AlreadyFailed",
        provider.requests().len()
    );
}
