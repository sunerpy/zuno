//! G3/G4 long-run growth and liveness gates.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream;
use serde_json::json;
use zuno_catalog::lsp_config::ResolvedLsp;
use zuno_config::schema::CompactionConfig;
use zuno_config::schema::lsp::{BUILTIN_SERVER_IDS, LspConfig};
use zuno_db::message::{MessageRecord, MessageStore, PartRecord};
use zuno_db::{Connection, migration, open};
use zuno_engine::compaction::{
    CompactionCache, CompactionOutcome, CompactionRequest, CompactionState, CompactionTrigger,
    NoopCompactionHooks, TokenWindow, TranscriptEntry, run_compaction,
};
use zuno_engine::interrupt::InterruptSignal;
use zuno_engine::r#loop::{
    AgentModelResolver, AvailableTools, DispatchRequest, PreparedToolDispatch, ResolvedAgent,
    ResolvedModel, RunTurnRequest, ToolDispatchResult, ToolDispatcher, TurnContext, TurnError,
    TurnEvent, TurnOutcome, event_channel, hydrate_retained_history,
    project_history_owned_with_ids, run_turn,
};
use zuno_error::ProviderError;
use zuno_llm::cache::{CacheTracker, DynamicContext, LockedTools, McpToolStatus};
use zuno_llm::event::{PromptAccounting, StreamEvent};
use zuno_llm::registry::{
    ApiSurface, Capabilities, CompletionRequest, Provider, ProviderRegistry, ProviderStream, Spec,
};
use zuno_lsp::{Manager, RestartPolicy, ServerRegistry, ServerState};
use zuno_paths::Env;
use zuno_provider_openai::OpenAiDecoder;
use zuno_pty::{CreateInput, PtyService};
use zuno_testkit::perf::{
    FrozenThresholds, create_watcher_tree, load_committed_baseline, sample_process_tree,
};
use zuno_testkit::{CassettePlayer, HttpInteraction};
use zuno_tool::{ToolDefinition, ToolOutput};
use zuno_watch::{WatchOptions, Watcher};

const SESSION_ID: &str = "ses_g3_g4_soak";
const SOAK_TURNS: usize = 500;
const MIN_SOAK_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const TOOL_OUTPUT_BYTES: usize = 50 * 1024 * 1024;
const PTY_OUTPUT_BYTES: u64 = 100 * 1024 * 1024;
const SYSTEM_PROMPT: &str = "Use the available tool when requested, then answer concisely.";
const CHAT_MODEL: &str = "gpt-4o-mini";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RssAtTurn {
    turn: usize,
    rss_kib: u64,
}

#[derive(Debug, Clone, Copy)]
struct GrowthVerdict {
    slope_mib_per_turn: f64,
    max_slope_mib_per_turn: f64,
    peak_ratio: f64,
    max_peak_ratio: f64,
    window_first_turn: usize,
    window_last_turn: usize,
    window_samples: usize,
}

impl GrowthVerdict {
    fn evaluate(samples: &[RssAtTurn], thresholds: FrozenThresholds) -> Result<Self, String> {
        if samples.len() < 60 {
            return Err(format!(
                "G3 needs samples through turn 60, received {}",
                samples.len()
            ));
        }
        if samples.windows(2).any(|pair| pair[0].turn >= pair[1].turn) {
            return Err("G3 RSS sample turns must be strictly increasing".to_owned());
        }

        let final_half = &samples[samples.len() / 2..];
        let mut slopes = Vec::with_capacity(final_half.len() * (final_half.len() - 1) / 2);
        for (index, left) in final_half.iter().enumerate() {
            for right in &final_half[index + 1..] {
                let delta_turns = (right.turn - left.turn) as f64;
                let delta_mib = (right.rss_kib as f64 - left.rss_kib as f64) / 1024.0;
                slopes.push(delta_mib / delta_turns);
            }
        }
        slopes.sort_by(f64::total_cmp);
        let slope_mib_per_turn = median(&slopes);

        let final_tenth_start = samples.len() * 9 / 10;
        let final_peak = samples[final_tenth_start..]
            .iter()
            .map(|sample| sample.rss_kib)
            .max()
            .expect("the final tenth is non-empty");
        let middle_peak = samples
            .iter()
            .filter(|sample| (40..=60).contains(&sample.turn))
            .map(|sample| sample.rss_kib)
            .max()
            .ok_or_else(|| "G3 requires RSS samples for turns 40-60".to_owned())?;
        let peak_ratio = if middle_peak == 0 {
            f64::INFINITY
        } else {
            final_peak as f64 / middle_peak as f64
        };

        let verdict = Self {
            slope_mib_per_turn,
            max_slope_mib_per_turn: thresholds.g3_max_mib_per_turn,
            peak_ratio,
            max_peak_ratio: thresholds.g3_max_peak_ratio,
            window_first_turn: final_half.first().expect("non-empty final half").turn,
            window_last_turn: final_half.last().expect("non-empty final half").turn,
            window_samples: final_half.len(),
        };
        if slope_mib_per_turn > thresholds.g3_max_mib_per_turn
            || peak_ratio > thresholds.g3_max_peak_ratio
        {
            return Err(format!(
                "G3 failed for turns {}..{} ({} samples): Theil-Sen RSS slope \
                 {slope_mib_per_turn:.3} MiB/turn (maximum {:.3}); final/middle \
                 peak ratio {peak_ratio:.3} (maximum {:.3})",
                verdict.window_first_turn,
                verdict.window_last_turn,
                verdict.window_samples,
                thresholds.g3_max_mib_per_turn,
                thresholds.g3_max_peak_ratio
            ));
        }
        Ok(verdict)
    }
}

