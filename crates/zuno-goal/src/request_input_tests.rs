//! The Goal tool publishes once; the host service owns Goal binding and waiting.

use super::*;
use serde_json::json;
use std::sync::Mutex;
use zuno_tool::question::{QuestionError, QuestionResult};
use zuno_tool::{AllowAll, InterruptHandle, NeverInterrupted, ToolContinuation};
use zuno_types::question::{
    QuestionAnswers, QuestionCommand, QuestionItem, QuestionReceipt, QuestionView,
};

struct RecordingPort {
    opened: Mutex<Vec<QuestionSpec>>,
    error: Mutex<Option<QuestionError>>,
    state: QuestionState,
    answers: QuestionAnswers,
    duplicate: bool,
}

impl Default for RecordingPort {
    fn default() -> Self {
        Self {
            opened: Mutex::new(Vec::new()),
            error: Mutex::new(None),
            state: QuestionState::Pending,
            answers: QuestionAnswers::new(),
            duplicate: false,
        }
    }
}

#[async_trait::async_trait]
impl QuestionPort for RecordingPort {
    async fn open(&self, spec: QuestionSpec) -> QuestionResult<QuestionReceipt> {
        self.opened.lock().expect("opened specs").push(spec.clone());
        if let Some(error) = self.error.lock().expect("port error").take() {
            return Err(error);
        }
        let mut origin = spec.origin;
        origin.goal_id = Some("goal-bound-by-service".to_owned());
        Ok(QuestionReceipt {
            question: QuestionView {
                id: "que_assigned_by_service".to_owned(),
                origin,
                revision: 7,
                mode: spec.mode,
                purpose: spec.purpose,
                state: self.state,
                questions: spec
                    .questions
                    .into_iter()
                    .enumerate()
                    .map(|(index, question)| QuestionItem {
                        id: format!("q{}", index + 1),
                        question,
                    })
                    .collect(),
                answers: self.answers.clone(),
                draft_answers: Default::default(),
                plan: spec.plan,
                decision: None,
                authorization: None,
                time_created: 100,
                time_updated: 200,
            },
            input_id: (self.state == QuestionState::Answered)
                .then(|| "input-delivered-separately".to_owned()),
            duplicate: self.duplicate,
        })
    }

    async fn apply(&self, _: &str, _: &str, _: QuestionCommand) -> QuestionResult<QuestionReceipt> {
        panic!("publishing required input must not manufacture a client answer")
    }

    async fn get(&self, _: &str, _: &str) -> QuestionResult<QuestionView> {
        panic!("the tool must not read a separate question snapshot")
    }

    async fn pending(&self, _: &str) -> QuestionResult<Vec<QuestionView>> {
        panic!("the tool must not independently look up pending questions")
    }

    async fn wait_for_change(
        &self,
        _: &str,
        _: &str,
        _: i64,
        _: Arc<dyn InterruptHandle>,
    ) -> QuestionResult<QuestionView> {
        panic!("Goal input publication must yield instead of waiting for a human")
    }
}

