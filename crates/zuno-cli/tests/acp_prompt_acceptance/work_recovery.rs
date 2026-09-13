//! Ordinary Work recovery through the ACP process, durable receipts and provider wire.

use super::*;
use zuno_db::inbox::SubmissionState;
use zuno_db::input_receipt::InputReceiptStore;
use zuno_goal::{GoalStatus, GoalStore};
use zuno_session_control::QuestionService;
use zuno_tool::question::QuestionPort;
use zuno_types::admission::{InputAdmissionReceipt, InputReceiptState};
use zuno_types::execution::{
    CollaborationMode, SessionPauseReason, SessionReadiness, SessionScheduling,
};
use zuno_types::question::{
    QuestionAction, QuestionCommand, QuestionMode, QuestionPurpose, QuestionState, QuestionView,
};

const RETRY_ATTEMPTS: usize = 2;
const FAILED_PROMPT: &str = "Inspect the unavailable ordinary Work fixture.";
const NEXT_PROMPT: &str = "Handle this new independent Work request without a resume command.";
const LATE_ANSWER: &str = "late-work-choice-7e21";
const QUEUED_ANSWER: &str = "queued-before-resume-choice-b91d";

fn has_tools(body: &Value) -> bool {
    body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
}

async fn provider_requests(provider: &MockServer) -> Vec<Value> {
    provider
        .received_requests()
        .await
        .expect("loopback provider requests")
        .into_iter()
        .map(|request| serde_json::from_slice(&request.body).expect("provider JSON"))
        .filter(has_tools)
        .collect()
}

struct FailedWorkThenReply {
    attempts: Arc<AtomicUsize>,
}

impl Respond for FailedWorkThenReply {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider JSON");
        if !has_tools(&body) {
            return compatible_text_response("Ordinary Work recovery");
        }
        if self.attempts.fetch_add(1, Ordering::SeqCst) < RETRY_ATTEMPTS {
            ResponseTemplate::new(503)
                .insert_header("x-request-id", "ordinary-work-retry")
                .set_body_json(json!({"error": {
                    "message": "Synthetic provider overload",
                    "code": "provider_overloaded",
                    "request_id": "ordinary-work-retry"
                }}))
        } else {
            compatible_text_response("The new independent request completed.")
        }
    }
}

#[derive(Clone, Copy)]
enum PriorGoal {
    Absent,
    Completed,
}

