//! Runs as a transport child module so lifecycle tests can exercise the real
//! request cleanup without exposing transport internals to production consumers.

use std::collections::{BTreeMap, VecDeque};

use tokio::io::DuplexStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use zuno_tool::InterruptHandle;
use zuno_tool::question::{QuestionError, QuestionPort, QuestionResult};
use zuno_types::execution::TurnExecutionIdentity;
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionBinding, PlanQuestionDecision, QuestionAction,
    QuestionCommand, QuestionItem, QuestionMode, QuestionOption, QuestionOrigin, QuestionPurpose,
    QuestionReceipt, QuestionRequest, QuestionSpec, QuestionState, QuestionView,
};

use super::test_client::ScriptedClient;
use super::*;
use crate::{AcpQuestionPresenter, AcpSessionRoute};

type Applied = (String, String, QuestionCommand);
type Presented = QuestionResult<Option<QuestionReceipt>>;

struct ScriptedPort {
    stored: QuestionView,
    commands: Mutex<Vec<Applied>>,
    replies: Mutex<VecDeque<QuestionResult<QuestionReceipt>>>,
}

impl ScriptedPort {
    fn new(stored: QuestionView, replies: Vec<QuestionResult<QuestionReceipt>>) -> Self {
        Self {
            stored,
            commands: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
        }
    }

    fn commands(&self) -> Vec<Applied> {
        lock(&self.commands).clone()
    }
}

#[async_trait]
impl QuestionPort for ScriptedPort {
    async fn open(&self, _spec: QuestionSpec) -> QuestionResult<QuestionReceipt> {
        panic!("a presenter must not publish a second question")
    }

    async fn apply(
        &self,
        session_id: &str,
        request_id: &str,
        command: QuestionCommand,
    ) -> QuestionResult<QuestionReceipt> {
        command.validate()?;
        lock(&self.commands).push((session_id.to_owned(), request_id.to_owned(), command));
        lock(&self.replies)
            .pop_front()
            .expect("one scripted reply per apply")
    }

    async fn get(&self, _session_id: &str, _request_id: &str) -> QuestionResult<QuestionView> {
        panic!("the presenter already owns the displayed snapshot")
    }

    async fn pending(&self, _session_id: &str) -> QuestionResult<Vec<QuestionView>> {
        panic!("the session supervisor owns pending-question discovery")
    }

    async fn wait_for_change(
        &self,
        _session_id: &str,
        _request_id: &str,
        _after_revision: i64,
        _interrupt: Arc<dyn InterruptHandle>,
    ) -> QuestionResult<QuestionView> {
        panic!("the session supervisor owns durable change notifications")
    }
}

fn view() -> QuestionView {
    QuestionView {
        id: "que-durable".to_owned(),
        origin: QuestionOrigin {
            session_id: "ses-durable".to_owned(),
            message_id: Some("message-finished".to_owned()),
            call_id: Some("call-finished".to_owned()),
            turn_id: Some("turn-finished".to_owned()),
            goal_id: None,
        },
        revision: 7,
        mode: QuestionMode::Deferred,
        purpose: QuestionPurpose::Clarification,
        state: QuestionState::Pending,
        questions: vec![
            QuestionItem {
                id: "item-approach".to_owned(),
                question: QuestionRequest::closed(
                    "Which approach?",
                    "Approach",
                    vec![
                        QuestionOption::new("Keep", "Keep the current approach"),
                        QuestionOption::new("Replace", "Use the new approach"),
                    ],
                ),
            },
            QuestionItem {
                id: "item-notes".to_owned(),
                question: QuestionRequest {
                    question: "Any additional context?".to_owned(),
                    header: "Notes".to_owned(),
                    options: Vec::new(),
                    multiple: None,
                    custom: None,
                },
            },
        ],
        answers: BTreeMap::new(),
        draft_answers: BTreeMap::new(),
        plan: None,
        decision: None,
        authorization: None,
        time_created: 100,
        time_updated: 200,
    }
}

fn plan_view() -> QuestionView {
    let mut view = view();
    view.purpose = QuestionPurpose::PlanAuthorization;
    view.questions = vec![QuestionItem {
        id: "item-plan".to_owned(),
        question: QuestionRequest::closed(
            "Start working on the reviewed plan?",
            "Plan",
            vec![
                QuestionOption::new("approve", "Approve implementation"),
                QuestionOption::new("decline", "Keep planning"),
            ],
        ),
    }];
    view.plan = Some(PlanQuestionBinding {
        source_cycle_id: None,
        plan_id: "plan-1".to_owned(),
        plan_revision: 3,
        title: "Reviewed delivery plan".to_owned(),
        completed_steps: 0,
        total_steps: 2,
        work_identity: TurnExecutionIdentity::new("build", "provider", "model"),
        review_gate: Value::Null,
    });
    view
}

