//! Cross-crate proof for candidate review, reflection, promotion, and undo.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use zuno_agent::reflection::{
    ReflectionError, ReflectionFork, ReflectionRequest, ReflectionRunner, ReflectionToolCall,
    ReflectionTools, ReflectionTurn, TranscriptEvent, TurnDelivery, TurnTranscript,
};
use zuno_memory::{
    MemoryProposal, MemoryService, MemoryStore, PromotionPolicy, Scope, ScopeLimits, ScopePaths,
    SessionMemory,
};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext, erase};
use zuno_tools::MemoryTool;
use zuno_types::{MemoryAction, MemoryCandidateStatus, MemoryScope, MemorySource};

const CORRECTION: &str =
    "Use `cargo test -p zuno-memory --test integration` before the workspace suite.";

fn fixture(
    directory: &TempDir,
    promotion: PromotionPolicy,
) -> (Arc<zuno_db::Pool>, Arc<MemoryService>) {
    let pool =
        Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("open database"));
    let mut connection = pool.open_connection().expect("database connection");
    zuno_db::migration::apply(&mut connection).expect("initialize schema");
    connection
        .execute_batch(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
             VALUES ('project', '/tmp/project', 1, 1, '[]');
             INSERT INTO session (
                 id, project_id, slug, directory, title, version, time_created, time_updated
             ) VALUES (
                 'ses_reflection', 'project', 'reflection', '/tmp/project',
                 'Reflection', '1', 1, 1
             );",
        )
        .expect("seed reflection session");
    drop(connection);
    let service = Arc::new(MemoryService::new(
        Arc::clone(&pool),
        ScopePaths::at(
            directory.path().join("global").join("MEMORY.md"),
            directory.path().join("project").join("RULES.md"),
        ),
        ScopeLimits::default(),
        promotion,
    ));
    (pool, service)
}

fn service(directory: &TempDir, promotion: PromotionPolicy) -> Arc<MemoryService> {
    fixture(directory, promotion).1
}