async fn failed_work_then_new_prompt(prior_goal: PriorGoal) {
    let provider = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(FailedWorkThenReply {
            attempts: Arc::clone(&attempts),
        })
        .mount(&provider)
        .await;
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).expect("fixture config");
    config["provider"]["test"]["retry"] = json!({
        "max_attempts": RETRY_ATTEMPTS,
        "recovery_window_ms": 2000,
        "initial_delay_ms": 1,
        "max_delay_ms": 1,
        "jitter_percent": 0
    });
    let mut client = PromptClient::start(&config.to_string());
    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(client.root.path())).expect("isolated Work database"),
    );
    let goals = GoalStore::from_pool(
        Arc::clone(&pool),
        client.root.path().join("work-recovery-goal-spill"),
    )
    .expect("fixture Goal store");
    if matches!(prior_goal, PriorGoal::Completed) {
        // Seed while the session is closed so no live driver can observe the
        // temporary active Goal used to construct this completed fixture.
        materialize_acp_fixture_session(client.root.path(), &client.session_id, "test-model", None);
        client.request(3, "session/close", json!({"sessionId": client.session_id}));
        assert!(client.responses(&[3])[0].get("error").is_none());
        let goal = goals
            .create_goal(
                &client.session_id,
                "An already delivered unrelated Goal.",
                None,
            )
            .expect("create fixture Goal");
        let completed = goals
            .complete_checked(&client.session_id, goal.revision)
            .expect("complete fixture Goal")
            .expect("completed Goal remains inspectable");
        assert_eq!(completed.status, GoalStatus::Complete);
        client.request(
            4,
            "session/load",
            json!({
                "sessionId": client.session_id,
                "cwd": client.root.path(),
                "mcpServers": []
            }),
        );
        assert!(client.responses(&[4])[0].get("error").is_none());
    }
    let original_goal = goals.goal(&client.session_id).expect("original Goal");
    assert_eq!(
        original_goal.is_some(),
        matches!(prior_goal, PriorGoal::Completed)
    );
    let original_pause = goals
        .pause_state(&client.session_id)
        .expect("original pause");
    let original_retry = goals
        .retry_state(&client.session_id)
        .expect("original retry");
    assert_eq!(attempts.load(Ordering::SeqCst), 0);

    client.prompt(10, FAILED_PROMPT);
    let failed = client.responses(&[10]).remove(0);
    let failure = &failed["error"]["data"];
    assert_eq!(failure["admission"], "accepted", "{failed}");
    assert_eq!(failure["receipt"]["state"], "failed", "{failed}");
    assert!(failure["receipt"]["appliedAt"].is_number(), "{failed}");
    assert!(failure["receipt"]["completedAt"].is_number(), "{failed}");
    assert_eq!(attempts.load(Ordering::SeqCst), RETRY_ATTEMPTS);
    let failed_id = failure["receipt"]["inputId"]
        .as_str()
        .expect("failed input ID");
    let receipts = InputReceiptStore::new(Arc::clone(&pool));
    let failed_receipt = receipts
        .get(&client.session_id, failed_id)
        .expect("failed durable receipt")
        .expect("failed receipt exists");
    assert_eq!(failed_receipt.state, InputReceiptState::Failed);
    assert_eq!(
        durable_input(client.root.path(), &client.session_id, failed_id).state,
        SubmissionState::Consumed
    );
    assert_eq!(goals.goal(&client.session_id).unwrap(), original_goal);
    assert_eq!(
        goals.pause_state(&client.session_id).unwrap(),
        original_pause
    );
    assert_eq!(
        goals.retry_state(&client.session_id).unwrap(),
        original_retry
    );

    // This is a new real prompt, not /resume and not a retry of the failed input.
    client.prompt(11, NEXT_PROMPT);
    let next = client.responses(&[11]).remove(0);
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    let next_receipt = &next["result"]["_meta"]["zuno"]["receipt"];
    assert_eq!(next_receipt["state"], "completed", "{next}");
    assert_ne!(next_receipt["inputId"], failed_id);
    assert_ne!(next_receipt["turnId"], failure["receipt"]["turnId"]);
    assert_eq!(client.admitted_count(FAILED_PROMPT), 1);
    assert_eq!(client.admitted_count(NEXT_PROMPT), 1);
    client.disconnect().await;

    let requests = provider_requests(&provider).await;
    assert_eq!(requests.len(), RETRY_ATTEMPTS + 1);
    assert!(
        requests[RETRY_ATTEMPTS]["messages"]
            .to_string()
            .contains(NEXT_PROMPT)
    );
    assert_eq!(
        receipts.get(&client.session_id, failed_id).unwrap(),
        Some(failed_receipt),
        "later work must not rewrite the failed receipt"
    );
    assert_eq!(goals.goal(&client.session_id).unwrap(), original_goal);
    assert_eq!(
        goals.pause_state(&client.session_id).unwrap(),
        original_pause
    );
    assert_eq!(
        goals.retry_state(&client.session_id).unwrap(),
        original_retry
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_503_retry_failure_allows_the_next_real_prompt_without_resume() {
    failed_work_then_new_prompt(PriorGoal::Absent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_503_retry_failure_preserves_an_unrelated_completed_goal() {
    failed_work_then_new_prompt(PriorGoal::Completed).await;
}

fn listed_question(client: &mut PromptClient, id: u64) -> QuestionView {
    client.request(
        id,
        "questions/list",
        json!({"sessionId": client.session_id}),
    );
    let response = client.responses(&[id]).remove(0);
    let questions = response["result"]["questions"]
        .as_array()
        .unwrap_or_else(|| panic!("native question listing failed: {response}"));
    assert_eq!(questions.len(), 1, "{response}");
    serde_json::from_value(questions[0].clone()).expect("native question view")
}

fn answer_command(question: &QuestionView, command_id: &str, answer: &str) -> QuestionCommand {
    QuestionCommand {
        command_id: command_id.to_owned(),
        expected_revision: question.revision,
        action: QuestionAction::Answer {
            answers: [(question.questions[0].id.clone(), vec![answer.to_owned()])].into(),
        },
    }
}

async fn completed_unanswered_work(provider: &MockServer) -> (PromptClient, String, QuestionView) {
    Mock::given(method("POST"))
        .respond_with(QuestionTurnResponder)
        .mount(provider)
        .await;
    let config = config_with_second_model(&provider.uri());
    let mut client = PromptClient::start(&config);
    client.prompt(
        3,
        "Ask an optional database question and finish independent Work.",
    );
    // No elicitation capability or human response is supplied. The actual
    // `question` tool must still let this process finish the prompt.
    let completed = client.responses(&[3]).remove(0);
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    assert_eq!(
        completed["result"]["_meta"]["zuno"]["receipt"]["state"], "completed",
        "{completed}"
    );
    let question = listed_question(&mut client, 4);
    assert_eq!(question.purpose, QuestionPurpose::Clarification);
    assert_eq!(question.mode, QuestionMode::Deferred);
    assert_eq!(question.state, QuestionState::Pending);
    assert!(question.answers.is_empty());
    assert_eq!(question.authorization, None);
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(client.root.path())).unwrap());
    let state = zuno_db::session_execution::SessionExecutionStore::new(pool)
        .get(&client.session_id)
        .unwrap()
        .expect("native Work execution state");
    assert_eq!(state.mode, CollaborationMode::Work);
    assert!(!matches!(
        state.scheduling.unwrap().readiness,
        SessionReadiness::WaitingHuman { .. } | SessionReadiness::Paused { .. }
    ));
    assert_eq!(provider_requests(provider).await.len(), 2);
    (client, config, question)
}

async fn completed_answer(client: &PromptClient, input_id: &str) -> InputAdmissionReceipt {
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(client.root.path())).unwrap());
    let receipts = InputReceiptStore::new(pool);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let receipt = receipts.get(&client.session_id, input_id).unwrap();
        if let Some(receipt) = receipt
            .as_ref()
            .filter(|receipt| receipt.state.is_terminal())
        {
            assert_eq!(receipt.state, InputReceiptState::Completed, "{receipt:?}");
            assert!(receipt.applied_at.is_some(), "{receipt:?}");
            assert!(receipt.completed_at.is_some(), "{receipt:?}");
            return receipt.clone();
        }
        assert!(
            Instant::now() < deadline,
            "answer never reached the provider and completed: {receipt:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn assert_answer_consumed_once(client: &PromptClient, question_id: &str, input_id: &str) {
    assert_eq!(
        durable_input(client.root.path(), &client.session_id, input_id).state,
        SubmissionState::Consumed
    );
    let connection = zuno_db::open::open(&acp_database(client.root.path())).unwrap();
    let answers: i64 = connection
        .query_row(
            "SELECT count(*) FROM session_input WHERE session_id=?1 \
             AND json_extract(prompt,'$.kind')='humanRequestAnswer' \
             AND json_extract(prompt,'$.requestID')=?2",
            [&client.session_id, question_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        answers, 1,
        "duplicate replies must reuse the original input ID"
    );
    let messages: i64 = connection
        .query_row(
            "SELECT count(*) FROM message WHERE session_id=?1 AND id=?2",
            [&client.session_id, input_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        messages, 1,
        "one answer must have one durable model message"
    );
}

fn assert_answer_in_request(request: &Value, question_id: &str, answer: &str) {
    let messages = request["messages"].as_array().expect("provider messages");
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                message["role"] == "user"
                    && message["content"].to_string().contains(question_id)
                    && message["content"].to_string().contains(answer)
            })
            .count(),
        1,
        "the provider must receive exactly one copy of the accepted answer: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .all(|message| !message["content"].to_string().contains(answer))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unanswered_work_question_finishes_and_native_reply_after_reconnect_is_consumed_once() {
    let provider = MockServer::start().await;
    let (mut client, config, original) = completed_unanswered_work(&provider).await;
    let root = Arc::clone(&client.root);
    let session_id = client.session_id.clone();
    client.disconnect().await;
    drop(client);

    let mut client = PromptClient::connect(&config, root, Some(&session_id));
    let question = listed_question(&mut client, 3);
    assert_eq!(question.id, original.id);
    assert_eq!(question.revision, original.revision);
    assert_eq!(question.state, QuestionState::Pending);
    assert!(question.answers.is_empty());
    assert_eq!(provider_requests(&provider).await.len(), 2);
    let params = json!({
        "sessionId": client.session_id,
        "requestId": question.id,
        "command": answer_command(&question, "late-answer-after-reconnect", LATE_ANSWER)
    });
    client.request(4, "questions/respond", params.clone());
    let answered = client.responses(&[4]).remove(0);
    assert!(answered.get("error").is_none(), "{answered}");
    assert_eq!(answered["result"]["duplicate"], false, "{answered}");
    let input_id = answered["result"]["inputId"]
        .as_str()
        .expect("native reply admits an input")
        .to_owned();
    completed_answer(&client, &input_id).await;

    client.request(5, "questions/respond", params);
    let repeated = client.responses(&[5]).remove(0);
    assert_eq!(repeated["result"]["duplicate"], true, "{repeated}");
    assert_eq!(repeated["result"]["inputId"], input_id);
    client.disconnect().await;
    assert_answer_consumed_once(&client, &question.id, &input_id);
    let requests = provider_requests(&provider).await;
    assert_eq!(
        requests.len(),
        3,
        "duplicate reply started another provider turn"
    );
    assert_answer_in_request(&requests[2], &question.id, LATE_ANSWER);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answer_queued_by_a_separate_writer_is_in_the_first_resumed_provider_request() {
    let provider = MockServer::start().await;
    let (mut client, config, question) = completed_unanswered_work(&provider).await;
    let root = Arc::clone(&client.root);
    let session_id = client.session_id.clone();
    client.disconnect().await;
    drop(client);

    let pool = Arc::new(zuno_db::Pool::open(&acp_database(root.path())).unwrap());
    let execution = zuno_db::session_execution::SessionExecutionStore::new(Arc::clone(&pool));
    let state = execution.get(&session_id).unwrap().unwrap();
    // Hold the existing authorized Work while the separate writer commits.
    // Reconnection must not consume the answer before explicit /resume.
    execution
        .set_scheduling(
            &session_id,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused {
                    reason: SessionPauseReason::User,
                },
                ..Default::default()
            },
            zuno_db::message::now_millis(),
        )
        .unwrap();
    let questions = QuestionService::new(Arc::clone(&pool));
    let command = answer_command(&question, "separate-writer-answer", QUEUED_ANSWER);
    let answered = questions
        .apply(&session_id, &question.id, command.clone())
        .await
        .expect("native answer with no live run registry");
    let input_id = answered.input_id.expect("queued answer input ID");
    assert_eq!(
        durable_input(root.path(), &session_id, &input_id).state,
        SubmissionState::Queued
    );
    assert!(
        InputReceiptStore::new(Arc::clone(&pool))
            .get(&session_id, &input_id)
            .unwrap()
            .unwrap()
            .applied_at
            .is_none()
    );
    drop(questions);
    drop(execution);
    drop(pool);

    let mut client = PromptClient::connect(&config, root, Some(&session_id));
    client.request(3, "session/list", json!({}));
    assert!(client.responses(&[3])[0].get("error").is_none());
    assert_eq!(provider_requests(&provider).await.len(), 2);
    assert_eq!(
        durable_input(client.root.path(), &session_id, &input_id).state,
        SubmissionState::Queued
    );
    client.prompt(4, "/resume");
    let resumed = client.responses(&[4]).remove(0);
    assert_eq!(resumed["result"]["stopReason"], "end_turn", "{resumed}");
    let applied = completed_answer(&client, &input_id).await;
    assert_eq!(
        applied.turn_id.as_deref(),
        resumed["result"]["_meta"]["zuno"]["receipt"]["turnId"].as_str()
    );
    let requests = provider_requests(&provider).await;
    assert_eq!(
        requests.len(),
        3,
        "resume made an extra request before applying the answer"
    );
    assert_answer_in_request(&requests[2], &question.id, QUEUED_ANSWER);

    client.request(
        5,
        "questions/respond",
        json!({"sessionId": session_id, "requestId": question.id, "command": command}),
    );
    let repeated = client.responses(&[5]).remove(0);
    assert_eq!(repeated["result"]["duplicate"], true, "{repeated}");
    assert_eq!(repeated["result"]["inputId"], input_id);
    client.disconnect().await;
    assert_answer_consumed_once(&client, &question.id, &input_id);
    assert_eq!(provider_requests(&provider).await.len(), 3);
}
