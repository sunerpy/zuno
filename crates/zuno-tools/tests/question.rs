//! Tool publication and receipts over the native durable QuestionPort.

mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use support::question::{
    OTHER_SESSION_ID, QuestionFixture, SESSION_ID, TURN_ID, context, context_with_parent,
    plan_binding,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zuno_db::event_log::SessionEventLog;
use zuno_db::human_request::HumanRequestState;
use zuno_db::session_execution::SessionExecutionStore;
use zuno_error::ToolError;
use zuno_tool::question::{QuestionError, QuestionPort};
use zuno_tool::{
    InterruptHandle, METADATA_HUMAN_REQUEST_ID_KEY, QuestionResultPresentation,
    QuestionResultStatus, Tool, ToolContinuation, ToolOutput, ToolResultPresentation, erase,
};
use zuno_tools::plan_exit::PlanExitTool;
use zuno_tools::question::QuestionTool;
use zuno_types::execution::CollaborationMode;
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionDecision, QuestionAction, QuestionAnswers, QuestionCommand,
    QuestionMode, QuestionPurpose, QuestionState, QuestionView,
};

const PRIVATE_ANSWER: &str = "human-answer-delivered-once-through-the-inbox";
const PRIVATE_DRAFT: &str = "unsubmitted-draft-never-visible-to-the-model";

fn one_question() -> Value {
    json!({"questions":[{
        "question":"Which database?",
        "header":"Database",
        "options":[
            {"label":"Postgres","description":"Relational"},
            {"label":"SQLite","description":"Embedded"}
        ]
    }]})
}

fn command(id: &str, view: &QuestionView, action: QuestionAction) -> QuestionCommand {
    QuestionCommand {
        command_id: id.to_owned(),
        expected_revision: view.revision,
        action,
    }
}

fn answer(view: &QuestionView, item: usize, value: &str) -> QuestionAction {
    QuestionAction::Answer {
        answers: [(view.questions[item].id.clone(), vec![value.to_owned()])].into(),
    }
}

fn request_id(output: &ToolOutput) -> &str {
    output.metadata[METADATA_HUMAN_REQUEST_ID_KEY]
        .as_str()
        .expect("receipt references its durable request")
}

fn assert_no_inputs(fixture: &QuestionFixture) {
    let pool = fixture.pool();
    let count = pool
        .get()
        .expect("inbox connection")
        .query_row(
            "SELECT count(*) FROM session_input WHERE session_id = ?1",
            [SESSION_ID],
            |row| row.get::<_, i64>(0),
        )
        .expect("all persisted model inputs");
    assert_eq!(
        count, 0,
        "no input may be admitted, even as settled history"
    );
}

async fn assert_no_questions(fixture: &QuestionFixture) {
    let pending = fixture.port.pending(SESSION_ID).await.expect("pending");
    assert!(pending.is_empty(), "{pending:?}");
}

fn assert_receipt(output: &ToolOutput, view: &QuestionView, status: QuestionResultStatus) {
    assert_eq!(request_id(output), view.id);
    assert_eq!(output.metadata["questionStatus"], status.as_str());
    assert_eq!(output.metadata["questionCount"], view.questions.len());
    assert_eq!(output.metadata["questionPurpose"], view.purpose.as_str());
    assert!(output.output.contains(&view.id));
    assert!(output.title.starts_with(status.label()));
    assert!(!output.metadata.contains_key("answers"));
    assert!(!output.metadata.contains_key("draftAnswers"));
    let Some(ToolResultPresentation::Question(presentation)) = &output.presentation else {
        panic!("receipt needs a typed question presentation");
    };
    assert_eq!(presentation.status(), status);
    assert_eq!(presentation.question_count(), view.questions.len());
    assert_eq!(presentation.answers(), None);
    assert_eq!(output.metadata["elapsedMs"], presentation.elapsed_ms());
    let persisted = serde_json::to_value(output).expect("durable tool result");
    assert!(persisted.get("presentation").is_none());
    assert!(
        !persisted.to_string().contains(PRIVATE_ANSWER),
        "the tool receipt must not duplicate the human response"
    );
    assert!(
        !persisted.to_string().contains(PRIVATE_DRAFT),
        "unsubmitted form values must not enter the tool result"
    );
}

async fn finish(task: JoinHandle<Result<ToolOutput, ToolError>>) -> ToolOutput {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("question tool must return after the human event")
        .expect("tool task")
        .expect("receipt")
}

#[derive(Clone)]
struct TestInterrupt(CancellationToken);

#[async_trait]
impl InterruptHandle for TestInterrupt {
    fn is_set(&self) -> bool {
        self.0.is_cancelled()
    }

    async fn notified(&self) {
        self.0.cancelled().await;
    }
}

