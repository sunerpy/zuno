//! Real engine requests around an out-of-band Memory commit at the foreground wait.

use super::{DynamicContextRefreshInstruction, HostDynamicContextRefresher};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Pool, migration};
use zuno_engine::hooks::TurnHooks;
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, DynamicContextRefresher,
    PreparedToolDispatch, ResolvedAgent, ResolvedModel, RunTurnRequest, ToolDispatchResult,
    ToolDispatcher, TurnContext, TurnEvent, TurnOutcome, event_channel, run_turn,
};
use zuno_engine::status::SessionRunRegistry;
use zuno_goal::{GoalContinuation, GoalStore};
use zuno_llm::cache::McpToolStatus;
use zuno_llm::event::{FinishReason, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_memory::{MemoryProposal, MemoryService, PromotionPolicy, Scope, ScopeLimits, ScopePaths};
use zuno_paths::DbLocation;
use zuno_tool::{
    HistoryPolicy, ToolDefinition, ToolDynamicContextRefresh, ToolOutput, ToolUiIntent,
};
use zuno_types::{MemoryAction, MemoryCandidateStatus, MemoryScope, MemorySource};

const SESSION: &str = "ses_memory_refresh_test";
const OLD_MEMORY: &str = "MEMORY_REFRESH_OLD: prefer verbose test reports.";
const NEW_MEMORY: &str = "MEMORY_REFRESH_NEW: prefer concise test reports.";
const STATIC_PROMPT: &str = "STATIC_REFRESH_TEST: execute the requested probe.";
const FOREGROUND_INSTRUCTION: &str = "FOREGROUND_REFRESH_TEST: preserve the active task.";

#[derive(Debug)]
struct RecordingProvider {
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "memory-refresh-mock"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, request: CompletionRequest) -> ProviderStream<'_> {
        let request_number = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        let events = match request_number {
            1 => vec![
                StreamEvent::ToolUseStart {
                    id: "call-probe".to_owned(),
                    name: "probe".to_owned(),
                },
                StreamEvent::ToolInputDelta {
                    id: "call-probe".to_owned(),
                    delta: "{}".to_owned(),
                },
                StreamEvent::ToolUseEnd {
                    id: "call-probe".to_owned(),
                },
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::ToolCalls),
                },
            ],
            2 => vec![
                StreamEvent::TextDelta("Probe completed.".to_owned()),
                StreamEvent::MessageEnd {
                    stop_reason: Some(FinishReason::Stop),
                },
            ],
            _ => panic!("the test permits exactly two local provider requests"),
        };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }
}

struct TestResolver;

impl AgentModelResolver for TestResolver {
    fn resolve_agent(&self, name: &str) -> Option<ResolvedAgent> {
        (name == "build").then(|| ResolvedAgent::new("build", STATIC_PROMPT))
    }

    fn resolve_model(&self, provider: &str, model: &str) -> Option<ResolvedModel> {
        (provider == "memory-refresh-mock" && model == "memory-refresh-model").then(|| {
            ResolvedModel::new(
                Spec::new("memory-refresh-mock"),
                "memory-refresh-model",
                ApiSurface::Chat,
            )
        })
    }
}

#[derive(Default)]
struct UnmarkedProbe {
    calls: Mutex<usize>,
}

#[async_trait]
impl ToolDispatcher for UnmarkedProbe {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(
            vec![ToolDefinition {
                id: "probe".to_owned(),
                display_name: "Probe".to_owned(),
                description: "A deterministic local probe without a context refresh marker."
                    .to_owned(),
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
                ui_intent: ToolUiIntent::Generic,
                history_policy: HistoryPolicy::ExactDeclaration,
            }],
            McpToolStatus::Ready,
        )
    }

    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        assert_eq!(request.call.name, "probe");
        *self.calls.lock().unwrap() += 1;
        let output = ToolOutput::text("Probe", "Probe completed without changing Memory.");
        assert_eq!(output.dynamic_context_refresh(), None);
        PreparedToolDispatch::ready(ToolDispatchResult::success(output))
    }
}

struct CommitDuringForegroundWait {
    memory: Arc<MemoryService>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
    steps: Mutex<Vec<u32>>,
}

