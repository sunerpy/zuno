use std::sync::Arc;

use tempfile::TempDir;
use zuno_db::{Pool, migration, session};
use zuno_goal::{GoalPauseReason, GoalStatus, GoalStore};
use zuno_paths::DbLocation;
use zuno_review::{CodeGraphIndexSnapshot, PlanReviewGate, ReviewSourceSnapshot, ReviewStore};
use zuno_session_control::{
    EnterPlanRequest, SessionControlError, SessionControlService, StartWorkDisposition,
    StartWorkRequest,
};
use zuno_tools::{PlanStep, PlanStepStatus, PlanUpdateParams, WorkStateStore};
use zuno_types::execution::{
    CollaborationMode, InputTriggerKind, SessionExecutionPhase, TurnExecutionIdentity,
};

const SESSION: &str = "ses_control";

struct Fixture {
    _spill: TempDir,
    pool: Arc<Pool>,
    goals: GoalStore,
    work: WorkStateStore,
    control: SessionControlService,
}

impl Fixture {
    fn new() -> Self {
        let pool = Arc::new(Pool::open(&DbLocation::Memory).expect("pool"));
        {
            let mut connection = pool.get().expect("connection");
            migration::apply(&mut connection).expect("schema");
            connection
                .execute(
                    "INSERT INTO project \
                     (id, worktree, time_created, time_updated, sandboxes) \
                     VALUES ('project', '/workspace', 1, 1, '[]')",
                    [],
                )
                .expect("project");
        }
        pool.transaction(|transaction| {
            session::create(
                transaction,
                &session::SessionCreate::new(
                    SESSION,
                    "control",
                    "project",
                    "/workspace",
                    "/workspace",
                    "Control test",
                    "zuno",
                )
                .at(1),
            )
            .map(|_| ())
        })
        .expect("session");
        let spill = tempfile::tempdir().expect("spill");
        let goals =
            GoalStore::from_pool(Arc::clone(&pool), spill.path().to_path_buf()).expect("goals");
        Self {
            _spill: spill,
            work: WorkStateStore::new(Arc::clone(&pool)),
            control: SessionControlService::new(Arc::clone(&pool)),
            pool,
            goals,
        }
    }

    fn identity() -> TurnExecutionIdentity {
        TurnExecutionIdentity::new("build", "provider", "model").with_reasoning(Some("high"))
    }

    fn plan(&self) -> zuno_tools::WorkPlan {
        self.work
            .update_plan(
                SESSION,
                PlanUpdateParams {
                    expected_revision: None,
                    goal_id: None,
                    title: "Implement the approved design".to_owned(),
                    steps: vec![PlanStep {
                        id: "step_1".to_owned(),
                        title: "Implement".to_owned(),
                        status: PlanStepStatus::InProgress,
                    }],
                },
            )
            .expect("plan")
    }

    fn enter_plan(&self) {
        self.control
            .enter_plan(EnterPlanRequest {
                session_id: SESSION,
                work_identity: Self::identity(),
                at_ms: 10,
            })
            .expect("enter plan");
    }
}

fn source(at_ms: i64) -> ReviewSourceSnapshot {
    ReviewSourceSnapshot {
        id: format!("src_{at_ms}"),
        repository_root: "/workspace".to_owned(),
        head_sha: "abc123".to_owned(),
        branch: Some("main".to_owned()),
        worktree_path: "/workspace".to_owned(),
        dirty: false,
        worktree_digest: "sha256:clean".to_owned(),
        scope_paths: vec!["crates".to_owned()],
        artifact: None,
        codegraph: CodeGraphIndexSnapshot {
            initialized: true,
            extraction_status: "current".to_owned(),
            built_with_extraction_version: 1,
            current_extraction_version: 1,
            ..CodeGraphIndexSnapshot::default()
        },
        captured_at_ms: at_ms,
    }
}