#[tokio::test]
async fn async_publication_is_durable_without_waiting_answering_or_stopping_the_turn() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::asynchronous(fixture.shared_port()));
    let output = tool
        .invoke(one_question(), context("call_async"))
        .await
        .expect("publication receipt");
    let view = fixture
        .reopen()
        .get(SESSION_ID, request_id(&output))
        .await
        .expect("another pool can inspect the question");
    assert_receipt(&output, &view, QuestionResultStatus::Pending);
    assert_eq!(view.state, QuestionState::Pending);
    assert_eq!(view.mode, QuestionMode::Deferred);
    assert!(view.answers.is_empty());
    assert!(view.draft_answers.is_empty());
    assert_eq!(view.origin.session_id, SESSION_ID);
    assert_eq!(view.origin.message_id.as_deref(), Some("msg_question"));
    assert_eq!(view.origin.call_id.as_deref(), Some("call_async"));
    assert_eq!(view.origin.turn_id.as_deref(), Some(TURN_ID));
    assert_eq!(view.origin.goal_id, None);
    assert!(view.questions[0].question.allows_custom());
    assert_eq!(fixture.port.wait_count(), 0);
    assert_eq!(output.continuation, ToolContinuation::Continue);
    assert_no_inputs(&fixture);
    let published = fixture.port.opened();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].purpose, QuestionPurpose::Clarification);
    assert_eq!(published[0].mode, QuestionMode::Deferred);
    assert_eq!(published[0].plan, None);
}

#[tokio::test]
async fn a_blocking_answer_is_delivered_once_in_the_inbox_and_never_in_the_tool_result() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::new(fixture.shared_port()));
    let task =
        tokio::spawn(async move { tool.invoke(one_question(), context("call_blocking")).await });
    let opened = fixture.port.wait_for_request("call_blocking").await;
    assert!(
        !task.is_finished(),
        "publication must not fabricate an answer"
    );
    assert_eq!(opened.mode, QuestionMode::Blocking);
    let applied = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command("client_answer", &opened, answer(&opened, 0, PRIVATE_ANSWER)),
        )
        .await
        .expect("durable human response");
    let output = finish(task).await;
    assert_receipt(&output, &applied.question, QuestionResultStatus::Answered);
    assert_eq!(output.continuation, ToolContinuation::Continue);
    assert_eq!(fixture.port.wait_count(), 1);
    let inputs = fixture.inbox().pending(SESSION_ID).expect("durable inbox");
    assert_eq!(inputs.len(), 1);
    assert_eq!(Some(inputs[0].id.clone()), applied.input_id);
    assert_eq!(inputs[0].prompt["kind"], "humanRequestAnswer");
    assert_eq!(inputs[0].prompt["requestID"], opened.id);
    assert_eq!(
        inputs[0].prompt["response"]["answers"][&opened.questions[0].id],
        json!([PRIVATE_ANSWER])
    );
    assert_eq!(
        fixture
            .reopen()
            .get(SESSION_ID, &opened.id)
            .await
            .expect("reopened question"),
        applied.question
    );
}

#[tokio::test]
async fn repeated_publication_and_reply_reuse_the_request_and_the_durable_input() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::asynchronous(fixture.shared_port()));
    let first = tool
        .invoke(one_question(), context("call_retry"))
        .await
        .expect("first publication");
    let repeated = tool
        .invoke(one_question(), context("call_retry"))
        .await
        .expect("idempotent publication");
    assert_eq!(request_id(&first), request_id(&repeated));
    let opened = fixture
        .port
        .get(SESSION_ID, request_id(&first))
        .await
        .expect("pending question");
    assert_eq!(opened.revision, 1);
    let submitted = command("client_retry", &opened, answer(&opened, 0, PRIVATE_ANSWER));
    let applied = fixture
        .port
        .apply(SESSION_ID, &opened.id, submitted.clone())
        .await
        .expect("first reply");
    let duplicate = fixture
        .reopen()
        .apply(SESSION_ID, &opened.id, submitted)
        .await
        .expect("retry from a fresh connection");
    assert!(duplicate.duplicate);
    assert_eq!(applied.input_id, duplicate.input_id);
    let after_answer = tool
        .invoke(one_question(), context("call_retry"))
        .await
        .expect("recovered tool receipt");
    assert_receipt(
        &after_answer,
        &applied.question,
        QuestionResultStatus::Answered,
    );
    assert_eq!(fixture.inbox().pending(SESSION_ID).expect("inbox").len(), 1);
    let events = SessionEventLog::new(fixture.pool())
        .read_after(SESSION_ID, None)
        .expect("question audit");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "question.opened")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "question.updated")
            .count(),
        1
    );
}

