use std::sync::Arc;

use serde_json::json;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox};
use zuno_db::session_execution::SessionExecutionStore;
use zuno_session_control::{QuestionService, SessionControlService};
use zuno_tool::question::QuestionPort;
use zuno_types::execution::{
    CollaborationMode, InputTriggerKind, SessionExecutionPhase, SessionPauseReason,
    SessionReadiness, SessionScheduling, TurnExecutionIdentity,
};
use zuno_types::question::{
    QuestionAction, QuestionCommand, QuestionMode, QuestionOption, QuestionOrigin, QuestionPurpose,
    QuestionRequest, QuestionSpec, QuestionState,
};

use super::{DurableInputScope, durable_questions};

const SESSION: &str = "ses_acp_questions";

fn pool() -> Arc<zuno_db::Pool> {
    let pool = Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("pool"));
    let mut connection = pool.get().expect("connection");
    zuno_db::migration::apply(&mut connection).expect("schema");
    connection
        .execute_batch(
            "INSERT INTO project(id,worktree,time_created,time_updated,sandboxes)
         VALUES ('project','/workspace',1,1,'[]');
         INSERT INTO session(id,project_id,slug,directory,title,version,time_created,time_updated)
         VALUES ('ses_acp_questions','project','acp','/workspace','ACP','test',1,1);",
        )
        .expect("session");
    drop(connection);
    let spill = tempfile::tempdir().expect("Goal fixture spill directory");
    zuno_goal::GoalStore::from_pool(Arc::clone(&pool), spill.path().to_path_buf())
        .expect("Goal schema");
    pool
}

fn paused(pool: &Arc<zuno_db::Pool>) -> zuno_types::execution::SessionExecutionState {
    let store = SessionExecutionStore::new(Arc::clone(pool));
    let mut state = store
        .seed(
            SESSION,
            CollaborationMode::Work,
            Some(TurnExecutionIdentity::new("build", "provider", "model")),
            10,
        )
        .expect("execution state");
    state.phase = SessionExecutionPhase::Paused;
    state.cycle_id = Some("current-cycle".to_owned());
    state.scheduling = Some(SessionScheduling {
        readiness: SessionReadiness::Paused {
            reason: SessionPauseReason::NoProgress,
        },
        progress_fingerprint: Some("unchanged-progress".to_owned()),
        unchanged_progress_count: 3,
    });
    store.update(state.revision, state).expect("paused")
}

fn spec() -> QuestionSpec {
    QuestionSpec {
        origin: QuestionOrigin {
            session_id: SESSION.to_owned(),
            message_id: None,
            call_id: None,
            turn_id: None,
            goal_id: None,
        },
        mode: QuestionMode::Deferred,
        purpose: QuestionPurpose::Clarification,
        questions: vec![
            QuestionRequest::closed(
                "Which approach?",
                "Approach",
                vec![
                    QuestionOption::new("Keep", "Keep this approach"),
                    QuestionOption::new("Replace", "Choose another approach"),
                ],
            ),
            QuestionRequest::closed(
                "When?",
                "Timing",
                vec![QuestionOption::new("Later", "Answer later")],
            ),
        ],
        expected_goal_revision: None,
        plan: None,
    }
}