fn goal_resume_view() -> QuestionView {
    let mut view = view();
    view.purpose = QuestionPurpose::GoalResume;
    view.origin.goal_id = Some("goal-paused".to_owned());
    view.origin.message_id = Some("input-already-accepted".to_owned());
    view.origin.call_id = None;
    view.questions = vec![QuestionItem {
        id: "resume-goal".to_owned(),
        question: QuestionRequest::closed(
            "Resume the paused Goal?",
            "Goal",
            vec![
                QuestionOption::new("Resume goal", "Resume this Goal"),
                QuestionOption::new("Keep paused", "Keep the Goal paused"),
            ],
        ),
    }];
    view
}

#[tokio::test]
async fn goal_resume_native_form_keeps_closed_choices_optional_without_a_default() {
    let view = goal_resume_view();
    let port = port_for(&view);
    let client = ScriptedClient::new(|method, params| {
        assert_eq!(method, "elicitation/create");
        assert_eq!(params["_meta"]["zuno"]["questionPurpose"], "goal_resume");
        let schema = &params["requestedSchema"];
        let field = &schema["properties"]["answer:resume-goal"];
        assert_eq!(
            field["oneOf"]
                .as_array()
                .expect("native closed choices")
                .iter()
                .map(|choice| choice["const"].clone())
                .collect::<Vec<_>>(),
            vec![json!("Resume goal"), json!("Keep paused")]
        );
        assert!(field.get("default").is_none());
        assert!(schema.get("required").is_none());
        Ok(json!({"action":"accept","content":{}}))
    });
    AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view.clone())
        .await
        .expect("present Goal resume")
        .expect("fixture callback submits the optional form");
    assert!(matches!(
        port.commands()[0].2.action,
        QuestionAction::Defer { .. }
    ));
    assert_eq!(port.stored.origin, view.origin);
}

#[tokio::test]
async fn goal_resume_native_answers_preserve_the_explicit_label_and_revision() {
    let view = goal_resume_view();
    for choice in ["Resume goal", "Keep paused"] {
        let (_, command) = apply_response(
            &view,
            json!({"action":"accept","content":{"answer:resume-goal":choice}}),
        )
        .await;
        assert_eq!(command.expected_revision, view.revision);
        assert_eq!(
            command.action,
            QuestionAction::Answer {
                answers: BTreeMap::from([("resume-goal".to_owned(), vec![choice.to_owned()])]),
            }
        );
    }
}

#[tokio::test]
async fn goal_resume_native_cancel_and_decline_never_become_resume_answers() {
    let view = goal_resume_view();
    let (_, cancelled) = apply_response(
        &view,
        json!({"action":"cancel","content":{"answer:resume-goal":"Resume goal"}}),
    )
    .await;
    assert!(matches!(cancelled.action, QuestionAction::Defer { .. }));
    let (_, declined) = apply_response(&view, json!({"action":"decline"})).await;
    assert_eq!(declined.action, QuestionAction::Cancel);
}

fn receipt(view: &QuestionView) -> QuestionReceipt {
    let mut question = view.clone();
    question.revision += 1;
    question.mode = QuestionMode::Deferred;
    QuestionReceipt {
        question,
        input_id: None,
        duplicate: false,
    }
}

fn port_for(view: &QuestionView) -> Arc<ScriptedPort> {
    Arc::new(ScriptedPort::new(view.clone(), vec![Ok(receipt(view))]))
}

async fn apply_response(
    view: &QuestionView,
    response: Value,
) -> (QuestionReceipt, QuestionCommand) {
    let port = port_for(view);
    let client = ScriptedClient::new(move |_, _| Ok(response.clone()));
    let result = AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view.clone())
        .await
        .expect("valid response")
        .expect("committed receipt");
    let commands = port.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, view.origin.session_id);
    assert_eq!(commands[0].1, view.id);
    assert_eq!(commands[0].2.expected_revision, view.revision);
    assert_eq!(
        port.stored, *view,
        "presentation mutated its input snapshot"
    );
    (result, commands[0].2.clone())
}