fn median(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

#[derive(Debug, Clone, Copy)]
struct WatchdogLimits {
    progress_timeout: Duration,
    hard_deadline: Duration,
}

#[derive(Debug)]
struct LivenessWatchdog {
    limits: WatchdogLimits,
    last_progress: Duration,
}

impl LivenessWatchdog {
    fn new(limits: WatchdogLimits) -> Self {
        Self {
            limits,
            last_progress: Duration::ZERO,
        }
    }

    fn progress(&mut self, elapsed: Duration) {
        self.last_progress = elapsed;
    }

    fn check(&self, elapsed: Duration) -> Result<(), String> {
        if elapsed > self.limits.hard_deadline {
            return Err(format!(
                "G4 hard deadline exceeded: {} ms elapsed, maximum {} ms",
                elapsed.as_millis(),
                self.limits.hard_deadline.as_millis()
            ));
        }
        let silence = elapsed.saturating_sub(self.last_progress);
        if silence > self.limits.progress_timeout {
            return Err(format!(
                "G4 progress timeout exceeded: {} ms without state progress, maximum {} ms",
                silence.as_millis(),
                self.limits.progress_timeout.as_millis()
            ));
        }
        Ok(())
    }

    fn next_check_after(&self, elapsed: Duration) -> Duration {
        let hard = self.limits.hard_deadline.saturating_sub(elapsed);
        let silence = elapsed.saturating_sub(self.last_progress);
        let progress = self.limits.progress_timeout.saturating_sub(silence);
        hard.min(progress).saturating_add(Duration::from_millis(1))
    }
}

type TokenUsage = (Option<u64>, Option<u64>, Option<u64>, Option<u64>);

#[derive(Debug, Default)]
struct ProgressTracker {
    messages: HashSet<String>,
    checkpoints: HashSet<(String, bool)>,
    tool_states: HashMap<String, u8>,
    tool_results: HashSet<String>,
    usage: Option<TokenUsage>,
    repaired: usize,
}

impl ProgressTracker {
    fn observe(&mut self, event: &TurnEvent) -> bool {
        match event {
            TurnEvent::HistoryRepaired {
                repaired_tool_results,
            } if *repaired_tool_results > self.repaired => {
                self.repaired = *repaired_tool_results;
                true
            }
            TurnEvent::AssistantMessageCreated { message_id, .. } => {
                self.messages.insert(message_id.clone())
            }
            TurnEvent::AssistantCheckpointed {
                message_id,
                interrupted,
                ..
            } => self.checkpoints.insert((message_id.clone(), *interrupted)),
            TurnEvent::ToolDispatchStarted { call_id, .. } => {
                set_tool_state(&mut self.tool_states, call_id, 1)
            }
            TurnEvent::ToolDispatchCompleted { call_id, .. } => {
                set_tool_state(&mut self.tool_states, call_id, 2)
            }
            TurnEvent::ToolResultAppended { call_id, .. } => {
                self.tool_results.insert(call_id.clone())
            }
            TurnEvent::Provider {
                event:
                    StreamEvent::TokenUsage {
                        input_tokens,
                        output_tokens,
                        cache_read_input_tokens,
                        cache_write_input_tokens,
                        accounting: PromptAccounting::CacheInsideInput,
                    },
                ..
            } => {
                let usage = (
                    *input_tokens,
                    *output_tokens,
                    *cache_read_input_tokens,
                    *cache_write_input_tokens,
                );
                if self.usage == Some(usage) {
                    false
                } else {
                    self.usage = Some(usage);
                    true
                }
            }
            _ => false,
        }
    }
}

fn set_tool_state(states: &mut HashMap<String, u8>, call_id: &str, next: u8) -> bool {
    let state = states.entry(call_id.to_owned()).or_default();
    if *state >= next {
        false
    } else {
        *state = next;
        true
    }
}

#[derive(Debug)]
struct CassetteProvider {
    responses: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl CassetteProvider {
    fn new(responses: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

impl Provider for CassetteProvider {
    fn id(&self) -> &str {
        "cassette"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calls: true,
            ..Capabilities::text_only()
        }
    }

    fn stream(&self, _request: CompletionRequest) -> ProviderStream<'_> {
        let events = self
            .responses
            .lock()
            .expect("cassette response lock")
            .pop_front()
            .expect("one cassette response per provider request");
        Box::pin(stream::iter(events.into_iter().map(Ok::<_, ProviderError>)))
    }
}

#[derive(Debug, Clone, Copy)]
struct SoakResolver;

impl AgentModelResolver for SoakResolver {
    fn resolve_agent(&self, requested: &str) -> Option<ResolvedAgent> {
        (requested == "build").then(|| ResolvedAgent::new("build", SYSTEM_PROMPT))
    }

    fn resolve_model(&self, provider_id: &str, model_id: &str) -> Option<ResolvedModel> {
        (provider_id == "cassette" && model_id == CHAT_MODEL)
            .then(|| ResolvedModel::new(Spec::new("cassette"), CHAT_MODEL, ApiSurface::Chat))
    }
}

#[derive(Debug)]
struct LargeOutputDispatcher {
    output: String,
    calls: AtomicUsize,
}

async fn monitor_turn<F>(
    future: F,
    mut events: tokio::sync::mpsc::Receiver<TurnEvent>,
    limits: WatchdogLimits,
    peak_rss_kib: &mut u64,
) -> Result<TurnOutcome, String>
where
    F: Future<Output = Result<TurnOutcome, TurnError>>,
{
    let started = Instant::now();
    let mut watchdog = LivenessWatchdog::new(limits);
    let mut progress = ProgressTracker::default();
    let mut outcome = None;
    let mut samples = tokio::time::interval(SAMPLE_INTERVAL);
    samples.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(future);

    loop {
        let elapsed = started.elapsed();
        watchdog.check(elapsed)?;
        let check_after = watchdog.next_check_after(elapsed);
        tokio::select! {
            result = &mut future, if outcome.is_none() => {
                outcome = Some(result.map_err(|error| error.to_string())?);
            }
            event = events.recv() => {
                match event {
                    Some(event) if progress.observe(&event) => watchdog.progress(started.elapsed()),
                    Some(_) => {}
                    None => return outcome.ok_or_else(|| {
                        "turn event channel closed before the turn returned".to_owned()
                    }),
                }
            }
            _ = samples.tick() => {
                *peak_rss_kib = (*peak_rss_kib).max(current_process_tree_rss());
            }
            _ = tokio::time::sleep(check_after) => {
                watchdog.check(started.elapsed())?;
            }
        }
    }
}

fn current_process_tree_rss() -> u64 {
    sample_process_tree(std::process::id(), Instant::now())
        .expect("sample G3 process tree")
        .total_rss_kib
}

fn decode_chat_interaction(interaction: &HttpInteraction) -> Vec<StreamEvent> {
    assert_eq!(interaction.response.status, 200, "cassette response status");
    assert!(
        interaction.response.is_sse(),
        "cassette response must be SSE"
    );
    let mut decoder = OpenAiDecoder::new("openai", CHAT_MODEL, ApiSurface::Chat);
    let mut events = Vec::new();
    for chunk in interaction.response.body.as_bytes().chunks(17) {
        events.extend(decoder.push(chunk));
    }
    events.extend(decoder.finish());
    events
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("decode committed OpenAI cassette")
}

fn cassette_responses(name: &str) -> Vec<Vec<StreamEvent>> {
    let mut player = CassettePlayer::from_oracle(name).expect("load committed cassette");
    let mut responses = Vec::new();
    while player.remaining() > 0 {
        responses.push(decode_chat_interaction(
            player.next_unchecked().expect("next cassette interaction"),
        ));
    }
    player.finish().expect("consume committed cassette");
    responses
}

fn seed_connection(project: &Path) -> Connection {
    let mut connection = open::open(&zuno_paths::DbLocation::Memory).expect("open soak database");
    migration::apply(&mut connection).expect("apply soak schema");
    let project = project.to_string_lossy().replace('\'', "''");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('project-soak', '{project}', 1, 1, '[]'); \
             INSERT INTO session \
               (id, project_id, slug, directory, title, version, time_created, time_updated) \
             VALUES ('{SESSION_ID}', 'project-soak', 'soak', '{project}', 'soak', '1', 1, 1);"
        ))
        .expect("seed soak session");
    connection
}