#[async_trait]
impl TurnHooks for CommitDuringForegroundWait {
    async fn before_provider_request(
        &self,
        session_id: &str,
        _turn_id: &str,
        next_step: u32,
    ) -> Result<(), String> {
        assert_eq!(session_id, SESSION);
        self.steps.lock().unwrap().push(next_step);
        if next_step == 2 {
            assert_eq!(self.requests.lock().unwrap().len(), 1);
            let memory = Arc::clone(&self.memory);
            // An independent task commits while foreground waiting is active.
            // Refreshing before this hook would still observe the old revision.
            tokio::task::spawn_blocking(move || {
                commit_memory(&memory, Some(OLD_MEMORY), NEW_MEMORY)
            })
            .await
            .map_err(|error| error.to_string())??;
        }
        Ok(())
    }
}

fn commit_memory(memory: &MemoryService, old: Option<&str>, content: &str) -> Result<(), String> {
    let candidate = memory
        .propose(MemoryProposal {
            scope: MemoryScope::Project,
            action: if old.is_some() {
                MemoryAction::Replace
            } else {
                MemoryAction::Add
            },
            content: Some(content.to_owned()),
            old_text: old.map(str::to_owned),
            reason: "An external user-owned Memory update.".to_owned(),
            confidence: 1.0,
            source: MemorySource::User,
            source_session_id: Some(SESSION.to_owned()),
            source_message_id: None,
        })
        .map_err(|error| error.to_string())?;
    match candidate.projection.status {
        MemoryCandidateStatus::Applied => Ok(()),
        MemoryCandidateStatus::Pending => memory
            .apply(candidate.id())
            .map(|_| ())
            .map_err(|error| error.to_string()),
        status => Err(format!("the fixture memory commit ended as {status:?}")),
    }
}

#[derive(Clone, Copy)]
enum RefreshMode {
    Host,
    FrozenControl,
}