fn context() -> ToolContext {
    ToolContext::new(
        "session-goal",
        "message-goal",
        "call-goal",
        "deep",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn params() -> Value {
    json!({
        "expected_revision": 42,
        "question": "Which deployment target — 部署到哪里?",
        "header": "Target",
        "options": [
            {"label":"Staging","description":"Verify before production"},
            {"label":"Production","description":"Use the production target"}
        ],
        "multiple": true
    })
}

fn context_with_turn() -> ToolContext {
    context().with_orchestration_snapshot(Arc::new(serde_json::from_value(json!({
        "schemaVersion": 4,
        "turnId": "turn-from-context",
        "step": 3,
        "capability": {
            "schemaVersion":4,
            "pack":{"id":"fixture","version":"1","upstreamRevision":"fixture"},
            "extensionRevision":0, "permissionPolicySha256":"permission",
            "sandbox":{"mode":"workspace-write","network":"deny","writableRoots":[],"protectedPaths":[]},
            "profiles":[], "presets":[], "councils":[], "workflows":[], "skills":[]
        },
        "owner":{"sessionId":"session-goal","parentSessionId":null,"parentAttempt":null,"workflow":null,"workflowNode":null},
        "agent":{"name":"deep","sourceId":"fixture","definitionSha256":"definition","permissionSha256":"permission","promptPolicySha256":"prompt"},
        "model":{"providerId":"fixture","modelId":"fixture","wireModelId":"fixture","surface":"responses","reasoningSha256":"reasoning","preset":null},
        "selectedSkills":[],
        "prompt":{"eventId":null,"assemblySha256":"assembly","actualSha256":"actual"},
        "tools":[]
    })).expect("typed context snapshot")))
}

#[tokio::test]
async fn required_goal_input_publishes_typed_origin_and_revision_then_yields() {
    let port = Arc::new(RecordingPort::default());
    let tool = erase(GoalRequestInputTool::new(port.clone()));
    let mut ctx = context_with_turn();
    // A tool's public convenience fields cannot replace the authenticated call
    // coordinates retained by ToolContext.
    ctx.session_id = "untrusted-session".to_owned();
    ctx.message_id = "untrusted-message".to_owned();
    ctx.call_id = "untrusted-call".to_owned();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tool.execute(params(), ctx),
    )
    .await
    .expect("no human wait")
    .expect("publish Goal input");
    let opened = port.opened.lock().expect("published specs");
    assert_eq!(opened.len(), 1);
    let spec = &opened[0];
    assert_eq!(spec.origin.session_id, "session-goal");
    assert_eq!(spec.origin.message_id.as_deref(), Some("message-goal"));
    assert_eq!(spec.origin.call_id.as_deref(), Some("call-goal"));
    assert_eq!(spec.origin.turn_id.as_deref(), Some("turn-from-context"));
    assert_eq!(
        spec.origin.goal_id, None,
        "the service resolves the Goal inside its transaction"
    );
    assert_eq!(spec.expected_goal_revision, Some(42));
    assert_eq!(spec.mode, QuestionMode::Blocking);
    assert_eq!(spec.purpose, QuestionPurpose::RequiredInput);
    assert_eq!(spec.plan, None);
    assert_eq!(spec.questions.len(), 1);
    let question = &spec.questions[0];
    assert_eq!(question.question, "Which deployment target — 部署到哪里?");
    assert_eq!(question.header, "Target");
    assert_eq!(question.multiple, Some(true));
    assert_eq!(question.custom, Some(true));
    assert_eq!(
        question.options[0],
        QuestionOption::new("Staging", "Verify before production")
    );
    assert_eq!(question.options[1].label, "Production");
    assert_eq!(output.continuation, ToolContinuation::WaitingForHuman);
    assert_eq!(
        output.metadata[METADATA_HUMAN_REQUEST_ID_KEY],
        "que_assigned_by_service"
    );
    assert_eq!(output.metadata["questionRevision"], 7);
    assert_eq!(output.metadata["questionStatus"], "pending");
    assert_eq!(output.metadata["questionPurpose"], "required_input");
    assert_eq!(output.metadata["questionDuplicate"], false);
    assert!(!output.metadata.contains_key("humanRequest"));
    assert_eq!(tool.replay_policy(), ToolReplayPolicy::Never);
    assert_eq!(tool.effect(&params()), ToolEffect::UserMediated);
}

#[tokio::test]
async fn publication_receipts_never_repeat_even_an_already_answered_duplicate() {
    let secret = "answer delivered only in the durable inbox";
    let port = Arc::new(RecordingPort {
        state: QuestionState::Answered,
        answers: [("q1".to_owned(), vec![secret.to_owned()])]
            .into_iter()
            .collect(),
        duplicate: true,
        ..RecordingPort::default()
    });
    let output = erase(GoalRequestInputTool::new(port.clone()))
        .execute(params(), context())
        .await
        .expect("duplicate publication receipt");
    assert_eq!(output.title, "Goal input receipt");
    assert_eq!(output.continuation, ToolContinuation::WaitingForHuman);
    assert_eq!(output.metadata["questionStatus"], "answered");
    assert_eq!(output.metadata["questionDuplicate"], true);
    assert!(!output.output.contains(secret));
    assert!(
        !serde_json::to_string(&output.metadata)
            .expect("metadata")
            .contains(secret)
    );
    assert!(!output.metadata.contains_key("answers"));
    assert_eq!(
        port.opened.lock().expect("one open")[0].origin.turn_id,
        None
    );
}

#[tokio::test]
async fn cancelled_receipts_are_not_reported_as_answers_or_pending_requests() {
    let port = Arc::new(RecordingPort {
        state: QuestionState::Cancelled,
        ..RecordingPort::default()
    });
    let output = erase(GoalRequestInputTool::new(port))
        .execute(params(), context())
        .await
        .expect("cancelled receipt");
    assert_eq!(output.metadata["questionStatus"], "cancelled");
    assert!(output.output.contains("(cancelled)"));
    assert_eq!(output.continuation, ToolContinuation::WaitingForHuman);
}

#[tokio::test]
async fn invalid_goal_question_arguments_never_reach_the_question_port() {
    let port = Arc::new(RecordingPort::default());
    let tool = erase(GoalRequestInputTool::new(port.clone()));
    let mut invalids = Vec::new();
    for (field, value) in [
        ("expected_revision", json!(0)),
        ("question", json!(" ")),
        ("header", json!("h".repeat(31))),
        ("options", json!([])),
        (
            "options",
            json!([
                {"label":"Same","description":"one"},
                {"label":" Same ","description":"two"}
            ]),
        ),
        ("goal_id", json!("model-chosen-goal")),
        ("purpose", json!("plan_authorization")),
        ("mode", json!("deferred")),
    ] {
        let mut input = params();
        input[field] = value;
        invalids.push(input);
    }
    for input in invalids {
        let error = tool
            .execute(input.clone(), context())
            .await
            .expect_err("invalid arguments");
        assert!(
            matches!(error, ToolError::InvalidArgs { .. }),
            "{input}: {error:?}"
        );
    }
    assert!(port.opened.lock().expect("no publication").is_empty());
}

#[tokio::test]
async fn service_goal_revision_rejections_keep_the_typed_source() {
    let port = Arc::new(RecordingPort {
        error: Mutex::new(Some(QuestionError::Rejected {
            code: "goal_revision_conflict",
            detail: "the Goal changed before the publication transaction".to_owned(),
        })),
        ..RecordingPort::default()
    });
    let error = erase(GoalRequestInputTool::new(port.clone()))
        .execute(params(), context())
        .await
        .expect_err("service rejects the stale Goal");
    let ToolError::Failed { source, .. } = error else {
        panic!("service failure must retain its source");
    };
    assert!(matches!(
        source.downcast_ref::<QuestionError>(),
        Some(QuestionError::Rejected {
            code: "goal_revision_conflict",
            ..
        })
    ));
    assert_eq!(
        port.opened.lock().expect("published specs")[0].expected_goal_revision,
        Some(42)
    );
}

#[tokio::test]
async fn service_database_failures_remain_typed_and_are_not_rendered_into_fake_receipts() {
    let port = Arc::new(RecordingPort {
        error: Mutex::new(Some(QuestionError::Database(zuno_error::DbError::Busy {
            retry_after: Some(std::time::Duration::from_millis(80)),
        }))),
        ..RecordingPort::default()
    });
    let error = erase(GoalRequestInputTool::new(port))
        .execute(params(), context())
        .await
        .expect_err("publication did not commit");
    let ToolError::Failed { source, .. } = error else {
        panic!("preserve the typed service failure");
    };
    assert!(matches!(
        source.downcast_ref::<QuestionError>(),
        Some(QuestionError::Database(zuno_error::DbError::Busy { retry_after: Some(delay) }))
            if *delay == std::time::Duration::from_millis(80)
    ));
}