#[test]
fn plan_handoff_and_start_work_commit_goal_authority_and_control_input_together() {
    let fixture = Fixture::new();
    let goal = fixture
        .goals
        .create_goal(SESSION, "deliver the implementation", None)
        .expect("goal");
    let plan = fixture.plan();
    fixture.enter_plan();
    let paused = fixture
        .goals
        .goal(SESSION)
        .expect("read goal")
        .expect("goal");
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(
        fixture
            .goals
            .pause_state(SESSION)
            .expect("pause")
            .expect("pause")
            .reason,
        GoalPauseReason::PlanMode
    );

    let handoff = fixture
        .control
        .mark_plan_handoff(SESSION, 20)
        .expect("handoff");
    assert_eq!(handoff.handoff_plan_id.as_deref(), Some(plan.id.as_str()));
    assert_eq!(handoff.handoff_plan_revision, Some(plan.revision));

    let outcome = fixture
        .control
        .start_work(StartWorkRequest {
            session_id: SESSION,
            expected_plan_revision: Some(plan.revision),
            anchor_message_id: None,
            draft_review_risk_reason: None,
            session_busy: false,
            at_ms: 30,
        })
        .expect("start work");
    assert_eq!(outcome.disposition, StartWorkDisposition::Started);
    assert_eq!(outcome.review_gate, PlanReviewGate::Unbound);
    assert_eq!(outcome.state.mode, CollaborationMode::Work);
    assert_eq!(outcome.state.phase, SessionExecutionPhase::Authorized);
    assert_eq!(
        outcome.state.authorized_plan_id.as_deref(),
        Some(plan.id.as_str())
    );
    assert_eq!(outcome.state.authorized_plan_revision, Some(plan.revision));
    assert_eq!(outcome.input.trigger_kind, InputTriggerKind::UserControl);
    assert_eq!(outcome.input.cycle_id, outcome.state.cycle_id);
    assert_eq!(
        outcome.goal.as_ref().map(|goal| goal.status),
        Some(GoalStatus::Active)
    );
    assert_eq!(
        outcome.goal.as_ref().map(|goal| goal.goal_id.as_str()),
        Some(goal.goal_id.as_str())
    );

    let repeated = fixture
        .control
        .start_work(StartWorkRequest {
            session_id: SESSION,
            expected_plan_revision: Some(plan.revision),
            anchor_message_id: None,
            draft_review_risk_reason: None,
            session_busy: true,
            at_ms: 40,
        })
        .expect("idempotent start work");
    assert_eq!(repeated.disposition, StartWorkDisposition::Queued);
    assert_eq!(repeated.state.revision, outcome.state.revision);
    assert_eq!(repeated.input.id, outcome.input.id);
    assert_eq!(
        repeated.goal.as_ref().map(|goal| goal.revision),
        outcome.goal.as_ref().map(|goal| goal.revision)
    );
}

#[test]
fn start_work_requires_the_exact_current_handoff_revision() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.enter_plan();
    fixture
        .control
        .mark_plan_handoff(SESSION, 20)
        .expect("handoff");
    let changed = fixture
        .work
        .update_plan(
            SESSION,
            PlanUpdateParams {
                expected_revision: Some(plan.revision),
                goal_id: None,
                title: "Changed after handoff".to_owned(),
                steps: plan.steps,
            },
        )
        .expect("change plan");

    let error = fixture
        .control
        .start_work(StartWorkRequest {
            session_id: SESSION,
            expected_plan_revision: Some(changed.revision),
            anchor_message_id: None,
            draft_review_risk_reason: None,
            session_busy: false,
            at_ms: 30,
        })
        .expect_err("stale handoff");
    assert!(matches!(
        error,
        SessionControlError::HandoffRequired {
            plan_revision,
            ..
        } if plan_revision == changed.revision
    ));
}

#[test]
fn a_bound_draft_review_blocks_by_default_and_persists_explicit_risk_acceptance() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.enter_plan();
    fixture
        .control
        .mark_plan_handoff(SESSION, 20)
        .expect("handoff");
    let review = ReviewStore::new(Arc::clone(&fixture.pool))
        .open_review(
            SESSION,
            Some(&plan.id),
            Some(plan.revision),
            true,
            || Ok(source(21)),
            21,
        )
        .expect("review");

    let error = fixture
        .control
        .start_work(StartWorkRequest {
            session_id: SESSION,
            expected_plan_revision: Some(plan.revision),
            anchor_message_id: None,
            draft_review_risk_reason: None,
            session_busy: false,
            at_ms: 30,
        })
        .expect_err("Draft review blocks");
    assert!(matches!(
        error,
        SessionControlError::DraftReview {
            review_id,
            review_revision,
        } if review_id == review.review_id && review_revision == review.revision
    ));

    let accepted = fixture
        .control
        .start_work(StartWorkRequest {
            session_id: SESSION,
            expected_plan_revision: Some(plan.revision),
            anchor_message_id: Some("msg_anchor".to_owned()),
            draft_review_risk_reason: Some("proceed with the documented residual risk".to_owned()),
            session_busy: false,
            at_ms: 31,
        })
        .expect("explicit acceptance");
    let risk = accepted
        .state
        .draft_review_risk
        .expect("persisted Draft risk");
    assert_eq!(risk.review_id, review.review_id);
    assert_eq!(risk.review_revision, review.revision);
    assert_eq!(risk.reason, "proceed with the documented residual risk");
    assert_eq!(
        accepted
            .state
            .continuation
            .as_ref()
            .and_then(|token| token.anchor_message_id.as_deref()),
        Some("msg_anchor")
    );
}