#[tokio::test]
async fn a_partial_reply_releases_the_waiter_and_preserves_unanswered_items_for_later() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::new(fixture.shared_port()));
    let task = tokio::spawn(async move {
        tool.invoke(
            json!({"questions":[
                {"question":"Which environment?","header":"Environment"},
                {"question":"Which terminals?","header":"Terminal","multiple":true}
            ]}),
            context("call_partial"),
        )
        .await
    });
    let opened = fixture.port.wait_for_request("call_partial").await;
    let selected = vec!["PowerShell".to_owned(), "Bash".to_owned()];
    let partial = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "partial",
                &opened,
                QuestionAction::Answer {
                    answers: [(opened.questions[1].id.clone(), selected.clone())].into(),
                },
            ),
        )
        .await
        .expect("partial reply");
    let output = finish(task).await;
    assert_receipt(&output, &partial.question, QuestionResultStatus::Deferred);
    assert_eq!(partial.question.state, QuestionState::Pending);
    assert_eq!(partial.question.mode, QuestionMode::Deferred);
    assert!(
        !partial
            .question
            .answers
            .contains_key(&opened.questions[0].id)
    );
    assert_eq!(output.continuation, ToolContinuation::Continue);
    let completed = fixture
        .reopen()
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "remaining",
                &partial.question,
                answer(&partial.question, 0, PRIVATE_ANSWER),
            ),
        )
        .await
        .expect("answer the remaining item after the tool returned");
    assert_eq!(completed.question.state, QuestionState::Answered);
    assert_eq!(completed.question.questions, opened.questions);
    assert_eq!(
        completed.question.answers[&opened.questions[1].id],
        selected
    );
    assert_eq!(fixture.inbox().pending(SESSION_ID).expect("inbox").len(), 2);
}

#[tokio::test]
async fn deferral_and_empty_submission_leave_the_question_open_without_an_invented_answer() {
    for action in [
        QuestionAction::Defer {
            draft_answers: QuestionAnswers::new(),
        },
        QuestionAction::Answer {
            answers: QuestionAnswers::new(),
        },
    ] {
        let fixture = QuestionFixture::new();
        let tool = erase(QuestionTool::new(fixture.shared_port()));
        let task =
            tokio::spawn(async move { tool.invoke(one_question(), context("call_defer")).await });
        let opened = fixture.port.wait_for_request("call_defer").await;
        let receipt = fixture
            .port
            .apply(SESSION_ID, &opened.id, command("defer", &opened, action))
            .await
            .expect("defer");
        let output = finish(task).await;
        assert_receipt(&output, &receipt.question, QuestionResultStatus::Deferred);
        assert_eq!(receipt.question.state, QuestionState::Pending);
        assert!(receipt.question.answers.is_empty());
        assert!(receipt.question.draft_answers.is_empty());
        assert_eq!(receipt.input_id, None);
        assert_eq!(receipt.question.decision, None);
        assert_eq!(output.continuation, ToolContinuation::Continue);
        assert_eq!(
            fixture
                .port
                .pending(SESSION_ID)
                .await
                .expect("still pending"),
            [receipt.question]
        );
        assert_no_inputs(&fixture);
    }
}

#[tokio::test]
async fn a_saved_draft_releases_the_waiter_and_survives_reopen_without_model_input() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::new(fixture.shared_port()));
    let task = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move { tool.invoke(one_question(), context("call_draft")).await }
    });
    let opened = fixture.port.wait_for_request("call_draft").await;
    let draft_answers = QuestionAnswers::from([(
        opened.questions[0].id.clone(),
        vec![PRIVATE_DRAFT.to_owned()],
    )]);
    let save = command(
        "save_draft",
        &opened,
        QuestionAction::Defer {
            draft_answers: draft_answers.clone(),
        },
    );
    let saved = fixture
        .port
        .apply(SESSION_ID, &opened.id, save.clone())
        .await
        .expect("save unsubmitted values");
    assert_eq!(saved.input_id, None);
    assert_eq!(saved.question.state, QuestionState::Pending);
    assert_eq!(saved.question.mode, QuestionMode::Deferred);
    assert!(saved.question.answers.is_empty());
    assert_eq!(saved.question.draft_answers, draft_answers);
    assert!(!saved.question.is_fully_answered());
    assert_no_inputs(&fixture);
    let output = finish(task).await;
    assert_receipt(&output, &saved.question, QuestionResultStatus::Deferred);
    assert_eq!(output.continuation, ToolContinuation::Continue);

    let reopened = fixture.reopen();
    assert_eq!(
        reopened
            .get(SESSION_ID, &opened.id)
            .await
            .expect("saved form"),
        saved.question
    );
    let repeated = reopened
        .apply(SESSION_ID, &opened.id, save)
        .await
        .expect("retry draft save");
    assert!(repeated.duplicate);
    assert_eq!(repeated.input_id, None);
    assert_eq!(repeated.question, saved.question);
    let receipt = tool
        .invoke(one_question(), context("call_draft"))
        .await
        .expect("recover a deferred receipt");
    assert_receipt(&receipt, &saved.question, QuestionResultStatus::Deferred);
    assert_eq!(fixture.port.wait_count(), 1);
    assert_no_inputs(&fixture);

    let confirmed = reopened
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "confirm_answer",
                &saved.question,
                answer(&saved.question, 0, PRIVATE_ANSWER),
            ),
        )
        .await
        .expect("explicitly submit the chosen answer");
    assert_eq!(confirmed.question.state, QuestionState::Answered);
    assert!(confirmed.question.draft_answers.is_empty());
    assert_eq!(
        confirmed.question.answers[&opened.questions[0].id],
        [PRIVATE_ANSWER]
    );
    let inputs = fixture
        .inbox()
        .pending(SESSION_ID)
        .expect("confirmed input");
    assert_eq!(inputs.len(), 1);
    assert_eq!(Some(inputs[0].id.clone()), confirmed.input_id);
    assert!(inputs[0].prompt.to_string().contains(PRIVATE_ANSWER));
    assert!(!inputs[0].prompt.to_string().contains(PRIVATE_DRAFT));
}