#[tokio::test]
async fn partial_answers_are_forwarded_by_stable_item_id() {
    let view = view();
    let (receipt, command) = apply_response(
        &view,
        json!({
            "action": "accept",
            "content": {"answer:item-notes": "first\nsecond"},
        }),
    )
    .await;
    assert_eq!(
        command.action,
        QuestionAction::Answer {
            answers: BTreeMap::from([("item-notes".to_owned(), vec!["first\nsecond".to_owned()])]),
        }
    );
    assert_eq!(receipt.question.state, QuestionState::Pending);
    assert!(receipt.input_id.is_none());
}

#[tokio::test]
async fn native_cancel_saves_drafts_and_prefills_override_confirmed_values() {
    let mut view = view();
    view.answers
        .insert("item-approach".to_owned(), vec!["Keep".to_owned()]);
    view.draft_answers
        .insert("item-approach".to_owned(), Vec::new());
    view.draft_answers
        .insert("item-notes".to_owned(), vec!["first\nsecond".to_owned()]);
    let client = ScriptedClient::new(|_, params| {
        let fields = &params["requestedSchema"]["properties"];
        assert!(fields["answer:item-approach"].get("default").is_none());
        assert_eq!(fields["answer:item-notes"]["default"], "first\nsecond");
        Ok(json!({"action": "cancel", "content": {
            "answer:item-approach": "Replace",
            "answer:item-notes": "edited\nlater",
        }}))
    });
    let port = port_for(&view);
    AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view.clone())
        .await
        .expect("draft deferral");
    assert_eq!(
        port.commands()[0].2.action,
        QuestionAction::Defer {
            draft_answers: BTreeMap::from([
                ("item-approach".to_owned(), vec!["Replace".to_owned()]),
                ("item-notes".to_owned(), vec!["edited\nlater".to_owned()]),
            ]),
        }
    );
    assert_eq!(port.stored.answers, view.answers);
}

#[tokio::test]
async fn empty_acceptance_never_confirms_complete_drafts_and_partial_accept_omits_other_drafts() {
    let mut view = view();
    view.draft_answers = BTreeMap::from([
        ("item-approach".to_owned(), vec!["Keep".to_owned()]),
        (
            "item-notes".to_owned(),
            vec!["unsubmitted notes".to_owned()],
        ),
    ]);
    let (_, empty) = apply_response(&view, json!({"action":"accept","content":{}})).await;
    assert_eq!(
        empty.action,
        QuestionAction::Defer {
            draft_answers: BTreeMap::new()
        }
    );
    let (_, partial) = apply_response(
        &view,
        json!({
            "action":"accept","content":{"answer:item-approach":"Replace"},
        }),
    )
    .await;
    assert_eq!(
        partial.action,
        QuestionAction::Answer {
            answers: BTreeMap::from([("item-approach".to_owned(), vec!["Replace".to_owned()])]),
        }
    );
}

#[tokio::test]
async fn plan_drafts_never_prefill_or_grant_the_explicit_decision() {
    let mut view = plan_view();
    view.draft_answers
        .insert("item-plan".to_owned(), vec!["approve".to_owned()]);
    let client = ScriptedClient::new(|_, params| {
        assert!(
            params["requestedSchema"]["properties"]["planDecision"]
                .get("default")
                .is_none()
        );
        Ok(json!({"action":"cancel","content":{"planDecision":"decline"}}))
    });
    let port = port_for(&view);
    AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view)
        .await
        .expect("Plan draft");
    assert_eq!(
        port.commands()[0].2.action,
        QuestionAction::Defer {
            draft_answers: BTreeMap::from([("item-plan".to_owned(), vec!["decline".to_owned()])]),
        }
    );
}

#[tokio::test]
async fn stable_item_fields_and_prefills_survive_question_reordering() {
    let mut view = view();
    view.answers
        .insert("item-approach".to_owned(), vec!["Replace".to_owned()]);
    view.answers
        .insert("item-notes".to_owned(), vec!["first\nsecond".to_owned()]);
    let forms = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&forms);
    let client = ScriptedClient::new(move |method, params| {
        assert_eq!(method, "elicitation/create");
        lock(&recorded).push(params.clone());
        Ok(json!({"action": "cancel"}))
    });
    let port = Arc::new(ScriptedPort::new(
        view.clone(),
        vec![Ok(receipt(&view)), Ok(receipt(&view))],
    ));
    let presenter = AcpQuestionPresenter::new(port, client.connection());
    presenter.present(view.clone()).await.expect("first form");
    view.questions.reverse();
    presenter
        .present(view.clone())
        .await
        .expect("reordered form");

    let forms = lock(&forms);
    assert_eq!(forms.len(), 2);
    assert_eq!(
        forms[0]["requestedSchema"]["properties"],
        forms[1]["requestedSchema"]["properties"]
    );
    let schema = &forms[0]["requestedSchema"];
    assert!(
        schema.get("required").is_none(),
        "partial answers must be accepted"
    );
    assert_eq!(
        schema["properties"]["answer:item-approach"]["default"],
        "Replace"
    );
    assert_eq!(
        schema["properties"]["answer:item-notes"]["default"],
        "first\nsecond"
    );
    assert!(
        schema["properties"]["answer:item-notes"]
            .get("minLength")
            .is_none()
    );
    assert!(schema["properties"].get("q0").is_none());
    assert!(
        forms[0].get("toolCallId").is_none(),
        "a completed tool must not own this form"
    );
    assert_eq!(forms[0]["_meta"]["zuno"]["questionId"], view.id);
    assert_eq!(forms[0]["_meta"]["zuno"]["questionRevision"], view.revision);
}

