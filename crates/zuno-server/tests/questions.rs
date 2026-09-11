//! HTTP is a client of the durable question service, not a second interaction store.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use zuno_db::Pool;
use zuno_db::artifact_gc::ArtifactGcPaths;
use zuno_db::event_log::SessionEventLog;
use zuno_db::inbox::SessionInbox;
use zuno_db::session::SessionCreate;
use zuno_paths::DbLocation;
use zuno_server::api::{self, ApiState};
use zuno_server::{ServerBuilder, ServerConfig, ServerServices};
use zuno_session_control::{EnterPlanRequest, QuestionService, SessionControlService};
use zuno_tool::question::QuestionPort;
use zuno_tools::{PlanStep, PlanStepStatus, PlanUpdateParams, WorkStateStore};
use zuno_types::execution::{CollaborationMode, TurnExecutionIdentity};
use zuno_types::question::{
    PlanAuthorizationState, PlanQuestionDecision, QuestionAction, QuestionCommand, QuestionMode,
    QuestionOrigin, QuestionPrompt, QuestionPurpose, QuestionReceipt, QuestionSpec, QuestionState,
    QuestionView,
};

const SESSION: &str = "ses_question_api";

struct Fixture {
    _root: TempDir,
    state: ApiState,
    pool: Arc<Pool>,
    service: Arc<QuestionService>,
    services: ServerServices,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("question API fixture");
        let location = DbLocation::File(root.path().join("zuno.db"));
        let pool = Arc::new(Pool::open(&location).expect("inspection pool"));
        let state = ApiState::from_pool(
            Pool::open(&location).expect("API pool"),
            "/repo",
            ArtifactGcPaths::from_data_root(root.path()),
        )
        .expect("API state");
        for session in [SESSION, "ses_other"] {
            state
                .sessions()
                .create(&SessionCreate::new(
                    session,
                    session,
                    "global",
                    "/repo",
                    "/repo",
                    "Questions",
                    "test",
                ))
                .expect("session");
        }
        let services = ServerServices::new(64);
        zuno_goal::GoalStore::from_pool(Arc::clone(&pool), root.path().join("goal-spill"))
            .expect("Goal schema required by session control");
        let service =
            Arc::new(QuestionService::new(Arc::clone(&pool)).with_runs(services.runs.clone()));
        Self {
            _root: root,
            state,
            pool,
            service,
            services,
        }
    }

    fn app(&self) -> Router {
        self.app_with(self.service.clone())
    }

    fn app_with(&self, questions: Arc<dyn QuestionPort>) -> Router {
        ServerBuilder::new(ServerConfig::default().with_default_directory("/repo"))
            .with_services(self.services.clone())
            .with_routes(api::router_with_questions(self.state.clone(), questions))
            .router()
    }

    async fn open(&self) -> QuestionView {
        self.service
            .open(spec())
            .await
            .expect("open question")
            .question
    }

    fn counts(&self) -> (usize, usize) {
        (
            SessionEventLog::new(Arc::clone(&self.pool))
                .read_after(SESSION, None)
                .expect("events")
                .len(),
            SessionInbox::new(Arc::clone(&self.pool))
                .pending(SESSION)
                .expect("inbox")
                .len(),
        )
    }
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
            QuestionPrompt::new("Which environment?", "Environment", Vec::new()).into_request(),
            QuestionPrompt::new("Which terminal?", "Terminal", Vec::new()).into_request(),
        ],
        expected_goal_revision: None,
        plan: None,
    }
}

fn command(id: &str, revision: i64, action: QuestionAction) -> QuestionCommand {
    QuestionCommand {
        command_id: id.to_owned(),
        expected_revision: revision,
        action,
    }
}

fn answer(id: &str, revision: i64, item_id: &str, value: &str) -> QuestionCommand {
    command(
        id,
        revision,
        QuestionAction::Answer {
            answers: BTreeMap::from([(item_id.to_owned(), vec![value.to_owned()])]),
        },
    )
}

fn path(question: &QuestionView, suffix: &str) -> String {
    format!("/api/session/{SESSION}/question/{}/{suffix}", question.id)
}

async fn send(
    app: Router,
    method: Method,
    path: &str,
    body: impl Into<Body>,
) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(body.into())
                .expect("request"),
        )
        .await
        .expect("HTTP response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("response body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON response")
    };
    (status, json)
}