#[tokio::test]
async fn drafts_merge_without_replacing_confirmed_answers_or_committing_other_items() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::asynchronous(fixture.shared_port()));
    let questions = json!({"questions":[
        {"question":"Which environment?","header":"Environment"},
        {"question":"Which terminal?","header":"Terminal"}
    ]});
    let published = tool
        .invoke(questions.clone(), context("call_draft_merge"))
        .await
        .expect("publish");
    let opened = fixture
        .port
        .get(SESSION_ID, request_id(&published))
        .await
        .expect("question");
    let drafts = QuestionAnswers::from([
        (
            opened.questions[0].id.clone(),
            vec![PRIVATE_DRAFT.to_owned()],
        ),
        (
            opened.questions[1].id.clone(),
            vec![PRIVATE_DRAFT.to_owned()],
        ),
    ]);
    let saved = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "save_both",
                &opened,
                QuestionAction::Defer {
                    draft_answers: drafts,
                },
            ),
        )
        .await
        .expect("save both draft slots");
    assert_eq!(saved.input_id, None);
    assert_no_inputs(&fixture);
    let first = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "confirm_first",
                &saved.question,
                answer(&saved.question, 0, PRIVATE_ANSWER),
            ),
        )
        .await
        .expect("confirm only the first item");
    let confirmed = QuestionAnswers::from([(
        opened.questions[0].id.clone(),
        vec![PRIVATE_ANSWER.to_owned()],
    )]);
    let remaining_draft = QuestionAnswers::from([(
        opened.questions[1].id.clone(),
        vec![PRIVATE_DRAFT.to_owned()],
    )]);
    assert_eq!(first.question.answers, confirmed);
    assert_eq!(first.question.draft_answers, remaining_draft);
    assert_eq!(first.question.state, QuestionState::Pending);
    let original_inputs = fixture
        .inbox()
        .pending(SESSION_ID)
        .expect("confirmed input");
    assert_eq!(original_inputs.len(), 1);
    assert!(
        !original_inputs[0]
            .prompt
            .to_string()
            .contains(PRIVATE_DRAFT)
    );

    let mut view = first.question;
    for (id, action) in [
        (
            "empty_answer",
            QuestionAction::Answer {
                answers: QuestionAnswers::new(),
            },
        ),
        (
            "empty_defer",
            QuestionAction::Defer {
                draft_answers: QuestionAnswers::new(),
            },
        ),
    ] {
        let unchanged = fixture
            .reopen()
            .apply(SESSION_ID, &opened.id, command(id, &view, action))
            .await
            .expect("empty submission does not commit saved drafts");
        assert_eq!(unchanged.input_id, None);
        assert_eq!(unchanged.question.answers, confirmed);
        assert_eq!(unchanged.question.draft_answers, remaining_draft);
        assert_eq!(unchanged.question.state, QuestionState::Pending);
        view = unchanged.question;
    }
    let blanked = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "blank_draft",
                &view,
                QuestionAction::Defer {
                    draft_answers: [(opened.questions[0].id.clone(), Vec::new())].into(),
                },
            ),
        )
        .await
        .expect("record an unsubmitted blank over a confirmed answer");
    assert_eq!(blanked.input_id, None);
    assert_eq!(blanked.question.answers, confirmed);
    assert_eq!(
        blanked.question.draft_answers[&opened.questions[0].id],
        Vec::<String>::new()
    );
    assert_eq!(
        blanked.question.draft_answers[&opened.questions[1].id],
        [PRIVATE_DRAFT]
    );
    assert_eq!(
        fixture
            .inbox()
            .pending(SESSION_ID)
            .expect("unchanged model inputs"),
        original_inputs
    );
    let receipt = tool
        .invoke(questions, context("call_draft_merge"))
        .await
        .expect("inspect the current receipt");
    assert_receipt(&receipt, &blanked.question, QuestionResultStatus::Deferred);
}