#[tokio::test]
async fn empty_submit_and_native_cancel_defer_even_with_complete_stored_answers() {
    let mut view = view();
    view.answers
        .insert("item-approach".to_owned(), vec!["Keep".to_owned()]);
    view.answers
        .insert("item-notes".to_owned(), vec!["stored".to_owned()]);
    for response in [
        Value::Null,
        json!({"action": "accept"}),
        json!({"action": "accept", "content": null}),
        json!({"action": "accept", "content": {}}),
        json!({"action": "accept", "content": {
            "answer:item-approach": null, "answer:item-notes": "",
        }}),
        json!({"action": "accept", "content": {"answer:item-notes": " \n "}}),
        json!({"action": "cancel", "content": {"answer:item-approach": "Replace"}}),
    ] {
        let (receipt, command) = apply_response(&view, response).await;
        assert!(matches!(command.action, QuestionAction::Defer { .. }));
        assert_eq!(receipt.question.state, QuestionState::Pending);
        assert_eq!(receipt.question.answers, view.answers);
    }
}

#[tokio::test]
async fn native_decline_is_an_explicit_cancellation_for_both_purposes() {
    for view in [view(), plan_view()] {
        let (_, command) = apply_response(
            &view,
            json!({
                "action": "decline", "content": {"planDecision": "approve"},
            }),
        )
        .await;
        assert_eq!(command.action, QuestionAction::Cancel);
    }
}

#[tokio::test]
async fn plan_authorization_requires_the_explicit_decision_field() {
    for (label, decision) in [
        ("approve", PlanQuestionDecision::Approve),
        ("decline", PlanQuestionDecision::Decline),
    ] {
        let view = plan_view();
        let (_, command) = apply_response(
            &view,
            json!({
                "action": "accept",
                "content": {"planDecision": label, "riskReason": "Reviewed the risk"},
            }),
        )
        .await;
        assert_eq!(
            command.action,
            QuestionAction::PlanDecision {
                decision,
                risk_reason: Some("Reviewed the risk".to_owned()),
            }
        );
    }
}

#[tokio::test]
async fn plan_empty_values_and_native_cancel_never_infer_approval() {
    let mut view = plan_view();
    view.answers
        .insert("item-plan".to_owned(), vec!["Yes".to_owned()]);
    view.decision = Some(PlanQuestionDecision::Approve);
    for response in [
        json!({"action": "accept"}),
        json!({"action": "accept", "content": {}}),
        json!({"action": "accept", "content": {"planDecision": null}}),
        json!({"action": "accept", "content": {"planDecision": " \n "}}),
        json!({"action": "accept", "content": {"riskReason": "not a decision"}}),
        json!({"action": "cancel", "content": {"planDecision": "approve"}}),
    ] {
        let (receipt, command) = apply_response(&view, response).await;
        assert!(matches!(command.action, QuestionAction::Defer { .. }));
        assert_eq!(receipt.question.state, QuestionState::Pending);
    }
}

#[tokio::test]
async fn plan_form_has_explicit_choices_without_defaults_or_generic_answers() {
    let mut view = plan_view();
    view.answers
        .insert("item-plan".to_owned(), vec!["Yes".to_owned()]);
    view.decision = Some(PlanQuestionDecision::Approve);
    let client = ScriptedClient::new(|_, params| {
        let properties = &params["requestedSchema"]["properties"];
        assert!(properties.get("answer:item-plan").is_none());
        assert!(properties["planDecision"].get("default").is_none());
        assert_eq!(properties["planDecision"]["oneOf"][0]["const"], "approve");
        assert_eq!(properties["planDecision"]["oneOf"][1]["const"], "decline");
        assert!(
            params["message"]
                .as_str()
                .expect("message")
                .contains("Reviewed delivery plan")
        );
        assert!(params["requestedSchema"].get("required").is_none());
        Ok(json!({"action": "cancel"}))
    });
    AcpQuestionPresenter::new(port_for(&view), client.connection())
        .present(view)
        .await
        .expect("plan form");
}

