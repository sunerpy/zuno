#![allow(
    dead_code,
    reason = "each integration-test crate uses only the fixture operations it exercises"
)]

//! A test host for the shared question port, backed by the real durable store.
//!
//! The fixture supplies trusted Plan/Goal bindings; session-control tests own
//! their validation and authorization. Publication, replies, revisions, receipts,
//! and inbox admission use the production SQLite transactions.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::watch;
use uuid::Uuid;
use zuno_db::human_request::{self, HumanRequestState};
use zuno_db::inbox::SessionInbox;
use zuno_db::question::{self, QuestionStore};
use zuno_db::{Pool, migration, session};
use zuno_paths::DbLocation;
use zuno_tool::question::{QuestionError, QuestionPort, QuestionResult};
use zuno_tool::{AllowAll, InterruptHandle, NeverInterrupted, ToolContext};
use zuno_types::execution::TurnExecutionIdentity;
use zuno_types::question::{
    PlanQuestionBinding, QuestionCommand, QuestionMode, QuestionOption, QuestionPurpose,
    QuestionReceipt, QuestionRequest, QuestionSpec, QuestionView,
};

pub const SESSION_ID: &str = "ses_question_port";
pub const OTHER_SESSION_ID: &str = "ses_other_question_port";
pub const TURN_ID: &str = "turn_question_port";

pub struct QuestionFixture {
    pub port: Arc<DurableQuestionPort>,
    // Close the pool before removing its directory, including on Windows.
    _directory: tempfile::TempDir,
}

impl QuestionFixture {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("question workspace");
        let pool = Arc::new(
            Pool::open(&DbLocation::File(directory.path().join("questions.db")))
                .expect("question database"),
        );
        let worktree = directory.path().to_string_lossy().into_owned();
        {
            let mut connection = pool.get().expect("database connection");
            migration::apply(&mut connection).expect("question schema");
            connection
                .execute(
                    "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes) \
                     VALUES ('project',?1,1,1,'[]')",
                    [&worktree],
                )
                .expect("project");
        }
        for session_id in [SESSION_ID, OTHER_SESSION_ID] {
            pool.transaction(|tx| {
                session::create(
                    tx,
                    &session::SessionCreate::new(
                        session_id,
                        "question",
                        "project",
                        &worktree,
                        &worktree,
                        "Question port",
                        "zuno",
                    )
                    .at(1),
                )
                .map(|_| ())
            })
            .expect("question session");
        }
        Self {
            _directory: directory,
            port: Arc::new(DurableQuestionPort::new(pool)),
        }
    }

    pub fn shared_port(&self) -> Arc<dyn QuestionPort> {
        Arc::clone(&self.port) as Arc<dyn QuestionPort>
    }

    pub fn pool(&self) -> Arc<Pool> {
        Arc::clone(&self.port.pool)
    }

    pub fn inbox(&self) -> SessionInbox {
        SessionInbox::new(self.pool())
    }

    /// A fresh pool proves that state belongs to SQLite, not the test adapter.
    pub fn reopen(&self) -> Arc<DurableQuestionPort> {
        let pool = Arc::new(Pool::open(self.port.pool.location()).expect("reopen database"));
        let mut port = DurableQuestionPort::new(pool);
        port.changes = self.port.changes.clone();
        Arc::new(port)
    }
}

pub struct DurableQuestionPort {
    pool: Arc<Pool>,
    changes: watch::Sender<()>,
    opened: Mutex<Vec<QuestionSpec>>,
    waits: AtomicUsize,
    plan: Mutex<Option<PlanQuestionBinding>>,
    required_goal: Mutex<Option<(String, i64)>>,
}

impl DurableQuestionPort {
    fn new(pool: Arc<Pool>) -> Self {
        let (changes, _) = watch::channel(());
        Self {
            pool,
            changes,
            opened: Mutex::new(Vec::new()),
            waits: AtomicUsize::new(0),
            plan: Mutex::new(None),
            required_goal: Mutex::new(None),
        }
    }

    pub fn opened(&self) -> Vec<QuestionSpec> {
        self.opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn wait_count(&self) -> usize {
        self.waits.load(Ordering::Relaxed)
    }

    pub fn bind_plan(&self, plan: PlanQuestionBinding) {
        *self.plan.lock().unwrap_or_else(PoisonError::into_inner) = Some(plan);
    }

    pub fn bind_required_goal(&self, goal_id: &str, revision: i64) {
        *self
            .required_goal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((goal_id.to_owned(), revision));
    }

    pub async fn wait_for_request(&self, call_id: &str) -> QuestionView {
        let mut changes = self.changes.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(view) = self
                    .pending(SESSION_ID)
                    .await
                    .expect("pending questions")
                    .into_iter()
                    .find(|view| view.origin.call_id.as_deref() == Some(call_id))
                {
                    return view;
                }
                changes.changed().await.expect("test host remains alive");
            }
        })
        .await
        .expect("tool must publish its question")
    }

    /// Delivery failure and expiry are host events, not human answer commands.
    pub fn settle(&self, request_id: &str, state: HumanRequestState) {
        assert!(matches!(
            state,
            HumanRequestState::Expired | HumanRequestState::Failed
        ));
        self.pool
            .transaction(|tx| {
                let request = human_request::get_from(tx, request_id)?
                    .expect("host settlement requires a durable request");
                human_request::resolve_in(
                    tx,
                    request_id,
                    state,
                    request.response.as_ref(),
                    zuno_db::message::now_millis(),
                )
            })
            .expect("record host settlement")
            .expect("request was pending");
        self.changes.send_replace(());
    }
}