#[tokio::test]
async fn native_list_and_empty_response_preserve_no_progress_and_pending_questions() {
    let pool = pool();
    let before = paused(&pool);
    let service = QuestionService::new(Arc::clone(&pool));
    let opened = service.open(spec()).await.expect("publish question");
    let listed = durable_questions::list(&service, &json!({"sessionId": SESSION}))
        .await
        .expect("list handler");
    assert_eq!(listed["questions"][0]["id"], opened.question.id);
    let mut changes = service.subscribe();
    let command = QuestionCommand {
        command_id: "empty-response".to_owned(),
        expected_revision: opened.question.revision,
        action: QuestionAction::Answer {
            answers: Default::default(),
        },
    };
    let answered = durable_questions::respond(
        &service,
        &json!({
            "sessionId": SESSION, "requestId": opened.question.id, "command": command,
        }),
    )
    .await
    .expect("respond handler");
    assert_eq!(answered["question"]["state"], "pending");
    assert_eq!(service.pending(SESSION).await.expect("pending").len(), 1);
    let notification = changes.try_recv().expect("committed change broadcast");
    assert_eq!(notification.question.id, opened.question.id);
    assert!(notification.input_id.is_none());
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let queued = inbox.pending(SESSION).expect("retained input");
    assert!(
        queued.is_empty(),
        "empty answers must not enter the model inbox"
    );
    for _ in 0..3 {
        assert!(
            durable_questions::next_input(&inbox, SESSION, DurableInputScope::Automatic)
                .expect("scheduling check")
                .is_none()
        );
    }
    assert_eq!(
        SessionExecutionStore::new(pool)
            .get(SESSION)
            .expect("execution"),
        Some(before),
        "query, empty response or wake scan resumed ordinary work",
    );
}

#[tokio::test]
async fn partial_native_response_is_revisioned_and_does_not_reopen_its_form() {
    let service = QuestionService::new(pool());
    let opened = service.open(spec()).await.expect("publish");
    let mut ledger = durable_questions::PresentationLedger::default();
    assert!(ledger.claim(&opened.question));
    let command = QuestionCommand {
        command_id: "partial-response".to_owned(),
        expected_revision: opened.question.revision,
        action: QuestionAction::Answer {
            answers: std::collections::BTreeMap::from([(
                opened.question.questions[0].id.clone(),
                vec!["Keep".to_owned()],
            )]),
        },
    };
    let params = json!({
        "sessionId": SESSION, "requestId": opened.question.id, "command": command,
    });
    let result = durable_questions::respond(&service, &params)
        .await
        .expect("partial response");
    let receipt: zuno_types::question::QuestionReceipt =
        serde_json::from_value(result).expect("receipt");
    assert_eq!(receipt.question.state, QuestionState::Pending);
    assert_eq!(receipt.question.answers.len(), 1);
    assert!(receipt.question.revision > opened.question.revision);
    assert!(
        !ledger.claim(&receipt.question),
        "partial response reopened the modal"
    );
    assert!(
        durable_questions::PresentationLedger::default().claim(&receipt.question),
        "a restarted consumer must replay the pending question"
    );
    let replay = durable_questions::respond(&service, &params)
        .await
        .expect("idempotent replay");
    assert_eq!(replay["duplicate"], true);
}

#[tokio::test]
async fn native_deferred_drafts_are_stored_but_never_enter_model_inbox_content() {
    let pool = pool();
    let service = QuestionService::new(Arc::clone(&pool));
    let opened = service.open(spec()).await.expect("publish");
    let item = &opened.question.questions[0].id;
    let result = durable_questions::respond(
        &service,
        &json!({
            "sessionId":SESSION, "requestId":opened.question.id,
            "command":{
                "commandId":"draft-only", "expectedRevision":opened.question.revision,
                "action":{"type":"defer","draftAnswers":{item: ["Replace"]}},
            },
        }),
    )
    .await
    .expect("save draft");
    let receipt: zuno_types::question::QuestionReceipt =
        serde_json::from_value(result).expect("receipt");
    assert_eq!(receipt.question.state, QuestionState::Pending);
    assert!(receipt.question.answers.is_empty());
    assert_eq!(
        receipt.question.draft_answers.get(item),
        Some(&vec!["Replace".to_owned()])
    );
    assert!(
        receipt.input_id.is_none(),
        "draft-only deferral admitted model input"
    );
    assert!(
        SessionInbox::new(pool)
            .pending(SESSION)
            .expect("inbox")
            .is_empty()
    );
}