#[tokio::test]
async fn malformed_plan_decisions_leave_the_durable_row_untouched() {
    for response in [
        json!({"action": "accept", "content": {"planDecision": true}}),
        json!({"action": "accept", "content": {"planDecision": "Yes"}}),
        json!({"action": "accept", "content": {"planDecision": "Approve"}}),
        json!({"action": "accept", "content": {"q0": "Yes"}}),
        json!({"action": "accept", "content": []}),
        json!({}),
    ] {
        let view = plan_view();
        let port = port_for(&view);
        let client = ScriptedClient::new(move |_, _| Ok(response.clone()));
        let result = AcpQuestionPresenter::new(port.clone(), client.connection())
            .present(view.clone())
            .await;
        assert!(matches!(result, Err(QuestionError::Invalid(_))));
        assert!(port.commands().is_empty());
        assert_eq!(port.stored, view);
    }
}

#[tokio::test]
async fn malformed_answer_fields_are_not_forwarded_to_the_port() {
    for response in [
        json!({"action": "accept", "content": {"answer:unknown": "Keep"}}),
        json!({"action": "accept", "content": {"answer:item-approach": "Missing"}}),
        json!({"action": "accept", "content": {"answer:item-notes": 42}}),
        json!({"action": "accept", "content": {"planDecision": "approve"}}),
        json!({"action": "unknown"}),
    ] {
        let view = view();
        let port = port_for(&view);
        let client = ScriptedClient::new(move |_, _| Ok(response.clone()));
        let result = AcpQuestionPresenter::new(port.clone(), client.connection())
            .present(view.clone())
            .await;
        assert!(matches!(result, Err(QuestionError::Invalid(_))));
        assert!(port.commands().is_empty());
        assert_eq!(port.stored, view);
    }
}

#[tokio::test]
async fn native_multiple_choices_and_multiline_custom_prefills_remain_lossless() {
    let mut view = view();
    view.questions[0].question.multiple = Some(true);
    view.questions[0].question.custom = None;
    view.answers.insert(
        "item-approach".to_owned(),
        vec![
            "Replace".to_owned(),
            "custom\none".to_owned(),
            "custom two".to_owned(),
        ],
    );
    let client = ScriptedClient::new(|_, params| {
        let fields = &params["requestedSchema"]["properties"];
        assert!(fields["choice:item-approach"].get("minItems").is_none());
        assert_eq!(
            fields["choice:item-approach"]["default"],
            json!(["Replace"])
        );
        assert_eq!(
            fields["custom:item-approach"]["default"],
            "custom\none\ncustom two"
        );
        Ok(json!({
            "action": "accept",
            "content": {
                "choice:item-approach": ["Keep"],
                "custom:item-approach": "custom\none\ncustom two",
            },
        }))
    });
    let port = port_for(&view);
    AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view)
        .await
        .expect("multiple choices");
    assert_eq!(
        port.commands()[0].2.action,
        QuestionAction::Answer {
            answers: BTreeMap::from([(
                "item-approach".to_owned(),
                vec![
                    "Keep".to_owned(),
                    "custom\none".to_owned(),
                    "custom two".to_owned(),
                ]
            )]),
        }
    );
}

#[tokio::test]
async fn empty_multiselect_defers_and_duplicate_choices_are_rejected() {
    let mut view = view();
    view.questions[0].question.multiple = Some(true);
    let (_, command) = apply_response(
        &view,
        json!({
            "action": "accept", "content": {"answer:item-approach": []},
        }),
    )
    .await;
    assert!(matches!(command.action, QuestionAction::Defer { .. }));

    let port = port_for(&view);
    let client = ScriptedClient::new(|_, _| {
        Ok(json!({
            "action": "accept", "content": {"answer:item-approach": ["Keep", "Keep"]},
        }))
    });
    let result = AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view)
        .await;
    assert!(matches!(result, Err(QuestionError::Invalid(_))));
    assert!(port.commands().is_empty());
}

#[tokio::test]
async fn field_prefixes_do_not_collide_with_other_item_ids() {
    let mut view = view();
    view.questions[0].id = "x".to_owned();
    view.questions[0].question.custom = None;
    view.questions[1].id = "choice:x".to_owned();
    let (_, command) = apply_response(
        &view,
        json!({
            "action": "accept",
            "content": {"choice:x": "Keep", "answer:choice:x": "Notes"},
        }),
    )
    .await;
    assert_eq!(
        command.action,
        QuestionAction::Answer {
            answers: BTreeMap::from([
                ("x".to_owned(), vec!["Keep".to_owned()]),
                ("choice:x".to_owned(), vec!["Notes".to_owned()]),
            ]),
        }
    );
}