#[tokio::test]
async fn host_expiry_or_failure_keeps_saved_drafts_inspectable_without_answering_them() {
    for (state, status) in [
        (HumanRequestState::Expired, QuestionResultStatus::Expired),
        (HumanRequestState::Failed, QuestionResultStatus::Failed),
    ] {
        let fixture = QuestionFixture::new();
        let tool = erase(QuestionTool::asynchronous(fixture.shared_port()));
        let published = tool
            .invoke(one_question(), context("call_draft_terminal"))
            .await
            .expect("publish");
        let opened = fixture.port.wait_for_request("call_draft_terminal").await;
        let saved = fixture
            .port
            .apply(
                SESSION_ID,
                request_id(&published),
                command(
                    "terminal_draft",
                    &opened,
                    QuestionAction::Defer {
                        draft_answers: [(
                            opened.questions[0].id.clone(),
                            vec![PRIVATE_DRAFT.to_owned()],
                        )]
                        .into(),
                    },
                ),
            )
            .await
            .expect("save draft");
        fixture.port.settle(&opened.id, state);
        let settled = fixture
            .reopen()
            .get(SESSION_ID, &opened.id)
            .await
            .expect("terminal form remains readable");
        assert_eq!(settled.state.as_str(), state.as_str());
        assert_eq!(settled.draft_answers, saved.question.draft_answers);
        assert!(settled.answers.is_empty());
        let receipt = tool
            .invoke(one_question(), context("call_draft_terminal"))
            .await
            .expect("terminal receipt");
        assert_receipt(&receipt, &settled, status);
        assert_no_inputs(&fixture);
    }
}

#[tokio::test]
async fn interrupting_a_wait_returns_a_pending_receipt_and_keeps_the_request_answerable() {
    let fixture = QuestionFixture::new();
    let interrupt = CancellationToken::new();
    let mut ctx = context("call_interrupt");
    ctx.interrupt = Arc::new(TestInterrupt(interrupt.clone()));
    let tool = erase(QuestionTool::new(fixture.shared_port()));
    let task = tokio::spawn(async move { tool.invoke(one_question(), ctx).await });
    let opened = fixture.port.wait_for_request("call_interrupt").await;
    interrupt.cancel();
    let output = finish(task).await;
    assert_receipt(&output, &opened, QuestionResultStatus::Pending);
    assert_eq!(output.continuation, ToolContinuation::Continue);
    assert_no_inputs(&fixture);
    let reopened = fixture.reopen();
    assert_eq!(
        reopened.get(SESSION_ID, &opened.id).await.expect("request"),
        opened
    );
    let late = reopened
        .apply(
            SESSION_ID,
            &opened.id,
            command("late_reply", &opened, answer(&opened, 0, PRIVATE_ANSWER)),
        )
        .await
        .expect("later answer");
    assert_eq!(late.question.state, QuestionState::Answered);
    assert_eq!(fixture.inbox().pending(SESSION_ID).expect("inbox").len(), 1);
}

#[tokio::test]
async fn dropping_a_blocking_tool_future_does_not_withdraw_its_durable_question() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::new(fixture.shared_port()));
    let task = tokio::spawn(async move { tool.invoke(one_question(), context("call_drop")).await });
    let opened = fixture.port.wait_for_request("call_drop").await;
    task.abort();
    assert!(
        task.await
            .expect_err("tool future was dropped")
            .is_cancelled()
    );
    let reopened = fixture.reopen();
    assert_eq!(
        reopened.get(SESSION_ID, &opened.id).await.expect("request"),
        opened
    );
    assert_no_inputs(&fixture);
    let receipt = reopened
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "reply_after_drop",
                &opened,
                answer(&opened, 0, PRIVATE_ANSWER),
            ),
        )
        .await
        .expect("durable request outlives the waiter");
    assert_eq!(receipt.question.state, QuestionState::Answered);
    assert!(receipt.input_id.is_some());
}

#[tokio::test]
async fn cancellation_expiry_and_delivery_failure_have_distinct_receipts_without_answers() {
    for (state, status) in [
        (
            HumanRequestState::Cancelled,
            QuestionResultStatus::Cancelled,
        ),
        (HumanRequestState::Expired, QuestionResultStatus::Expired),
        (HumanRequestState::Failed, QuestionResultStatus::Failed),
    ] {
        let fixture = QuestionFixture::new();
        let tool = erase(QuestionTool::new(fixture.shared_port()));
        let task =
            tokio::spawn(
                async move { tool.invoke(one_question(), context("call_terminal")).await },
            );
        let opened = fixture.port.wait_for_request("call_terminal").await;
        if state == HumanRequestState::Cancelled {
            fixture
                .port
                .apply(
                    SESSION_ID,
                    &opened.id,
                    command("cancel", &opened, QuestionAction::Cancel),
                )
                .await
                .expect("explicit cancellation");
        } else {
            fixture.port.settle(&opened.id, state);
        }
        let output = finish(task).await;
        let settled = fixture
            .reopen()
            .get(SESSION_ID, &opened.id)
            .await
            .expect("settled");
        assert_receipt(&output, &settled, status);
        assert_eq!(settled.state.as_str(), state.as_str());
        assert!(settled.answers.is_empty());
        assert_eq!(output.continuation, ToolContinuation::Continue);
        assert_no_questions(&fixture).await;
    }
}