fn put_user(connection: &Connection, turn: usize, created: i64) {
    let message_id = format!("msg_soak_user_{turn:04}");
    let message = MessageRecord::from_json(json!({
        "id": message_id,
        "sessionID": SESSION_ID,
        "role": "user",
        "time": { "created": created },
        "agent": "build",
        "model": { "providerID": "cassette", "modelID": CHAT_MODEL }
    }))
    .expect("valid soak user message");
    let part = PartRecord::from_json(
        json!({
            "id": format!("prt_soak_user_{turn:04}"),
            "sessionID": SESSION_ID,
            "messageID": message_id,
            "type": "text",
            "text": if turn == 1 {
                "Call get_weather with city exactly Paris."
            } else {
                "Reply with exactly: Hello!"
            }
        }),
        created,
    )
    .expect("valid soak user part");
    let store = MessageStore::new(connection);
    store
        .put_message_at(&message, created)
        .expect("persist soak user message");
    store
        .put_part_at(&part, created)
        .expect("persist soak user part");
}

fn current_unix_millis() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_millis(),
    )
    .expect("Unix timestamp fits i64")
}

fn live_lsp_manager(project: &Path) -> Manager {
    let rust = which::which("rust-analyzer").expect("G3 requires rust-analyzer");
    let typescript =
        which::which("typescript-language-server").expect("G3 requires typescript-language-server");
    let mut servers = serde_json::Map::new();
    for id in BUILTIN_SERVER_IDS {
        servers.insert((*id).to_owned(), json!({ "disabled": true }));
    }
    servers.insert(
        "rust".to_owned(),
        json!({ "command": [rust.to_string_lossy()] }),
    );
    servers.insert(
        "typescript".to_owned(),
        json!({ "command": [typescript.to_string_lossy(), "--stdio"] }),
    );
    let config: LspConfig =
        serde_json::from_value(serde_json::Value::Object(servers)).expect("two-server LSP config");
    let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(
        &config,
    ))));
    Manager::new(
        project,
        registry,
        RestartPolicy::default(),
        std::num::NonZeroUsize::new(4).expect("non-zero"),
    )
}