#[tokio::test]
async fn receipt_and_duplicate_replay_are_owned_by_the_port() {
    let view = plan_view();
    let mut expected = receipt(&view);
    expected.question.state = QuestionState::Answered;
    expected.question.decision = Some(PlanQuestionDecision::Approve);
    expected.question.authorization = Some(PlanAuthorizationState::WaitingForHandoff);
    expected.input_id = Some("control-from-port".to_owned());
    let mut duplicate = expected.clone();
    duplicate.duplicate = true;
    let port = Arc::new(ScriptedPort::new(
        view.clone(),
        vec![Ok(expected.clone()), Ok(duplicate.clone())],
    ));
    let client = ScriptedClient::new(|_, _| {
        Ok(json!({
            "action": "accept", "content": {"planDecision": "approve"},
        }))
    });
    let presenter = AcpQuestionPresenter::new(port.clone(), client.connection());
    assert_eq!(
        presenter
            .present(view.clone())
            .await
            .expect("first decision"),
        Some(expected)
    );
    assert_eq!(
        presenter.present(view).await.expect("duplicate decision"),
        Some(duplicate)
    );
    let commands = port.commands();
    assert_eq!(commands.len(), 2);
    assert_eq!(
        commands[0], commands[1],
        "replay changed the idempotency key"
    );
}

#[tokio::test]
async fn command_ids_include_revision_action_and_durable_session() {
    let view = view();
    let (_, first) = apply_response(&view, json!({"action": "cancel"})).await;
    let (_, replay) = apply_response(&view, json!({"action": "cancel"})).await;
    assert_eq!(first, replay);
    let (_, declined) = apply_response(&view, json!({"action": "decline"})).await;
    let mut updated = view.clone();
    updated.revision += 1;
    let (_, revised) = apply_response(&updated, json!({"action": "cancel"})).await;
    updated.origin.session_id = "another-session".to_owned();
    let (_, another) = apply_response(&updated, json!({"action": "cancel"})).await;
    let ids = [&first, &declined, &revised, &another]
        .map(|command| command.command_id.as_str())
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), 4);
}

#[tokio::test]
async fn stale_revision_errors_are_returned_without_reloading_or_replaying() {
    let view = view();
    let port = Arc::new(ScriptedPort::new(
        view.clone(),
        vec![Err(QuestionError::Conflict {
            request_id: view.id.clone(),
            expected: view.revision,
            actual: view.revision + 1,
        })],
    ));
    let client = ScriptedClient::new(|_, _| Ok(json!({"action": "cancel"})));
    let result = AcpQuestionPresenter::new(port.clone(), client.connection())
        .present(view.clone())
        .await;
    assert!(matches!(
        result,
        Err(QuestionError::Conflict {
            expected: 7,
            actual: 8,
            ..
        })
    ));
    assert_eq!(port.commands().len(), 1);
    assert_eq!(port.stored, view);
}

#[tokio::test]
async fn fallback_routing_keeps_the_durable_child_as_the_command_target() {
    let view = view();
    let route = Arc::new(AcpSessionRoute::new(false));
    route.bind_root("ses-root").expect("bind root");
    let client = ScriptedClient::new(|_, params| {
        assert_eq!(params["sessionId"], "ses-root");
        assert_eq!(params["_meta"]["zuno"]["childSessionId"], "ses-durable");
        Ok(json!({"action": "cancel"}))
    });
    let port = port_for(&view);
    AcpQuestionPresenter::new(port.clone(), client.connection())
        .with_route(route)
        .present(view.clone())
        .await
        .expect("routed presentation");
    assert_eq!(port.commands()[0].0, view.origin.session_id);
}

#[tokio::test]
async fn terminal_and_invalid_views_are_never_sent() {
    let mut terminal = view();
    terminal.state = QuestionState::Answered;
    let port = port_for(&terminal);
    let client = ScriptedClient::unreachable();
    let presenter = AcpQuestionPresenter::new(port.clone(), client.connection());
    assert!(
        presenter
            .present(terminal.clone())
            .await
            .expect("terminal view")
            .is_none()
    );
    let mut invalid = terminal;
    invalid.state = QuestionState::Pending;
    invalid.questions[1].id = invalid.questions[0].id.clone();
    assert!(matches!(
        presenter.present(invalid).await,
        Err(QuestionError::Invalid(_))
    ));
    assert!(client.methods().is_empty());
    assert!(port.commands().is_empty());
}

