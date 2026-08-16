//! Cross-crate proof for resident memory, the model-facing tool, and reflection.
//!
//! The component crates cannot prove this loop separately: the store does not own
//! reflection, reflection accepts only an erased tool, and the tool does not own a
//! session prompt. These tests deliberately cross all three boundaries.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;
use zuno_agent::reflection::{
    ReflectionConfig, ReflectionError, ReflectionFork, ReflectionRequest, ReflectionRunner,
    ReflectionToolCall, ReflectionTools, ReflectionTurn, TranscriptEvent, TurnDelivery,
    TurnTranscript,
};
use zuno_config::Config;
use zuno_memory::{
    MemoryStore, Operation, Scope, ScopeLimits, SessionMemory, assemble_system_prompt,
};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext, erase};
use zuno_tools::{MemoryTool, ScopePaths};

const BASE_PROMPT: &str = "SYSTEM\nPreserve these bytes: ${UNEXPANDED}\r\n终";
const CORRECTION: &str =
    "Use `cargo test -p zuno-memory --test integration` before the workspace suite.";

fn parse_config(text: &str) -> Config {
    Config::from_json_str(Path::new("opencode.json"), text).expect("memory config parses")
}

fn paths(directory: &TempDir) -> ScopePaths {
    ScopePaths::at(
        directory.path().join("global").join("MEMORY.md"),
        directory.path().join("project").join("RULES.md"),
    )
}