async fn start_real_memory_drivers(
    project: &Path,
) -> (
    Watcher,
    zuno_watch::EventStream,
    Manager,
    PtyService,
    zuno_pty::PtyId,
) {
    create_watcher_tree(project).expect("create the 50,000-file W-soak tree");
    let watcher_env = Env::from_process().with("ZUNO_EXPERIMENTAL_FILEWATCHER", "true");
    let (watcher, events) = Watcher::start(
        WatchOptions::new(project)
            .env(watcher_env)
            .require_git(false)
            .debounce(Duration::from_millis(50)),
    )
    .expect("start live W-soak watcher");
    assert!(watcher.decision().watches_project());

    std::fs::create_dir_all(project.join("src")).expect("create Rust source directory");
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='g3-soak'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("write soak Cargo.toml");
    let rust_source = project.join("src/lib.rs");
    std::fs::write(&rust_source, "pub fn value() -> usize { 1 }\n")
        .expect("write soak Rust source");
    std::fs::write(project.join("package.json"), r#"{"private":true}"#)
        .expect("write soak package.json");
    let typescript_source = project.join("main.ts");
    std::fs::write(&typescript_source, "export const value: number = 1;\n")
        .expect("write soak TypeScript source");

    let lsp = live_lsp_manager(project);
    lsp.touch_file(&rust_source)
        .await
        .expect("start real rust-analyzer");
    lsp.touch_file(&typescript_source)
        .await
        .expect("start real TypeScript language server");
    let statuses = lsp.status().await;
    assert_eq!(
        statuses.len(),
        2,
        "G3 requires exactly two live LSP servers"
    );
    assert!(
        statuses
            .iter()
            .all(|status| status.state == ServerState::Connected && status.process_id.is_some()),
        "both real LSP servers must be connected: {statuses:?}"
    );

    let pty = PtyService::new(project);
    let info = pty
        .create(CreateInput {
            command: Some("/bin/sh".to_owned()),
            args: Some(vec![
                "-c".to_owned(),
                format!(
                    "yes 0123456789abcdef | head -c {PTY_OUTPUT_BYTES}; sleep {}",
                    MIN_SOAK_DURATION.as_secs() + 600
                ),
            ]),
            ..Default::default()
        })
        .expect("start real W-soak PTY");
    let pty_id = info.id;
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let written = pty
                .retained_output(&pty_id)
                .expect("read W-soak PTY output")
                .total_written;
            if written >= PTY_OUTPUT_BYTES {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("PTY must emit at least 100 MB within two minutes");

    (watcher, events, lsp, pty, pty_id)
}

async fn compact_soak_session(
    connection: &mut Connection,
    provider: &CassetteProvider,
) -> CompactionOutcome {
    let projected = project_history_owned_with_ids(
        SYSTEM_PROMPT,
        hydrate_retained_history(connection, SESSION_ID).expect("hydrate before compaction"),
    );
    let entries = projected
        .into_iter()
        .enumerate()
        .map(|(index, projected)| {
            TranscriptEntry::new(
                projected
                    .message_id
                    .unwrap_or_else(|| format!("system-{index}")),
                projected.message,
                32,
            )
        })
        .collect();
    let config = CompactionConfig {
        auto: Some(true),
        threshold_percent: None,
        prune: Some(false),
        tail_turns: Some(1),
        preserve_recent_tokens: Some(128),
        reserved: Some(20_000),
    };
    let mut state = CompactionState::default();
    let mut tracker = CacheTracker::new();
    let mut tools = LockedTools::<ToolDefinition>::new();
    let mut cache = CompactionCache::new(&mut tracker, &mut tools);
    run_compaction(
        connection,
        provider,
        &NoopCompactionHooks,
        &mut state,
        &mut cache,
        CompactionRequest::new(
            SESSION_ID,
            "g3-soak-cycle",
            "build",
            "cassette",
            CHAT_MODEL,
            entries,
            &config,
            TokenWindow {
                context: 120_000,
                max_output: 4_096,
            },
            CompactionTrigger::ContextLimit {
                used_tokens: Some(120_001),
                limit_tokens: Some(120_000),
            },
        ),
    )
    .await
    .expect("run real W-soak compaction")
}

async fn pace_turn(deadline: Instant, peak_rss_kib: &mut u64) {
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(SAMPLE_INTERVAL.min(remaining)).await;
        *peak_rss_kib = (*peak_rss_kib).max(current_process_tree_rss());
    }
}

#[async_trait]
impl ToolDispatcher for LargeOutputDispatcher {
    fn available_tools(&self) -> AvailableTools {
        AvailableTools::new(
            vec![ToolDefinition {
                id: "get_weather".to_owned(),
                display_name: "get_weather".to_owned(),
                description: "Return deterministic cassette-backed weather data.".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"]
                }),
                ui_intent: zuno_tool::ToolUiIntent::Generic,
            }],
            McpToolStatus::Ready,
        )
    }

    async fn prepare(&self, request: DispatchRequest) -> PreparedToolDispatch {
        assert_eq!(request.call.name, "get_weather");
        self.calls.fetch_add(1, Ordering::SeqCst);
        PreparedToolDispatch::ready(ToolDispatchResult::success(ToolOutput::text(
            "weather fixture",
            self.output.clone(),
        )))
    }
}