#[tokio::test]
async fn native_response_reports_revision_conflicts_and_rejects_untyped_commands() {
    let service = QuestionService::new(pool());
    let opened = service.open(spec()).await.expect("publish");
    let make = |id: &str| {
        json!({
            "sessionId": SESSION,
            "requestId": opened.question.id,
            "command": {
                "commandId": id, "expectedRevision": opened.question.revision,
                "action": {"type": "defer"},
            },
        })
    };
    durable_questions::respond(&service, &make("defer-1"))
        .await
        .expect("defer");
    let error = durable_questions::respond(&service, &make("defer-2"))
        .await
        .expect_err("stale revision");
    assert_eq!(error.code, -32003);
    assert_eq!(
        error.data.expect("CAS details")["kind"],
        "question_revision_conflict"
    );
    for params in [
        json!({"sessionId": SESSION, "requestId": opened.question.id, "answers": []}),
        json!({"sessionId": SESSION, "requestId": opened.question.id, "command": {
            "commandId": "unsafe", "expectedRevision": 2, "action": {"type": "plan_decision"},
        }}),
    ] {
        assert_eq!(
            durable_questions::respond(&service, &params)
                .await
                .expect_err("typed command required")
                .code,
            -32602
        );
    }
}

#[test]
fn rejected_reports_are_retained_and_do_not_starve_an_admissible_query() {
    let pool = pool();
    let before = paused(&pool);
    let inbox = SessionInbox::new(Arc::clone(&pool));
    let report = inbox
        .admit(
            NewSessionInput::new(
                "report-1",
                SESSION,
                json!({"kind":"backgroundExecutionReport","executionID":"bg-1","text":"finished"}),
                InputDelivery::Queue,
                20,
            )
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(Some("current-cycle")),
        )
        .expect("report");
    let query = inbox
        .admit(NewSessionInput::new(
            "query-1",
            SESSION,
            json!({"kind":"acpPrompt","text":"What is the status?","content":[]}),
            InputDelivery::Queue,
            21,
        ))
        .expect("query");
    assert!(DurableInputScope::Automatic.admits(&report).is_some());
    assert!(
        durable_questions::next_input(&inbox, SESSION, DurableInputScope::Automatic)
            .expect("gate")
            .is_none()
    );
    let (selected, _) = durable_questions::next_input(&inbox, SESSION, DurableInputScope::Prompts)
        .expect("query gate")
        .expect("query can be answered without resuming work");
    assert_eq!(selected.id, query.id);
    assert_eq!(
        inbox
            .get(SESSION, &report.id)
            .expect("report")
            .expect("retained")
            .state,
        report.state
    );
    assert_eq!(
        SessionExecutionStore::new(pool)
            .get(SESSION)
            .expect("state"),
        Some(before)
    );
}

#[test]
fn explicit_resume_work_without_a_plan_is_a_drivable_control() {
    let pool = pool();
    let before = paused(&pool);
    let resumed = SessionControlService::new(Arc::clone(&pool))
        .resume_session(SESSION, before.revision, 30)
        .expect("explicit resume");
    assert_eq!(resumed.input.prompt["control"], "resume_work");
    let decoded = DurableInputScope::Controls
        .admits(&resumed.input)
        .expect("Work control");
    assert!(
        decoded
            .work_control
            .expect("continuation")
            .plan_id
            .is_none()
    );
    let inbox = SessionInbox::new(pool);
    let (input, _) = durable_questions::next_input(&inbox, SESSION, DurableInputScope::Controls)
        .expect("control gate")
        .expect("resume is admitted");
    assert_eq!(input.id, resumed.input.id);
}

#[test]
fn native_question_capabilities_name_real_handlers_and_resume_rejection_maps_to_client_error() {
    let capabilities = durable_questions::capabilities();
    assert_eq!(capabilities["listMethod"], durable_questions::LIST_METHOD);
    assert_eq!(
        capabilities["respondMethod"],
        durable_questions::RESPOND_METHOD
    );
    let error = super::session_control_rpc_error(
        zuno_session_control::SessionControlError::ResumeRejected {
            session_id: SESSION.to_owned(),
            detail: "exact human wait is still pending".to_owned(),
        },
    );
    assert_eq!(error.code, -32602);
}