async fn capture_requests(mode: RefreshMode) -> Vec<CompletionRequest> {
    let directory = tempfile::tempdir().unwrap();
    let pool = Arc::new(Pool::open(&DbLocation::Memory).unwrap());
    let mut connection = pool.open_connection().unwrap();
    migration::apply(&mut connection).unwrap();
    connection
        .execute_batch(&format!(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
         VALUES('p','/workspace',1,1,'[]');
         INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
         VALUES('{SESSION}','p','refresh','/workspace','Memory refresh','test',1,1);"
        ))
        .unwrap();
    let store = MessageStore::new(&connection);
    store.put_message_at(&MessageRecord::from_json(json!({
        "id":"msg_refresh_user","sessionID":SESSION,"role":"user","time":{"created":10},
        "agent":"build","model":{"providerID":"memory-refresh-mock","modelID":"memory-refresh-model"}
    })).unwrap(), 10).unwrap();
    store
        .put_part_at(
            &PartRecord::from_json(
                json!({
                    "id":"prt_refresh_user","sessionID":SESSION,"messageID":"msg_refresh_user",
                    "type":"text","text":"Run the probe and report its result."
                }),
                10,
            )
            .unwrap(),
            10,
        )
        .unwrap();
    let memory = Arc::new(MemoryService::new(
        pool.clone(),
        ScopePaths::at(
            directory.path().join("global.md"),
            directory.path().join("project.md"),
        ),
        ScopeLimits::default(),
        PromotionPolicy::Automatic,
    ));
    memory.reconcile().unwrap();
    commit_memory(&memory, None, OLD_MEMORY).unwrap();
    let before_revision = memory.snapshot(Scope::Project).unwrap().revision;
    let refresher = HostDynamicContextRefresher {
        goal_continuation: GoalContinuation::new(
            Arc::new(GoalStore::from_pool(pool.clone(), directory.path().join("spill")).unwrap()),
            SessionRunRegistry::new(),
        ),
        instruction: DynamicContextRefreshInstruction::Fixed("Keep the current task.".to_owned()),
        memory: Some(memory.clone()),
        memory_policy_store: zuno_db::session_memory_policy::SessionMemoryPolicyStore::new(
            pool.clone(),
        ),
        memory_default_use: true,
        memory_allowed: true,
        foreground_instruction: Arc::new(Mutex::new(Some(FOREGROUND_INSTRUCTION.to_owned()))),
    };
    // Both cases begin with a real snapshot of the old Memory, just as a host
    // does at turn start. Only before_request can observe the later commit.
    let initial = refresher
        .refresh(&connection, SESSION, ToolDynamicContextRefresh::WorkItems)
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingProvider {
        requests: requests.clone(),
    });
    let mut registry = ProviderRegistry::new();
    registry.register("memory-refresh-mock", move |_| provider.clone());
    let hooks = Arc::new(CommitDuringForegroundWait {
        memory: memory.clone(),
        requests: requests.clone(),
        steps: Mutex::new(Vec::new()),
    });
    let dispatcher = UnmarkedProbe::default();
    let resolver = TestResolver;
    let interrupt = InterruptSignal::new();
    let mut context = TurnContext::new(
        &mut connection,
        &registry,
        &resolver,
        &dispatcher,
        &interrupt,
    )
    .with_hooks(hooks.clone());
    if matches!(mode, RefreshMode::Host) {
        context = context.with_dynamic_context_refresher(&refresher);
    }
    let (sender, mut receiver) = event_channel();
    let (outcome, events) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            run_turn(
                RunTurnRequest::new(SESSION, "memory-refresh-turn", initial),
                context,
                sender
            ),
            async {
                let mut events = Vec::new();
                while let Some(event) = receiver.recv().await {
                    events.push(event);
                }
                events
            }
        )
    })
    .await
    .expect("the local scripted turn must finish promptly");
    assert!(matches!(
        outcome.unwrap(),
        TurnOutcome::Completed { steps: 2, .. }
    ));
    assert_eq!(*hooks.steps.lock().unwrap(), vec![1, 2]);
    assert_eq!(*dispatcher.calls.lock().unwrap(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TurnEvent::TurnStarted { .. }))
            .count(),
        1
    );
    assert!(memory.snapshot(Scope::Project).unwrap().revision > before_revision);
    assert_eq!(
        memory
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>(),
        vec![NEW_MEMORY]
    );
    let counts: (i64, i64) = connection.query_row(
        "SELECT (SELECT count(*) FROM message WHERE session_id=?1 AND json_extract(data,'$.role')='user'),
                (SELECT count(*) FROM session_input WHERE session_id=?1)",
        [SESSION], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(
        counts,
        (1, 0),
        "refresh must not manufacture foreground input"
    );
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let static_and_history = serde_json::to_string(&request.messages).unwrap();
        assert_eq!(static_and_history.matches(STATIC_PROMPT).count(), 1);
        assert!(!static_and_history.contains(OLD_MEMORY));
        assert!(!static_and_history.contains(NEW_MEMORY));
        assert_eq!(
            request
                .developer_context
                .join("\n")
                .matches(FOREGROUND_INSTRUCTION)
                .count(),
            1
        );
    }
    requests
}

#[tokio::test]
async fn host_memory_refresh_reaches_next_provider_request_after_background_commit_without_tool_marker()
 {
    let requests = capture_requests(RefreshMode::Host).await;
    let first = requests[0].developer_context.join("\n");
    let second = requests[1].developer_context.join("\n");
    assert_eq!(first.matches(OLD_MEMORY).count(), 1);
    assert!(!first.contains(NEW_MEMORY));
    assert_eq!(
        second.matches(NEW_MEMORY).count(),
        1,
        "the second provider request must use the committed revision"
    );
    assert!(
        !second.contains(OLD_MEMORY),
        "the old snapshot must not survive as a duplicate"
    );
}

#[tokio::test]
async fn frozen_memory_refresh_control_detects_the_same_commit_without_a_request_refresher() {
    let requests = capture_requests(RefreshMode::FrozenControl).await;
    let second = requests[1].developer_context.join("\n");
    assert_eq!(second.matches(OLD_MEMORY).count(), 1);
    assert!(
        !second.contains(NEW_MEMORY),
        "the control must expose the original stale-snapshot behavior"
    );
}