fn frozen_thresholds() -> FrozenThresholds {
    let baseline = load_committed_baseline().expect("committed baseline");
    FrozenThresholds::from_baseline(&baseline).expect("frozen thresholds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "500-turn real-driver soak takes at least two hours"]
async fn g3_and_g4_real_driver_soak_stays_bounded_and_live() {
    if zuno_testkit::recordings_root_or_skip(
        "g3_and_g4_real_driver_soak_stays_bounded_and_live",
        "the 500-turn real-driver soak was NOT measured",
    )
    .is_none()
    {
        return;
    }
    let thresholds = frozen_thresholds();
    let limits = WatchdogLimits {
        progress_timeout: Duration::from_secs_f64(thresholds.g4_progress_timeout_seconds),
        hard_deadline: Duration::from_secs_f64(thresholds.g4_hard_deadline_seconds),
    };
    let workspace = tempfile::tempdir().expect("create W-soak workspace");
    let project = workspace.path();
    let (watcher, mut watch_events, lsp, pty, pty_id) = start_real_memory_drivers(project).await;

    let tool_responses = cassette_responses("openai-chat/drives-a-tool-loop-end-to-end");
    assert_eq!(tool_responses.len(), 2, "tool-loop cassette shape changed");
    let text_response = cassette_responses("openai-chat/streams-text");
    assert_eq!(text_response.len(), 1, "text cassette shape changed");
    let text_response = text_response.into_iter().next().expect("one text response");
    let mut responses = tool_responses;
    responses.extend((2..=250).map(|_| text_response.clone()));
    responses.push(text_response.clone());
    responses.extend((251..=SOAK_TURNS).map(|_| text_response.clone()));
    let provider = Arc::new(CassetteProvider::new(responses));
    let mut providers = ProviderRegistry::new();
    let registered = Arc::clone(&provider);
    providers.register("cassette", move |_spec| registered.clone());
    let dispatcher = LargeOutputDispatcher {
        output: "x".repeat(TOOL_OUTPUT_BYTES),
        calls: AtomicUsize::new(0),
    };
    let resolver = SoakResolver;
    let interrupt = InterruptSignal::new();
    let mut connection = seed_connection(project);
    let mut samples = Vec::with_capacity(SOAK_TURNS);
    let soak_started = Instant::now();
    let mut compacted = false;

    for turn in 1..=SOAK_TURNS {
        let created = current_unix_millis();
        put_user(&connection, turn, created);
        let watched_index = turn % 50_000;
        let watched = project
            .join("watch-tree")
            .join(format!("d{:03}", watched_index / 500))
            .join(format!("f{watched_index:05}.txt"));
        std::fs::write(&watched, format!("turn {turn}\n")).expect("mutate watched W-soak file");

        let (sender, receiver) = event_channel();
        let mut peak_rss_kib = current_process_tree_rss();
        let outcome = monitor_turn(
            run_turn(
                RunTurnRequest::new(
                    SESSION_ID,
                    format!("g3-soak-{turn:04}"),
                    DynamicContext::default(),
                ),
                TurnContext::new(
                    &mut connection,
                    &providers,
                    &resolver,
                    &dispatcher,
                    &interrupt,
                ),
                sender,
            ),
            receiver,
            limits,
            &mut peak_rss_kib,
        )
        .await
        .unwrap_or_else(|error| panic!("turn {turn} failed G4: {error}"));
        assert!(
            matches!(outcome, TurnOutcome::Completed { .. }),
            "turn {turn} did not complete: {outcome:?}"
        );

        if turn == 250 {
            let outcome = compact_soak_session(&mut connection, provider.as_ref()).await;
            assert!(
                matches!(outcome, CompactionOutcome::Compacted(_)),
                "W-soak compaction did not complete: {outcome:?}"
            );
            compacted = true;
        }
        if turn % 25 == 0 {
            let rust_source = project.join("src/lib.rs");
            let typescript_source = project.join("main.ts");
            lsp.touch_file(&rust_source)
                .await
                .expect("refresh rust-analyzer during soak");
            lsp.touch_file(&typescript_source)
                .await
                .expect("refresh TypeScript server during soak");
        }

        let turn_deadline =
            soak_started + MIN_SOAK_DURATION.mul_f64(turn as f64 / SOAK_TURNS as f64);
        pace_turn(turn_deadline, &mut peak_rss_kib).await;
        samples.push(RssAtTurn {
            turn,
            rss_kib: peak_rss_kib,
        });
        while watch_events.try_recv().is_some() {}
        if turn % 25 == 0 {
            eprintln!(
                "W_SOAK_PROGRESS turn={turn}/{SOAK_TURNS} elapsed_seconds={} peak_rss_kib={peak_rss_kib}",
                soak_started.elapsed().as_secs()
            );
        }
    }

    assert!(soak_started.elapsed() >= MIN_SOAK_DURATION);
    assert_eq!(samples.len(), SOAK_TURNS);
    assert!(compacted, "W-soak must execute a real compaction cycle");
    assert_eq!(
        dispatcher.calls.load(Ordering::SeqCst),
        1,
        "W-soak must execute the 50 MB tool exactly once"
    );
    assert!(watcher.accepted() > 0, "live watcher observed no changes");
    assert!(watcher.published() > 0, "live watcher published no changes");
    assert_eq!(watcher.dropped(), 0, "live watcher dropped changes");
    let statuses = lsp.status().await;
    assert_eq!(statuses.len(), 2);
    assert!(
        statuses
            .iter()
            .all(|status| status.state == ServerState::Connected && status.process_id.is_some()),
        "real LSP server did not survive W-soak: {statuses:?}"
    );
    let pty_output = pty
        .retained_output(&pty_id)
        .expect("read final W-soak PTY counters");
    assert!(pty_output.total_written >= PTY_OUTPUT_BYTES);

    let verdict =
        GrowthVerdict::evaluate(&samples, thresholds).unwrap_or_else(|error| panic!("{error}"));
    let artifact = json!({
        "turns": SOAK_TURNS,
        "elapsed_seconds": soak_started.elapsed().as_secs(),
        "sample_interval_seconds": SAMPLE_INTERVAL.as_secs(),
        "rss_samples": samples.iter().map(|sample| json!({
            "turn": sample.turn,
            "peak_process_tree_rss_kib": sample.rss_kib,
        })).collect::<Vec<_>>(),
        "g3": {
            "theil_sen_mib_per_turn": verdict.slope_mib_per_turn,
            "maximum_mib_per_turn": verdict.max_slope_mib_per_turn,
            "final_to_middle_peak_ratio": verdict.peak_ratio,
            "maximum_peak_ratio": verdict.max_peak_ratio,
            "window_first_turn": verdict.window_first_turn,
            "window_last_turn": verdict.window_last_turn,
        },
        "g4": {
            "progress_timeout_seconds": thresholds.g4_progress_timeout_seconds,
            "hard_deadline_seconds": thresholds.g4_hard_deadline_seconds,
        },
        "drivers": {
            "watch_files": 50_000,
            "watch_events_accepted": watcher.accepted(),
            "watch_events_published": watcher.published(),
            "lsp_servers": statuses,
            "tool_output_bytes": TOOL_OUTPUT_BYTES,
            "pty_output_bytes": pty_output.total_written,
            "compaction_completed": compacted,
        }
    });
    let artifact_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/perf");
    std::fs::create_dir_all(&artifact_dir).expect("create G3/G4 artifact directory");
    std::fs::write(
        artifact_dir.join("task-89-soak.json"),
        serde_json::to_vec_pretty(&artifact).expect("serialize G3/G4 artifact"),
    )
    .expect("write G3/G4 artifact");
    eprintln!(
        "G3 PASS slope={:.3} MiB/turn (max {:.3}), peak_ratio={:.3} (max {:.3}); G4 PASS turns={SOAK_TURNS} elapsed_seconds={}",
        verdict.slope_mib_per_turn,
        verdict.max_slope_mib_per_turn,
        verdict.peak_ratio,
        verdict.max_peak_ratio,
        soak_started.elapsed().as_secs()
    );

    lsp.shutdown().await;
    pty.remove(&pty_id).expect("stop W-soak PTY");
}

#[test]
fn a_deliberate_two_mib_per_turn_slope_fails_and_reports_the_measurement() {
    let thresholds = frozen_thresholds();
    let samples: Vec<_> = (1..=100)
        .map(|turn| RssAtTurn {
            turn,
            rss_kib: 64 * 1024 + turn as u64 * 2 * 1024,
        })
        .collect();

    let error = GrowthVerdict::evaluate(&samples, thresholds)
        .expect_err("2 MiB/turn must exceed the frozen G3 bound");

    assert!(error.contains("2.000"), "{error}");
    assert!(
        error.contains(&format!("{:.3}", thresholds.g3_max_mib_per_turn)),
        "{error}"
    );
    assert!(error.contains("turns 51..100"), "{error}");
    eprintln!("G3_LEAK_PROOF {error}");
}

#[test]
fn only_the_final_half_determines_the_theil_sen_slope() {
    let thresholds = frozen_thresholds();
    let samples: Vec<_> = (1..=100)
        .map(|turn| RssAtTurn {
            turn,
            rss_kib: if turn <= 50 {
                200 * 1024
            } else {
                (100 + 2 * (turn - 50)) as u64 * 1024
            },
        })
        .collect();

    let error = GrowthVerdict::evaluate(&samples, thresholds)
        .expect_err("the final-half 2 MiB/turn leak must fail G3");

    assert!(error.contains("2.000 MiB/turn"), "{error}");
    assert!(error.contains("turns 51..100"), "{error}");
}

#[test]
fn peak_reference_uses_literal_turns_forty_through_sixty() {
    let thresholds = frozen_thresholds();
    let samples: Vec<_> = (1..=500)
        .map(|turn| RssAtTurn {
            turn,
            rss_kib: match turn {
                200..=300 => 50 * 1024,
                451..=500 => 140 * 1024,
                _ => 100 * 1024,
            },
        })
        .collect();

    let verdict = GrowthVerdict::evaluate(&samples, thresholds)
        .expect("140/100 passes when the reference is literal turns 40-60");

    assert_eq!(verdict.peak_ratio, 1.4);
    assert_eq!(verdict.window_first_turn, 251);
    assert_eq!(verdict.window_last_turn, 500);
}

#[test]
fn a_flat_final_window_passes_both_g3_predicates() {
    let thresholds = frozen_thresholds();
    let samples: Vec<_> = (1..=100)
        .map(|turn| RssAtTurn {
            turn,
            rss_kib: if turn < 5 { 80 * 1024 } else { 64 * 1024 },
        })
        .collect();

    let verdict = GrowthVerdict::evaluate(&samples, thresholds).expect("flat tail passes G3");

    assert!(verdict.slope_mib_per_turn <= thresholds.g3_max_mib_per_turn);
    assert!(verdict.peak_ratio <= thresholds.g3_max_peak_ratio);
    assert_eq!(verdict.window_first_turn, 51);
    assert_eq!(verdict.window_last_turn, 100);
    assert_eq!(verdict.window_samples, 50);
    assert_eq!(
        verdict.max_slope_mib_per_turn,
        thresholds.g3_max_mib_per_turn
    );
    assert_eq!(verdict.max_peak_ratio, thresholds.g3_max_peak_ratio);
}

#[tokio::test]
async fn a_stalled_turn_trips_the_progress_watchdog() {
    let limits = WatchdogLimits {
        progress_timeout: Duration::from_millis(30),
        hard_deadline: Duration::from_secs(1),
    };
    let (sender, receiver) = event_channel();
    let stalled = async move {
        let _keep_channel_open = sender;
        futures::future::pending::<Result<TurnOutcome, TurnError>>().await
    };
    let mut peak_rss_kib = 0;

    let error = monitor_turn(stalled, receiver, limits, &mut peak_rss_kib)
        .await
        .expect_err("a silent turn must trip G4");

    assert!(error.contains("G4"), "{error}");
    assert!(error.contains("progress"), "{error}");
    assert!(error.contains("30"), "{error}");
    let measured_ms = error
        .split_whitespace()
        .nth(4)
        .expect("G4 error contains measured milliseconds")
        .parse::<u128>()
        .expect("measured G4 milliseconds are numeric");
    assert!(measured_ms > 30, "{error}");
    eprintln!("G4_STALL_PROOF {error}");
}

#[test]
fn heartbeats_raw_bytes_and_repeated_state_do_not_reset_g4_progress() {
    let mut progress = ProgressTracker::default();
    assert!(!progress.observe(&TurnEvent::TurnStarted {
        session_id: SESSION_ID.to_owned(),
    }));
    assert!(!progress.observe(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::TextDelta("incomplete".to_owned()),
    }));
    assert!(!progress.observe(&TurnEvent::Provider {
        step: 1,
        event: StreamEvent::StatusDetail {
            detail: "heartbeat".to_owned(),
        },
    }));
    let created = TurnEvent::AssistantMessageCreated {
        step: 1,
        message_id: "msg-progress".to_owned(),
    };
    assert!(progress.observe(&created));
    assert!(!progress.observe(&created));
    let usage = TurnEvent::Provider {
        step: 1,
        event: StreamEvent::TokenUsage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: None,
            accounting: PromptAccounting::CacheInsideInput,
        },
    };
    assert!(progress.observe(&usage));
    assert!(!progress.observe(&usage));
}

#[test]
fn the_hard_deadline_fires_even_when_progress_keeps_resetting_silence() {
    let limits = WatchdogLimits {
        progress_timeout: Duration::from_millis(30),
        hard_deadline: Duration::from_millis(50),
    };
    let mut watchdog = LivenessWatchdog::new(limits);
    watchdog.progress(Duration::from_millis(40));

    let error = watchdog
        .check(Duration::from_millis(51))
        .expect_err("progress cannot waive the G4 hard deadline");

    assert!(error.contains("G4"), "{error}");
    assert!(error.contains("hard deadline"), "{error}");
    assert!(error.contains("51"), "{error}");
    assert!(error.contains("50"), "{error}");
}