#[tokio::test]
async fn model_arguments_cannot_write_the_question_origin_purpose_or_answer() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::asynchronous(fixture.shared_port()));
    for (field, value) in [
        ("origin", json!({"sessionId":OTHER_SESSION_ID})),
        ("purpose", json!("plan_authorization")),
        ("mode", json!("blocking")),
        ("answers", json!({"q1":["Approve"]})),
        ("draftAnswers", json!({"q1":["unsubmitted"]})),
        ("plan", json!({"planId":"forged","planRevision":1})),
        ("goalId", json!("forged_goal")),
    ] {
        let mut args = one_question();
        args[field] = value;
        assert!(
            tool.invoke(args, context("call_invalid")).await.is_err(),
            "{field}"
        );
    }
    let mut args = one_question();
    args["questions"][0]["custom"] = json!(false);
    assert!(tool.invoke(args, context("call_invalid")).await.is_err());
    assert!(fixture.port.opened().is_empty());
    assert_no_questions(&fixture).await;
}

#[tokio::test]
async fn invalid_question_definitions_do_not_publish_a_successful_receipt_or_inbox_input() {
    let fixture = QuestionFixture::new();
    let tool = erase(QuestionTool::asynchronous(fixture.shared_port()));
    for questions in [
        json!([]),
        json!([{"question":"","header":"Question"}]),
        json!([{"question":"Choose","header":"Question","options":[
            {"label":"Same","description":"First"},{"label":"Same","description":"Second"}
        ]}]),
    ] {
        let error = tool
            .invoke(
                json!({"questions":questions}),
                context("call_invalid_definition"),
            )
            .await
            .expect_err("invalid durable question");
        assert!(matches!(
            error,
            ToolError::Failed { .. } | ToolError::InvalidArgs { .. }
        ));
    }
    assert_no_questions(&fixture).await;
    assert_no_inputs(&fixture);
}

#[tokio::test]
async fn child_attempts_cannot_publish_clarifications_required_input_or_plan_approval() {
    let fixture = QuestionFixture::new();
    let tools: Vec<(Arc<dyn Tool>, Value)> = vec![
        (
            erase(QuestionTool::new(fixture.shared_port())),
            one_question(),
        ),
        (
            erase(QuestionTool::asynchronous(fixture.shared_port())),
            one_question(),
        ),
        (
            erase(QuestionTool::required(fixture.shared_port())),
            one_question(),
        ),
        (erase(PlanExitTool::new(fixture.shared_port())), json!({})),
    ];
    for (tool, args) in tools {
        let error = tool
            .invoke(
                args,
                context_with_parent("call_child", Some(OTHER_SESSION_ID)),
            )
            .await
            .expect_err("children cannot ask their own human questions");
        assert!(matches!(error, ToolError::Denied { .. }));
    }
    assert!(fixture.port.opened().is_empty());
    assert_no_questions(&fixture).await;
}

#[tokio::test]
async fn required_input_keeps_waiting_for_human_after_deferral_until_a_real_answer_exists() {
    let fixture = QuestionFixture::new();
    fixture.port.bind_required_goal("goal_required", 3);
    let tool = erase(QuestionTool::required(fixture.shared_port()));
    let task = tokio::spawn({
        let tool = Arc::clone(&tool);
        async move { tool.invoke(one_question(), context("call_required")).await }
    });
    let opened = fixture.port.wait_for_request("call_required").await;
    assert_eq!(opened.purpose, QuestionPurpose::RequiredInput);
    assert_eq!(opened.origin.goal_id.as_deref(), Some("goal_required"));
    let draft_answers = QuestionAnswers::from([(
        opened.questions[0].id.clone(),
        vec![PRIVATE_DRAFT.to_owned()],
    )]);
    let deferred = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "required_later",
                &opened,
                QuestionAction::Defer {
                    draft_answers: draft_answers.clone(),
                },
            ),
        )
        .await
        .expect("defer required input");
    let output = finish(task).await;
    assert_receipt(&output, &deferred.question, QuestionResultStatus::Deferred);
    assert_eq!(deferred.input_id, None);
    assert!(deferred.question.answers.is_empty());
    assert_eq!(deferred.question.draft_answers, draft_answers);
    assert_eq!(output.continuation, ToolContinuation::WaitingForHuman);
    assert_no_inputs(&fixture);
    let answered = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "required_answer",
                &deferred.question,
                answer(&deferred.question, 0, PRIVATE_ANSWER),
            ),
        )
        .await
        .expect("real required answer");
    let recovered = tool
        .invoke(one_question(), context("call_required"))
        .await
        .expect("recover the settled tool result");
    assert_receipt(
        &recovered,
        &answered.question,
        QuestionResultStatus::Answered,
    );
    assert_eq!(recovered.continuation, ToolContinuation::Continue);
    assert_eq!(fixture.port.wait_count(), 1);
    assert!(answered.question.draft_answers.is_empty());
    let inputs = fixture
        .inbox()
        .pending(SESSION_ID)
        .expect("confirmed input");
    assert_eq!(inputs.len(), 1);
    assert!(!inputs[0].prompt.to_string().contains(PRIVATE_DRAFT));
}