async fn apply(app: Router, path: &str, command: &QuestionCommand) -> (StatusCode, Value) {
    send(
        app,
        Method::POST,
        path,
        serde_json::to_vec(command).expect("command"),
    )
    .await
}

fn receipt(body: &Value) -> QuestionReceipt {
    serde_json::from_value(body["data"].clone()).expect("typed committed receipt")
}

fn validate_response(schema: &str, body: &Value) {
    let mut document = api::openapi_with_questions();
    document["$ref"] = json!(format!("#/components/schemas/{schema}"));
    jsonschema::validator_for(&document)
        .expect("OpenAPI references resolve")
        .validate(body)
        .expect("response matches the shared DTO schema");
}

#[tokio::test]
async fn question_routes_and_openapi_are_gated_by_the_actual_provider() {
    let fixture = Fixture::new();
    let without = ServerBuilder::new(ServerConfig::default())
        .with_routes(api::router(fixture.state.clone()))
        .router();
    for (method, path) in [
        (Method::GET, "/api/question/request"),
        (Method::GET, "/api/session/ses_question_api/question"),
        (
            Method::POST,
            "/api/session/ses_question_api/question/que_missing/reply",
        ),
        (
            Method::POST,
            "/api/session/ses_question_api/question/que_missing/reject",
        ),
        (
            Method::POST,
            "/api/session/ses_question_api/question/que_missing/defer",
        ),
    ] {
        assert_eq!(
            send(without.clone(), method, path, Body::empty()).await.0,
            StatusCode::NOT_FOUND
        );
    }
    for (app, enabled) in [(without, false), (fixture.app(), true)] {
        let (status, document) = send(app, Method::GET, "/api/doc", Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        for suffix in ["reply", "reject", "defer"] {
            let path = format!("/api/session/{{sessionID}}/question/{{requestID}}/{suffix}");
            assert_eq!(document["paths"][&path]["post"].is_object(), enabled);
            if enabled {
                let operation = &document["paths"][&path]["post"];
                assert_eq!(operation["requestBody"]["required"], true);
                for status in ["400", "404", "409"] {
                    assert_eq!(
                        operation["responses"][status]["content"]["application/json"]["schema"]["$ref"],
                        "#/components/schemas/QuestionErrorResponse"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn pending_questions_survive_client_and_provider_replacement() {
    let fixture = Fixture::new();
    let mut blocking = spec();
    blocking.mode = QuestionMode::Blocking;
    let opened = fixture
        .service
        .open(blocking)
        .await
        .expect("open blocking question")
        .question;
    drop(fixture.app());
    let recovered = Arc::new(QuestionService::new(Arc::clone(&fixture.pool)));
    let app = fixture.app_with(recovered);
    for path in [
        "/api/question/request",
        "/api/session/ses_question_api/question",
    ] {
        let (status, body) = send(app.clone(), Method::GET, path, Body::empty()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"], json!([opened]));
        let schema = if path == "/api/question/request" {
            "QuestionRequestListResponse"
        } else {
            "SessionQuestionResponse"
        };
        validate_response(schema, &body);
    }
}

#[tokio::test]
async fn malformed_commands_and_wrong_scopes_never_close_or_claim_a_valid_question() {
    let fixture = Fixture::new();
    let opened = fixture.open().await;
    let before = fixture.counts();
    for body in [
        String::new(),
        "{".to_owned(),
        json!({"answers": []}).to_string(),
        json!({"commandId":"empty", "expectedRevision":1, "action":{"type":"plan_decision"}}).to_string(),
        json!({"commandId":"", "expectedRevision":1, "action":{"type":"defer"}}).to_string(),
        json!({"commandId":"bad-revision", "expectedRevision":0, "action":{"type":"defer"}}).to_string(),
        json!({"commandId":"bad-answers", "expectedRevision":1, "action":{"type":"answer","answers":[["yes"]]}}).to_string(),
        json!({"commandId":"extra", "expectedRevision":1, "action":{"type":"defer"}, "unexpected":true}).to_string(),
        " ".repeat(64 * 1024 + 1),
    ] {
        let (status, body) = send(fixture.app(), Method::POST, &path(&opened, "reply"), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_request");
        assert!(!body["error"]["message"].as_str().expect("error message").is_empty());
        validate_response("QuestionErrorResponse", &body);
    }
    let defer = command(
        "defer",
        1,
        QuestionAction::Defer {
            draft_answers: Default::default(),
        },
    );
    for (session, request, status) in [
        ("invalid", opened.id.as_str(), StatusCode::BAD_REQUEST),
        ("%FF", opened.id.as_str(), StatusCode::BAD_REQUEST),
        (SESSION, "invalid", StatusCode::BAD_REQUEST),
        ("ses_missing", opened.id.as_str(), StatusCode::NOT_FOUND),
        ("ses_other", opened.id.as_str(), StatusCode::NOT_FOUND),
        (SESSION, "que_missing", StatusCode::NOT_FOUND),
    ] {
        let endpoint = format!("/api/session/{session}/question/{request}/reply");
        assert_eq!(apply(fixture.app(), &endpoint, &defer).await.0, status);
    }
    let bad_item = answer("unknown-item", 1, "not-a-question-item", "yes");
    assert_eq!(
        apply(fixture.app(), &path(&opened, "reply"), &bad_item)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .service
            .get(SESSION, &opened.id)
            .await
            .expect("question"),
        opened
    );
    assert_eq!(fixture.counts(), before);
    assert_eq!(
        apply(fixture.app(), &path(&opened, "defer"), &defer)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn drafts_and_empty_answers_stay_out_of_model_input() {
    let fixture = Fixture::new();
    let opened = fixture.open().await;
    let drafts = BTreeMap::from([
        (
            opened.questions[0].id.clone(),
            vec!["CLIENT_ONLY_ENVIRONMENT".to_owned()],
        ),
        (
            opened.questions[1].id.clone(),
            vec!["CLIENT_ONLY_TERMINAL".to_owned()],
        ),
    ]);
    let (status, body) = send(
        fixture.app(),
        Method::POST,
        &path(&opened, "defer"),
        json!({
            "commandId": "save-drafts",
            "expectedRevision": opened.revision,
            "action": {"type": "defer", "draftAnswers": drafts}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let deferred = receipt(&body);
    assert!(deferred.input_id.is_none());
    assert_eq!(deferred.question.draft_answers, drafts);
    assert!(deferred.question.answers.is_empty());
    assert_eq!(deferred.question.state, QuestionState::Pending);
    assert_eq!(fixture.counts().1, 0);
    validate_response("QuestionReceiptResponse", &body);

    let recovered = Arc::new(QuestionService::new(Arc::clone(&fixture.pool)));
    let (status, body) = send(
        fixture.app_with(recovered),
        Method::GET,
        "/api/session/ses_question_api/question",
        Body::empty(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"][0]["draftAnswers"], json!(drafts));

    // Older clients omit draftAnswers; that must preserve saved form values.
    let (status, body) = send(
        fixture.app(),
        Method::POST,
        &path(&opened, "defer"),
        json!({
            "commandId": "defer-again",
            "expectedRevision": deferred.question.revision,
            "action": {"type": "defer"}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let again = receipt(&body);
    assert!(again.input_id.is_none());
    assert_eq!(again.question.draft_answers, drafts);

    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "reply"),
        &command(
            "empty",
            again.question.revision,
            QuestionAction::Answer {
                answers: BTreeMap::new(),
            },
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let empty = receipt(&body);
    assert!(empty.input_id.is_none());
    assert!(empty.question.answers.is_empty());
    assert_eq!(empty.question.draft_answers, drafts);
    assert_eq!(fixture.counts().1, 0);

    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "reply"),
        &answer(
            "submit-one",
            empty.question.revision,
            &opened.questions[0].id,
            "Linux",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let partial = receipt(&body);
    assert_eq!(partial.question.state, QuestionState::Pending);
    assert!(
        !partial
            .question
            .draft_answers
            .contains_key(&opened.questions[0].id)
    );
    assert_eq!(
        partial.question.draft_answers[&opened.questions[1].id],
        ["CLIENT_ONLY_TERMINAL"]
    );
    let inbox = SessionInbox::new(Arc::clone(&fixture.pool));
    let input = inbox
        .get(
            SESSION,
            partial.input_id.as_deref().expect("real answer is input"),
        )
        .expect("read input")
        .expect("input");
    assert_eq!(
        input.prompt["response"],
        json!({
            "type":"answer", "answers": {(opened.questions[0].id.as_str()): ["Linux"]}
        })
    );
    for draft in ["CLIENT_ONLY_ENVIRONMENT", "CLIENT_ONLY_TERMINAL"] {
        assert!(!input.prompt.to_string().contains(draft));
    }

    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "reject"),
        &command("cancel", partial.question.revision, QuestionAction::Cancel),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cancelled = receipt(&body);
    let input = inbox
        .get(
            SESSION,
            cancelled.input_id.as_deref().expect("cancel is input"),
        )
        .expect("read cancellation")
        .expect("input");
    assert_eq!(input.prompt["response"], json!({"type":"cancel"}));
    assert!(!input.prompt.to_string().contains("CLIENT_ONLY_TERMINAL"));
    assert_eq!(fixture.counts().1, 2);
}

#[tokio::test]
async fn stable_item_answers_defer_and_idempotent_receipts_use_the_service_state() {
    let fixture = Fixture::new();
    let opened = fixture.open().await;
    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "reply"),
        &answer("terminal", 1, &opened.questions[1].id, "PowerShell"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let partial = receipt(&body);
    assert_eq!(partial.question.state, QuestionState::Pending);
    assert_eq!(partial.question.questions, opened.questions);
    assert!(
        !partial
            .question
            .answers
            .contains_key(&opened.questions[0].id)
    );
    let deferred = receipt(
        &apply(
            fixture.app(),
            &path(&opened, "defer"),
            &command(
                "later",
                partial.question.revision,
                QuestionAction::Defer {
                    draft_answers: Default::default(),
                },
            ),
        )
        .await
        .1,
    );
    assert_eq!(deferred.question.state, QuestionState::Pending);
    assert_eq!(deferred.question.answers, partial.question.answers);
    assert!(deferred.input_id.is_none());
    let completion = answer(
        "environment",
        deferred.question.revision,
        &opened.questions[0].id,
        "Windows",
    );
    let (status, body) = apply(fixture.app(), &path(&opened, "reply"), &completion).await;
    assert_eq!(status, StatusCode::OK);
    let completed = receipt(&body);
    assert_eq!(completed.question.state, QuestionState::Answered);
    assert_eq!(
        completed.question.answers[&opened.questions[0].id],
        ["Windows"]
    );
    assert_eq!(
        completed.question.answers[&opened.questions[1].id],
        ["PowerShell"]
    );
    let before = fixture.counts();
    let recovered = Arc::new(QuestionService::new(Arc::clone(&fixture.pool)));
    let (status, body) = apply(
        fixture.app_with(recovered),
        &path(&opened, "reply"),
        &completion,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let repeated = receipt(&body);
    assert!(repeated.duplicate);
    assert_eq!(repeated.input_id, completed.input_id);
    assert_eq!(repeated.question, completed.question);
    assert_eq!(fixture.counts(), before);

    let changed = answer(
        "environment",
        deferred.question.revision,
        &opened.questions[0].id,
        "Linux",
    );
    let (status, body) = apply(fixture.app(), &path(&opened, "reply"), &changed).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "question_command_conflict");
    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "reply"),
        &answer("stale", 1, &opened.questions[0].id, "Linux"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "question_revision_conflict");
    assert_eq!(body["error"]["expected"], 1);
    assert_eq!(body["error"]["actual"], completed.question.revision);
    validate_response("QuestionErrorResponse", &body);
    assert_eq!(fixture.counts(), before);
}

#[tokio::test]
async fn reject_and_defer_require_explicit_matching_commands() {
    let fixture = Fixture::new();
    let opened = fixture.open().await;
    let cancel = command("cancel", 1, QuestionAction::Cancel);
    let defer = command(
        "defer",
        1,
        QuestionAction::Defer {
            draft_answers: Default::default(),
        },
    );
    for (suffix, command) in [("reject", &defer), ("defer", &cancel)] {
        assert_eq!(
            apply(fixture.app(), &path(&opened, suffix), command)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(
                fixture.app(),
                Method::POST,
                &path(&opened, suffix),
                Body::empty()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        fixture
            .service
            .get(SESSION, &opened.id)
            .await
            .expect("pending"),
        opened
    );
    let (status, body) = apply(fixture.app(), &path(&opened, "reject"), &cancel).await;
    assert_eq!(status, StatusCode::OK);
    let cancelled = receipt(&body);
    assert_eq!(cancelled.question.state, QuestionState::Cancelled);
    assert!(cancelled.question.answers.is_empty());
    let (status, body) = apply(fixture.app(), &path(&opened, "reject"), &cancel).await;
    assert_eq!(status, StatusCode::OK);
    assert!(receipt(&body).duplicate);
    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "defer"),
        &command(
            "too-late",
            cancelled.question.revision,
            QuestionAction::Defer {
                draft_answers: Default::default(),
            },
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "question_closed");
}

#[tokio::test]
async fn only_an_explicit_plan_decision_can_record_plan_consent() {
    let fixture = Fixture::new();
    WorkStateStore::new(Arc::clone(&fixture.pool))
        .update_plan(
            SESSION,
            PlanUpdateParams {
                expected_revision: None,
                goal_id: None,
                title: "Deliver the approved change".to_owned(),
                steps: vec![PlanStep {
                    id: "implementation".to_owned(),
                    title: "Implement".to_owned(),
                    status: PlanStepStatus::InProgress,
                }],
            },
        )
        .expect("durable Plan");
    SessionControlService::new(Arc::clone(&fixture.pool))
        .enter_plan(EnterPlanRequest {
            session_id: SESSION,
            work_identity: TurnExecutionIdentity::new("build", "provider", "model"),
            at_ms: zuno_db::message::now_millis(),
        })
        .expect("enter Plan mode");
    let mut plan = spec();
    plan.purpose = QuestionPurpose::PlanAuthorization;
    plan.origin.turn_id = Some("turn_plan".to_owned());
    let opened = fixture
        .service
        .open(plan)
        .await
        .expect("open Plan question")
        .question;
    let before = fixture.counts();
    for body in [
        json!({"answers": []}),
        json!({"commandId":"empty-answer","expectedRevision":1,"action":{"type":"answer","answers":{}}}),
        json!({"commandId":"approval-label","expectedRevision":1,"action":{"type":"answer","answers":{(opened.questions[0].id.as_str()):["approve"]}}}),
        json!({"commandId":"missing-decision","expectedRevision":1,"action":{"type":"plan_decision"}}),
    ] {
        assert_eq!(
            send(
                fixture.app(),
                Method::POST,
                &path(&opened, "reply"),
                body.to_string()
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
        );
    }
    assert_eq!(
        fixture
            .service
            .get(SESSION, &opened.id)
            .await
            .expect("pending Plan question"),
        opened
    );
    assert_eq!(fixture.counts(), before);
    let (status, body) = apply(
        fixture.app(),
        &path(&opened, "defer"),
        &command(
            "not-yet",
            1,
            QuestionAction::Defer {
                draft_answers: BTreeMap::from([(
                    opened.questions[0].id.clone(),
                    vec!["approve".to_owned()],
                )]),
            },
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let deferred = receipt(&body);
    assert_eq!(deferred.question.state, QuestionState::Pending);
    assert!(deferred.question.decision.is_none());
    assert!(deferred.question.authorization.is_none());
    assert_eq!(
        deferred.question.draft_answers[&opened.questions[0].id],
        ["approve"]
    );
    assert!(deferred.input_id.is_none());
    assert_eq!(fixture.counts().1, before.1);
    let decision = command(
        "approve",
        deferred.question.revision,
        QuestionAction::PlanDecision {
            decision: PlanQuestionDecision::Approve,
            risk_reason: None,
        },
    );
    let (status, body) = apply(fixture.app(), &path(&opened, "reply"), &decision).await;
    assert_eq!(status, StatusCode::OK);
    let accepted = receipt(&body);
    assert_eq!(
        accepted.question.decision,
        Some(PlanQuestionDecision::Approve)
    );
    assert_eq!(
        accepted.question.authorization,
        Some(PlanAuthorizationState::WaitingForHandoff)
    );
    assert!(accepted.input_id.is_none());
    let state =
        zuno_db::session_execution::read_in(&fixture.pool.get().expect("connection"), SESSION)
            .expect("execution state")
            .expect("Plan state");
    assert_eq!(
        state.mode,
        CollaborationMode::Plan,
        "HTTP must not bypass the service's handoff gate"
    );
    validate_response("QuestionReceiptResponse", &body);
}