#[tokio::test]
async fn failed_rpc_delivery_never_applies_a_durable_command() {
    for error in [
        RpcError::internal("disconnected"),
        RpcError::method_not_found("elicitation/create"),
        RpcError::cancelled("client withdrew the RPC"),
    ] {
        let view = view();
        let port = port_for(&view);
        let client = ScriptedClient::new(move |_, _| Err(error.clone()));
        let result = AcpQuestionPresenter::new(port.clone(), client.connection())
            .present(view.clone())
            .await
            .expect("delivery unavailable");
        assert!(result.is_none());
        assert!(port.commands().is_empty());
        assert_eq!(port.stored, view);
    }
}

#[tokio::test]
async fn session_scope_preserves_after_response_notification_ordering() {
    let client = ScriptedClient::unreachable();
    let prompt = client.connection().request_scoped();
    let session = prompt.session_scoped();
    session
        .session_update_after_response("ses-durable", json!({"queued": true}))
        .expect("queue after-response update");
    assert!(client.methods().is_empty());
    prompt
        .flush_after_response()
        .await
        .expect("flush after prompt response");
    assert_eq!(client.methods(), vec!["session/update"]);
    assert!(
        session
            .session_update_after_response("ses-durable", json!({}))
            .is_err()
    );
}

struct PresenterAgent {
    view: QuestionView,
    port: Arc<ScriptedPort>,
    release: Arc<Notify>,
    presentation: Arc<Mutex<Option<JoinHandle<Presented>>>>,
}

#[async_trait]
impl Agent for PresenterAgent {
    async fn request(
        &self,
        method: &str,
        _request: &RequestId,
        _params: Value,
        client: ClientConnection,
    ) -> Result<Value, RpcError> {
        if method == "initialize" {
            return Ok(json!({}));
        }
        if method != "session/prompt" {
            return Err(RpcError::method_not_found(method));
        }
        let presenter = AcpQuestionPresenter::new(self.port.clone(), client);
        let view = self.view.clone();
        *lock(&self.presentation) =
            Some(tokio::spawn(async move { presenter.present(view).await }));
        self.release.notified().await;
        Ok(json!({"stopReason": "end_turn"}))
    }

    async fn notification(
        &self,
        method: &str,
        _params: Value,
        _client: ClientConnection,
    ) -> Result<(), RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

struct LivePresentation {
    input: DuplexStream,
    output: BufReader<DuplexStream>,
    server: JoinHandle<Result<(), ServeError>>,
    release: Arc<Notify>,
    presentation: Arc<Mutex<Option<JoinHandle<Presented>>>>,
    port: Arc<ScriptedPort>,
    elicitation: Value,
}

impl LivePresentation {
    async fn start(view: QuestionView) -> Self {
        let port = port_for(&view);
        let release = Arc::new(Notify::new());
        let presentation = Arc::new(Mutex::new(None));
        let agent = PresenterAgent {
            view,
            port: Arc::clone(&port),
            release: Arc::clone(&release),
            presentation: Arc::clone(&presentation),
        };
        let (mut input, reader) = tokio::io::duplex(8192);
        let (writer, output) = tokio::io::duplex(8192);
        let mut output = BufReader::new(output);
        let server = tokio::spawn(serve(agent, reader, writer));
        send_client_frame(
            &mut input,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {},
            }),
        )
        .await;
        assert_eq!(next_client_frame(&mut output).await["id"], 1);
        send_client_frame(
            &mut input,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "session/prompt", "params": {},
            }),
        )
        .await;
        let elicitation = next_client_frame(&mut output).await;
        assert_eq!(elicitation["method"], "elicitation/create");
        Self {
            input,
            output,
            server,
            release,
            presentation,
            port,
            elicitation,
        }
    }

    async fn finish_prompt(&mut self) {
        self.release.notify_one();
        let finished = next_client_frame(&mut self.output).await;
        assert_eq!(
            finished["id"], 2,
            "prompt cleanup cancelled the question RPC"
        );
        assert_eq!(finished["result"]["stopReason"], "end_turn");
        assert!(self.port.commands().is_empty());
    }

    async fn result(&self) -> Presented {
        let task = lock(&self.presentation)
            .take()
            .expect("supervised presentation");
        timeout(Duration::from_secs(2), task)
            .await
            .expect("presentation finishes")
            .expect("presentation task joins")
    }

    async fn close(mut self) {
        self.input.shutdown().await.expect("close ACP input");
        timeout(Duration::from_secs(2), self.server)
            .await
            .expect("server exits")
            .expect("server task joins")
            .expect("server exits cleanly");
    }
}