#[tokio::test]
async fn plan_exit_publishes_a_deferred_host_bound_question_without_authorizing_work() {
    let fixture = QuestionFixture::new();
    let binding = plan_binding();
    fixture.port.bind_plan(binding.clone());
    let execution = SessionExecutionStore::new(fixture.pool());
    let before = execution
        .seed(
            SESSION_ID,
            CollaborationMode::Plan,
            Some(binding.work_identity.clone()),
            10,
        )
        .expect("Plan mode");
    let tool = erase(PlanExitTool::new(fixture.shared_port()));
    let output = tool
        .invoke(json!({}), context("call_plan_exit"))
        .await
        .expect("deferred Plan confirmation");
    let view = fixture
        .reopen()
        .get(SESSION_ID, request_id(&output))
        .await
        .expect("durable Plan question");
    assert_receipt(&output, &view, QuestionResultStatus::Pending);
    assert_eq!(view.mode, QuestionMode::Deferred);
    assert_eq!(view.purpose, QuestionPurpose::PlanAuthorization);
    assert_eq!(binding.source_cycle_id, None);
    assert_eq!(view.plan, Some(binding));
    assert_eq!(view.decision, None);
    assert_eq!(view.authorization, None);
    assert!(view.answers.is_empty());
    assert!(!view.questions[0].question.allows_custom());
    assert_eq!(view.origin.turn_id.as_deref(), Some(TURN_ID));
    assert_eq!(fixture.port.wait_count(), 0);
    assert_eq!(output.continuation, ToolContinuation::Continue);
    assert_eq!(execution.get(SESSION_ID).expect("execution"), Some(before));
    assert_no_inputs(&fixture);
    let requests = fixture.port.opened();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].purpose, QuestionPurpose::PlanAuthorization);
    assert_eq!(requests[0].mode, QuestionMode::Deferred);
    assert!(requests[0].questions.is_empty());
    assert_eq!(
        requests[0].plan, None,
        "the host supplies the trusted binding"
    );
}

#[tokio::test]
async fn an_unsubmitted_plan_approval_choice_is_only_a_draft_and_cannot_start_work() {
    let fixture = QuestionFixture::new();
    let binding = plan_binding();
    fixture.port.bind_plan(binding.clone());
    let execution = SessionExecutionStore::new(fixture.pool());
    let before = execution
        .seed(
            SESSION_ID,
            CollaborationMode::Plan,
            Some(binding.work_identity),
            10,
        )
        .expect("Plan mode");
    let tool = erase(PlanExitTool::new(fixture.shared_port()));
    let published = tool
        .invoke(json!({}), context("call_plan_draft"))
        .await
        .expect("publish Plan confirmation");
    let opened = fixture
        .port
        .get(SESSION_ID, request_id(&published))
        .await
        .expect("Plan question");
    let drafts = QuestionAnswers::from([(
        opened.questions[0].id.clone(),
        vec![opened.questions[0].question.options[0].label.clone()],
    )]);
    let saved = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "save_plan_choice",
                &opened,
                QuestionAction::Defer {
                    draft_answers: drafts.clone(),
                },
            ),
        )
        .await
        .expect("save the unsubmitted form choice");
    assert_eq!(saved.input_id, None);
    assert_eq!(saved.question.state, QuestionState::Pending);
    assert_eq!(saved.question.draft_answers, drafts);
    assert!(saved.question.answers.is_empty());
    assert_eq!(saved.question.decision, None);
    assert_eq!(saved.question.authorization, None);
    assert!(!saved.question.is_fully_answered());
    assert_no_inputs(&fixture);
    let receipt = tool
        .invoke(json!({}), context("call_plan_draft"))
        .await
        .expect("deferred Plan receipt");
    assert_receipt(&receipt, &saved.question, QuestionResultStatus::Deferred);
    assert_eq!(receipt.continuation, ToolContinuation::Continue);
    assert_eq!(execution.get(SESSION_ID).expect("execution"), Some(before));
    assert_eq!(
        fixture
            .reopen()
            .get(SESSION_ID, &opened.id)
            .await
            .expect("saved Plan form"),
        saved.question
    );
}