fn proposal(content: &str, confidence: f64) -> MemoryProposal {
    MemoryProposal {
        scope: MemoryScope::Project,
        action: MemoryAction::Add,
        content: Some(content.to_owned()),
        old_text: None,
        reason: "verified repository validation rule".to_owned(),
        confidence,
        source: MemorySource::User,
        source_session_id: None,
        source_message_id: None,
    }
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_reflection",
        "msg_reflection",
        "call_reflection",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

struct CorrectionRunner;

#[async_trait]
impl ReflectionRunner for CorrectionRunner {
    async fn review(
        &self,
        _request: ReflectionRequest,
        tools: ReflectionTools,
    ) -> Result<(), ReflectionError> {
        tools
            .dispatch(ReflectionToolCall::new(
                "reflection-memory-call",
                "memory_propose",
                json!({
                    "target": "project",
                    "action": "add",
                    "content": CORRECTION,
                    "reason": "the user corrected the repository gate",
                    "confidence": 0.98
                }),
            ))
            .await?;
        Ok(())
    }
}

#[tokio::test]
async fn reflection_creates_a_pending_candidate_and_approval_changes_the_next_prompt() {
    let directory = TempDir::new().expect("temp dir");
    let service = service(&directory, PromotionPolicy::Review);
    let fork = ReflectionFork::new(
        Arc::new(CorrectionRunner),
        erase(MemoryTool::reflection(Arc::clone(&service))),
    );
    let task = fork
        .spawn_after_turn(ReflectionTurn::new(
            TurnDelivery::new(true, false),
            TurnTranscript::new(vec![TranscriptEvent::user(format!(
                "Correction: {CORRECTION}"
            ))]),
            context(),
        ))
        .expect("reflection spawned");
    task.await
        .expect("reflection task joins")
        .expect("reflection review succeeds");

    let candidates = service.candidates().expect("candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].status, MemoryCandidateStatus::Pending);
    assert!(!service.paths().for_scope(Scope::Project).exists());

    service.apply(&candidates[0].id).expect("approve candidate");
    let prompt = SessionMemory::open(
        service.paths().for_scope(Scope::Global),
        service.paths().for_scope(Scope::Project),
    )
    .expect("session memory")
    .inject_into("SYSTEM");
    assert!(prompt.contains(CORRECTION), "{prompt}");
}

#[test]
fn high_confidence_promotion_applies_only_at_the_configured_threshold() {
    let directory = TempDir::new().expect("temp dir");
    let service = service(
        &directory,
        PromotionPolicy::HighConfidence { threshold: 9_000 },
    );

    let pending = service
        .propose(proposal("candidate below the threshold", 0.89))
        .expect("low confidence proposal");
    let applied = service
        .propose(proposal("candidate at the threshold", 0.90))
        .expect("high confidence proposal");

    assert_eq!(pending.projection.status, MemoryCandidateStatus::Pending);
    assert_eq!(applied.projection.status, MemoryCandidateStatus::Applied);
    assert_eq!(
        service.entries().expect("entries")[0].content,
        "candidate at the threshold"
    );
}

#[test]
fn undo_restores_the_exact_pre_apply_snapshot() {
    let directory = TempDir::new().expect("temp dir");
    let service = service(&directory, PromotionPolicy::Review);
    let first = service
        .propose(proposal("first durable entry", 1.0))
        .expect("first proposal");
    service.apply(first.id()).expect("first apply");
    let second = service
        .propose(proposal("second durable entry", 1.0))
        .expect("second proposal");
    service.apply(second.id()).expect("second apply");

    service.undo(second.id()).expect("undo second");

    assert_eq!(
        service
            .entries()
            .expect("entries")
            .into_iter()
            .map(|entry| entry.content)
            .collect::<Vec<_>>(),
        vec!["first durable entry"]
    );
    assert_eq!(
        service
            .candidates()
            .expect("candidates")
            .into_iter()
            .find(|candidate| candidate.id == second.id())
            .expect("second candidate")
            .status,
        MemoryCandidateStatus::Undone
    );
}

#[test]
fn restart_reconciliation_observes_apply_and_undo_without_replaying_either_write() {
    let directory = TempDir::new().expect("temp dir");
    let (pool, service) = fixture(&directory, PromotionPolicy::Review);
    let store = zuno_db::memory_candidate::MemoryCandidateStore::new(pool);
    let candidate = service
        .propose(proposal("durable entry", 1.0))
        .expect("proposal");
    let project_path = service.paths().for_scope(Scope::Project);

    store
        .begin_apply(candidate.id(), &[], &["durable entry".to_owned()], 20)
        .expect("record interrupted apply");
    MemoryStore::open(Scope::Project, project_path.to_path_buf())
        .expect("open resident store")
        .replace_exact(&[], &["durable entry".to_owned()])
        .expect("simulate completed file write");
    service.reconcile().expect("reconcile apply");
    assert_eq!(
        store
            .get(candidate.id())
            .expect("applied candidate")
            .projection
            .status,
        MemoryCandidateStatus::Applied
    );

    store
        .begin_undo(candidate.id(), 30)
        .expect("record interrupted undo");
    MemoryStore::open(Scope::Project, project_path.to_path_buf())
        .expect("open resident store")
        .replace_exact(&["durable entry".to_owned()], &[])
        .expect("simulate completed undo write");
    service.reconcile().expect("reconcile undo");
    assert_eq!(
        store
            .get(candidate.id())
            .expect("undone candidate")
            .projection
            .status,
        MemoryCandidateStatus::Undone
    );
    assert!(service.entries().expect("resident entries").is_empty());
}

#[test]
fn restart_reconciliation_marks_divergent_resident_state_uncertain() {
    let directory = TempDir::new().expect("temp dir");
    let (pool, service) = fixture(&directory, PromotionPolicy::Review);
    let store = zuno_db::memory_candidate::MemoryCandidateStore::new(pool);
    let candidate = service
        .propose(proposal("expected entry", 1.0))
        .expect("proposal");
    store
        .begin_apply(candidate.id(), &[], &["expected entry".to_owned()], 20)
        .expect("record interrupted apply");
    MemoryStore::open(
        Scope::Project,
        service.paths().for_scope(Scope::Project).to_path_buf(),
    )
    .expect("open resident store")
    .replace_exact(&[], &["different external state".to_owned()])
    .expect("simulate divergent state");

    service.reconcile().expect("reconcile divergence");

    assert_eq!(
        store
            .get(candidate.id())
            .expect("candidate")
            .projection
            .status,
        MemoryCandidateStatus::Uncertain
    );
    assert_eq!(
        service.entries().expect("resident entries")[0].content,
        "different external state"
    );
}