async fn send_client_frame(input: &mut DuplexStream, frame: Value) {
    let mut encoded = serde_json::to_vec(&frame).expect("encode client frame");
    encoded.push(b'\n');
    input.write_all(&encoded).await.expect("write client frame");
}

async fn next_client_frame(output: &mut BufReader<DuplexStream>) -> Value {
    let mut line = String::new();
    let size = timeout(Duration::from_secs(2), output.read_line(&mut line))
        .await
        .expect("ACP frame arrives")
        .expect("read ACP frame");
    assert_ne!(size, 0, "unexpected ACP EOF");
    serde_json::from_str(&line).expect("ACP frame is JSON")
}

#[tokio::test]
async fn late_response_survives_prompt_completion_and_prompt_cancellation() {
    for cancelled in [false, true] {
        let view = view();
        let mut live = LivePresentation::start(view.clone()).await;
        if cancelled {
            send_client_frame(
                &mut live.input,
                json!({
                    "jsonrpc": "2.0", "method": "$/cancel_request",
                    "params": {"requestId": 2},
                }),
            )
            .await;
            let cancelled = next_client_frame(&mut live.output).await;
            assert_eq!(
                cancelled["id"], 2,
                "prompt cancellation leaked to the question"
            );
            assert_eq!(cancelled["error"]["code"], -32800);
        } else {
            live.finish_prompt().await;
        }
        assert!(live.port.commands().is_empty());
        send_client_frame(
            &mut live.input,
            json!({
                "jsonrpc": "2.0", "id": live.elicitation["id"],
                "result": {"action": "accept", "content": {"answer:item-notes": "late answer"}},
            }),
        )
        .await;
        assert!(
            live.result()
                .await
                .expect("late response is accepted")
                .is_some()
        );
        let commands = live.port.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, view.origin.session_id);
        assert_eq!(commands[0].1, view.id);
        assert_eq!(commands[0].2.expected_revision, view.revision);
        assert_eq!(
            commands[0].2.action,
            QuestionAction::Answer {
                answers: BTreeMap::from([(
                    "item-notes".to_owned(),
                    vec!["late answer".to_owned()]
                )]),
            }
        );
        live.close().await;
    }
}

#[tokio::test]
async fn disconnect_ends_session_scoped_presentation_without_settling_the_row() {
    let view = view();
    let mut live = LivePresentation::start(view.clone()).await;
    live.finish_prompt().await;
    live.input.shutdown().await.expect("disconnect client");
    assert!(
        live.result()
            .await
            .expect("disconnected presentation")
            .is_none()
    );
    assert!(live.port.commands().is_empty());
    assert_eq!(live.port.stored, view);
    live.close().await;
}

#[tokio::test]
async fn supervised_task_abort_drops_the_rpc_without_settling_the_row() {
    let mut live = LivePresentation::start(view()).await;
    live.finish_prompt().await;
    let task = lock(&live.presentation)
        .take()
        .expect("supervised presentation");
    task.abort();
    assert!(task.await.expect_err("aborted presentation").is_cancelled());
    send_client_frame(
        &mut live.input,
        json!({
            "jsonrpc": "2.0", "id": live.elicitation["id"],
            "result": {"action": "accept", "content": {"answer:item-approach": "Keep"}},
        }),
    )
    .await;
    // The subsequent response proves the reader processed the abandoned RPC's
    // late response first. It must not apply that response to the durable port.
    send_client_frame(
        &mut live.input,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "test/check", "params": {},
        }),
    )
    .await;
    assert_eq!(next_client_frame(&mut live.output).await["id"], 3);
    assert!(live.port.commands().is_empty());
    live.close().await;
}

#[tokio::test]
async fn a_closed_writer_leaves_no_command_or_pending_rpc() {
    let (output, frames) = mpsc::channel(1);
    drop(frames);
    let pending = Arc::new(Mutex::new(PendingState::default()));
    let connection = ClientConnection {
        output,
        pending: Arc::clone(&pending),
        next_id: Arc::new(AtomicU64::new(1)),
        deferred: None,
        scoped_requests: None,
    };
    let view = view();
    let port = port_for(&view);
    assert!(
        AcpQuestionPresenter::new(port.clone(), connection)
            .present(view.clone())
            .await
            .expect("failed delivery")
            .is_none()
    );
    assert!(port.commands().is_empty());
    assert_eq!(port.stored, view);
    assert!(lock(&pending).waiters.is_empty());
}