#[tokio::test]
async fn explicit_plan_approval_remains_a_receipt_awaiting_the_host_handoff() {
    let fixture = QuestionFixture::new();
    let binding = plan_binding();
    fixture.port.bind_plan(binding.clone());
    let execution = SessionExecutionStore::new(fixture.pool());
    let before = execution
        .seed(
            SESSION_ID,
            CollaborationMode::Plan,
            Some(binding.work_identity),
            10,
        )
        .expect("Plan mode");
    let tool = erase(PlanExitTool::new(fixture.shared_port()));
    let published = tool
        .invoke(json!({}), context("call_approve_plan"))
        .await
        .expect("publish");
    let opened = fixture
        .port
        .get(SESSION_ID, request_id(&published))
        .await
        .expect("request");
    let approved = fixture
        .port
        .apply(
            SESSION_ID,
            &opened.id,
            command(
                "explicit_approval",
                &opened,
                QuestionAction::PlanDecision {
                    decision: PlanQuestionDecision::Approve,
                    risk_reason: None,
                },
            ),
        )
        .await
        .expect("record the explicit human decision");
    assert_eq!(
        approved.question.authorization,
        Some(PlanAuthorizationState::WaitingForHandoff)
    );
    assert_eq!(approved.input_id, None);
    let receipt = tool
        .invoke(json!({}), context("call_approve_plan"))
        .await
        .expect("idempotent receipt after approval");
    assert_receipt(&receipt, &approved.question, QuestionResultStatus::Answered);
    assert_eq!(receipt.continuation, ToolContinuation::Continue);
    assert_eq!(execution.get(SESSION_ID).expect("execution"), Some(before));
    assert_no_inputs(&fixture);
    assert_eq!(fixture.port.wait_count(), 0);
}

#[tokio::test]
async fn plan_exit_rejects_forged_authority_before_calling_the_port() {
    let fixture = QuestionFixture::new();
    let tool = erase(PlanExitTool::new(fixture.shared_port()));
    for args in [
        json!({"approved":true}),
        json!({"planId":"forged","planRevision":99}),
        json!({"answers":[["Approve"]]}),
        json!({"decision":"approve"}),
        json!({"agent":"build","model":"forged-model"}),
    ] {
        assert!(tool.invoke(args, context("call_forged")).await.is_err());
    }
    assert!(fixture.port.opened().is_empty());
    assert_no_questions(&fixture).await;
}

#[tokio::test]
async fn a_host_rejection_cannot_be_rendered_as_a_published_or_approved_plan() {
    let fixture = QuestionFixture::new();
    let error = erase(PlanExitTool::new(fixture.shared_port()))
        .invoke(json!({}), context("call_missing_plan"))
        .await
        .expect_err("the host cannot bind a missing Plan");
    let ToolError::Failed { source, .. } = error else {
        panic!("preserve the typed question-port failure");
    };
    assert!(matches!(
        source.downcast_ref::<QuestionError>(),
        Some(QuestionError::Rejected {
            code: "missing_plan",
            ..
        })
    ));
    assert_no_questions(&fixture).await;
    assert_no_inputs(&fixture);
}

#[test]
fn every_question_status_including_pending_and_deferred_round_trips_as_a_receipt() {
    for (status, wire, label) in [
        (QuestionResultStatus::Pending, "pending", "Awaiting answer"),
        (QuestionResultStatus::Deferred, "deferred", "Deferred"),
        (QuestionResultStatus::Answered, "answered", "Answered"),
        (QuestionResultStatus::Cancelled, "cancelled", "Cancelled"),
        (QuestionResultStatus::Expired, "expired", "Expired"),
        (QuestionResultStatus::Failed, "failed", "Failed"),
    ] {
        assert_eq!(status.as_str(), wire);
        assert_eq!(status.label(), label);
        assert_eq!(
            serde_json::from_value::<QuestionResultStatus>(json!(wire)).expect("wire status"),
            status
        );
        assert_eq!(
            QuestionTool::title(status, 2, Duration::from_secs(62)),
            format!("{label} · 2 questions · 1m 02s")
        );
        let presentation = QuestionResultPresentation::new(status, None, 2, 62_000);
        let encoded = serde_json::to_value(&presentation).expect("presentation");
        assert_eq!(encoded["status"], wire);
        assert!(encoded.get("answers").is_none());
        assert_eq!(
            serde_json::from_value::<QuestionResultPresentation>(encoded).expect("restore receipt"),
            presentation
        );
    }
}