fn context(session_id: &str) -> ToolContext {
    ToolContext::new(
        session_id,
        "msg_reflection",
        "call_reflection",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn delivered(session_id: &str, transcript: TurnTranscript) -> ReflectionTurn {
    ReflectionTurn::new(
        TurnDelivery::new(true, false),
        transcript,
        context(session_id),
    )
}

struct CorrectionRunner {
    reviews: AtomicUsize,
    invoked: Notify,
}

impl CorrectionRunner {
    fn new() -> Self {
        Self {
            reviews: AtomicUsize::new(0),
            invoked: Notify::new(),
        }
    }

    fn review_count(&self) -> usize {
        self.reviews.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ReflectionRunner for CorrectionRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        self.reviews.fetch_add(1, Ordering::SeqCst);
        self.invoked.notify_one();
        tools
            .dispatch(ReflectionToolCall::new(
                "reflection-memory-call",
                "memory",
                json!({
                    "target": "project",
                    "action": "add",
                    "content": CORRECTION,
                }),
            ))
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn a_correction_reflected_in_session_one_changes_session_twos_system_prompt() {
    let directory = TempDir::new().expect("temp dir");
    let paths = paths(&directory);
    let config = parse_config(r#"{"memory":{"nudge_interval":1}}"#);
    let memory = config.resolved_memory();
    let limits = ScopeLimits::new(memory.global_char_limit, memory.project_char_limit);

    let session_one = SessionMemory::open_configured(
        memory.resident,
        paths.for_scope(Scope::Global),
        paths.for_scope(Scope::Project),
        limits,
    )
    .expect("session one opens")
    .expect("resident memory is enabled");
    assert_eq!(
        assemble_system_prompt(BASE_PROMPT, Some(&session_one)),
        BASE_PROMPT
    );

    let tool = MemoryTool::configured(memory.tool, paths.clone(), limits)
        .expect("the model-facing memory tool is enabled");
    let runner = Arc::new(CorrectionRunner::new());
    let fork = ReflectionFork::new(
        ReflectionConfig {
            enabled: memory.reflection,
            turn_interval: memory.nudge_interval,
        },
        Arc::clone(&runner) as Arc<dyn ReflectionRunner>,
        erase(tool),
    )
    .expect("the configured tool has the memory id");
    let transcript = TurnTranscript::new(vec![TranscriptEvent::user(format!(
        "Correction: {CORRECTION}"
    ))]);

    fork.spawn_after_turn(delivered("ses_one", transcript))
        .expect("the delivered correction starts reflection")
        .await
        .expect("reflection contains its own failures");
    assert_eq!(runner.review_count(), 1, "the correction is reviewed once");
    drop(session_one);

    let session_two = SessionMemory::open_configured(
        memory.resident,
        paths.for_scope(Scope::Global),
        paths.for_scope(Scope::Project),
        limits,
    )
    .expect("session two opens")
    .expect("resident memory is enabled");
    let prompt = assemble_system_prompt(BASE_PROMPT, Some(&session_two));
    assert!(
        prompt.contains(CORRECTION),
        "next session did not learn: {prompt}"
    );
    assert!(prompt.contains(Scope::Project.label()), "{prompt}");
}

#[tokio::test]
async fn memory_false_matches_a_real_upstream_control_and_spawns_no_reflection() {
    let directory = TempDir::new().expect("temp dir");
    let paths = paths(&directory);
    let mut seeded = MemoryStore::open(Scope::Project, paths.for_scope(Scope::Project).into())
        .expect("seeded store opens");
    seeded
        .apply_batch(&[Operation::add(CORRECTION)])
        .expect("non-empty memory makes the control sensitive");

    let config = parse_config(r#"{"memory":false}"#);
    let memory = config.resolved_memory();
    assert!(!memory.resident && !memory.tool && !memory.reflection);
    let limits = ScopeLimits::new(memory.global_char_limit, memory.project_char_limit);

    let disabled_session = SessionMemory::open_configured(
        memory.resident,
        paths.for_scope(Scope::Global),
        paths.for_scope(Scope::Project),
        limits,
    )
    .expect("disabled construction is infallible");
    assert!(
        disabled_session.is_none(),
        "disabled memory must not open a store"
    );
    let disabled_prompt = assemble_system_prompt(BASE_PROMPT, disabled_session.as_ref());

    // This control is genuinely subsystem-absent: it is copied directly from the
    // pre-memory prompt bytes and never calls a memory constructor or prompt helper.
    let upstream_without_memory = BASE_PROMPT.as_bytes().to_vec();
    assert_eq!(
        disabled_prompt.as_bytes(),
        upstream_without_memory.as_slice()
    );
    assert!(!disabled_prompt.contains(CORRECTION));

    // Sensitivity guard: the same seeded files must change the enabled path. If a
    // future edit empties the fixture, the byte-identity assertion cannot pass
    // vacuously because this independent inequality fails.
    let enabled_session = SessionMemory::open_configured(
        true,
        paths.for_scope(Scope::Global),
        paths.for_scope(Scope::Project),
        limits,
    )
    .expect("enabled construction reads the seeded file")
    .expect("enabled session exists");
    let enabled_prompt = assemble_system_prompt(BASE_PROMPT, Some(&enabled_session));
    assert_ne!(
        enabled_prompt.as_bytes(),
        upstream_without_memory.as_slice()
    );
    assert!(enabled_prompt.contains(CORRECTION));

    assert!(
        MemoryTool::configured(memory.tool, paths.clone(), limits).is_none(),
        "the model-facing tool must be absent, not merely refusing calls"
    );

    // Deliberately provide a usable memory tool underneath the disabled fork. This
    // isolates the reflection gate: only `enabled: false` can explain no spawn.
    let runner = Arc::new(CorrectionRunner::new());
    let fork = ReflectionFork::new(
        ReflectionConfig {
            enabled: memory.reflection,
            turn_interval: 1,
        },
        Arc::clone(&runner) as Arc<dyn ReflectionRunner>,
        erase(MemoryTool::with_paths_and_limits(paths, limits)),
    )
    .expect("test tool has the memory id");
    let spawned = fork.spawn_after_turn(delivered(
        "ses_disabled",
        TurnTranscript::new(vec![TranscriptEvent::user(
            "Correction that must be ignored",
        )]),
    ));
    assert!(
        spawned.is_none(),
        "disabled reflection returned a task handle"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), runner.invoked.notified())
            .await
            .is_err(),
        "the runner was invoked despite memory: false"
    );
    assert_eq!(runner.review_count(), 0);
}