#[async_trait]
impl QuestionPort for DurableQuestionPort {
    async fn open(&self, mut spec: QuestionSpec) -> QuestionResult<QuestionReceipt> {
        self.opened
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(spec.clone());
        match spec.purpose {
            QuestionPurpose::Clarification => {}
            QuestionPurpose::GoalResume => {
                return Err(QuestionError::Unavailable(
                    "Goal resume requires the native session-control service".to_owned(),
                ));
            }
            QuestionPurpose::PlanAuthorization => {
                let plan = self
                    .plan
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .ok_or_else(|| QuestionError::Rejected {
                        code: "missing_plan",
                        detail: "the test host has no bound Plan".to_owned(),
                    })?;
                spec.questions = vec![QuestionRequest::closed(
                    format!("Start {} revision {}?", plan.title, plan.plan_revision),
                    "Start Work",
                    vec![
                        QuestionOption::new("Approve", "Implement this Plan"),
                        QuestionOption::new("Decline", "Continue planning"),
                    ],
                )];
                spec.plan = Some(plan);
            }
            QuestionPurpose::RequiredInput => {
                let (goal_id, revision) = self
                    .required_goal
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                    .ok_or_else(|| QuestionError::Rejected {
                        code: "missing_goal",
                        detail: "the test host has no required Goal input".to_owned(),
                    })?;
                spec.origin.goal_id = Some(goal_id);
                spec.expected_goal_revision = Some(revision);
            }
        }
        let receipt = self.pool.try_transaction(|tx| {
            question::create_in(
                tx,
                &format!("que_{}", Uuid::now_v7().simple()),
                &spec,
                zuno_db::message::now_millis(),
            )
        })?;
        self.changes.send_replace(());
        Ok(receipt)
    }

    async fn apply(
        &self,
        session_id: &str,
        request_id: &str,
        command: QuestionCommand,
    ) -> QuestionResult<QuestionReceipt> {
        let receipt = QuestionStore::new(Arc::clone(&self.pool)).apply(
            session_id,
            request_id,
            &command,
            zuno_db::message::now_millis(),
        )?;
        self.changes.send_replace(());
        Ok(receipt)
    }

    async fn get(&self, session_id: &str, request_id: &str) -> QuestionResult<QuestionView> {
        QuestionStore::new(Arc::clone(&self.pool)).get(session_id, request_id)
    }

    async fn pending(&self, session_id: &str) -> QuestionResult<Vec<QuestionView>> {
        QuestionStore::new(Arc::clone(&self.pool)).pending(session_id)
    }

    async fn wait_for_change(
        &self,
        session_id: &str,
        request_id: &str,
        after_revision: i64,
        interrupt: Arc<dyn InterruptHandle>,
    ) -> QuestionResult<QuestionView> {
        self.waits.fetch_add(1, Ordering::Relaxed);
        let mut changes = self.changes.subscribe();
        loop {
            let view = self.get(session_id, request_id).await?;
            if view.revision != after_revision
                || view.state.is_terminal()
                || view.mode == QuestionMode::Deferred
            {
                return Ok(view);
            }
            if interrupt.is_set() {
                return Err(QuestionError::Interrupted);
            }
            tokio::select! {
                () = interrupt.notified() => return Err(QuestionError::Interrupted),
                changed = changes.changed() => {
                    changed.map_err(|error| QuestionError::Unavailable(error.to_string()))?;
                }
            }
        }
    }
}

pub fn context(call_id: &str) -> ToolContext {
    context_with_parent(call_id, None)
}

pub fn context_with_parent(call_id: &str, parent: Option<&str>) -> ToolContext {
    let snapshot = serde_json::from_value(json!({
        "schemaVersion": 4, "turnId": TURN_ID, "step": 1,
        "cycleId": null, "parentAuthority": null,
        "capability": {
            "schemaVersion": 4,
            "pack": {"id":"test","version":"1","upstreamRevision":"test"},
            "extensionRevision": 0, "permissionPolicySha256": "policy",
            "sandbox": {
                "mode":"workspace-write", "network":"deny",
                "writableRoots":[], "protectedPaths":[]
            },
            "profiles":[], "presets":[], "councils":[], "workflows":[], "skills":[]
        },
        "owner": {
            "sessionId":SESSION_ID, "parentSessionId":parent, "parentAttempt":null,
            "workflow":null, "workflowNode":null
        },
        "agent": {
            "name":"plan", "sourceId":"test://plan", "definitionSha256":"definition",
            "permissionSha256":"permission", "promptPolicySha256":"prompt"
        },
        "model": {
            "providerId":"fake", "modelId":"fake-model", "wireModelId":"fake-model",
            "surface":"responses", "reasoningSha256":"reasoning", "preset":null
        },
        "selectedSkills": [],
        "prompt": {"eventId":"evt-plan","assemblySha256":"assembly","actualSha256":"actual"},
        "tools": []
    }))
    .expect("question attempt");
    ToolContext::new(
        SESSION_ID,
        "msg_question",
        call_id,
        "plan",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
    .with_orchestration_snapshot(Arc::new(snapshot))
}

pub fn plan_binding() -> PlanQuestionBinding {
    PlanQuestionBinding {
        plan_id: "plan_question_fixture".to_owned(),
        plan_revision: 7,
        source_cycle_id: None,
        title: "Implement the selected storage".to_owned(),
        completed_steps: 2,
        total_steps: 3,
        work_identity: TurnExecutionIdentity::new("build", "fake", "work-model"),
        review_gate: json!({"status":"ready","revision":3}),
    }
}
