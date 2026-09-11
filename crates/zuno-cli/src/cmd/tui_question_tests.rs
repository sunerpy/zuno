use super::*;

use crate::cmd::tui_permission::{PermissionBridge, PermissionBroker};
use std::collections::BTreeMap;
use std::time::Duration;
use zuno_tui::app::{AppEvent, Component, render_offscreen};
use zuno_tui::keybind::ActionComponent;
use zuno_tui::views::dialog::ObservedBase;
use zuno_tui::views::message::TranscriptView;
use zuno_types::question::{PlanQuestionBinding, QuestionItem, QuestionOrigin};

const SESSION: &str = "ses_questions";

fn database() -> Arc<zuno_db::Pool> {
    database_at(&zuno_paths::DbLocation::Memory)
}

fn database_at(location: &zuno_paths::DbLocation) -> Arc<zuno_db::Pool> {
    let pool = Arc::new(zuno_db::Pool::open(location).expect("database"));
    let mut connection = pool.get().expect("connection");
    zuno_db::migration::apply(&mut connection).expect("schema");
    connection
        .execute(
            "INSERT INTO project \
         (id,worktree,vcs,name,icon_url,icon_url_override,icon_color,time_created,\
          time_updated,time_initialized,sandboxes,commands) \
         VALUES ('prj','/tmp',NULL,NULL,NULL,NULL,NULL,1,1,NULL,'[]',NULL)",
            [],
        )
        .expect("project");
    connection
        .execute(
            "INSERT INTO session \
         (id,project_id,slug,directory,title,version,time_created,time_updated) \
         VALUES (?1,'prj',?1,'/tmp',?1,'test',1,1)",
            [SESSION],
        )
        .expect("session");
    drop(connection);
    pool
}

fn request(text: &str, header: &str) -> QuestionRequest {
    zuno_types::question::QuestionPrompt::new(
        text,
        header,
        vec![
            zuno_types::question::QuestionOption::new("First", "the first choice"),
            zuno_types::question::QuestionOption::new("Second", "the second choice"),
        ],
    )
    .into_request()
}

fn spec(mode: QuestionMode, questions: Vec<QuestionRequest>) -> QuestionSpec {
    QuestionSpec {
        origin: QuestionOrigin {
            session_id: SESSION.to_owned(),
            message_id: None,
            call_id: None,
            turn_id: None,
            goal_id: None,
        },
        mode,
        purpose: QuestionPurpose::Clarification,
        questions,
        expected_goal_revision: None,
        plan: None,
    }
}

fn broker(service: Arc<QuestionService>) -> (Arc<QuestionBroker>, mpsc::Receiver<TerminalEvent>) {
    let (sender, receiver) = zuno_tui::app::terminal_event_channel();
    let broker = Arc::new(QuestionBroker::new(sender));
    broker
        .attach_service(service, SESSION)
        .expect("bind service");
    (broker, receiver)
}

fn bridge(broker: &Arc<QuestionBroker>) -> PermissionBridge {
    let context = ViewContext::defaults();
    let host = DialogHost::new(
        context.clone(),
        Box::new(ObservedBase::new(TranscriptView::new(context.clone()))),
    );
    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    PermissionBridge::new(context.clone(), Arc::new(PermissionBroker::new(wake)), host)
        .with_question(QuestionBridge::new(context, Arc::clone(broker)))
}

fn key(code: zuno_tui::crossterm::event::KeyCode) -> KeyEvent {
    KeyEvent::new(code, zuno_tui::crossterm::event::KeyModifiers::NONE)
}

fn action(name: &str) -> &'static Definition {
    zuno_tui::keybind::definition(name).expect("registered action")
}

fn apply_action(bridge: &mut impl ActionComponent, name: &str) {
    bridge.handle_action(
        action(name),
        &key(zuno_tui::crossterm::event::KeyCode::Enter),
    );
}

fn rendered_text(component: &mut impl Component) -> String {
    let rendered = render_offscreen(component, 120, 28).expect("frame");
    (0..rendered.area.height)
        .map(|y| {
            (0..rendered.area.width)
                .map(|x| rendered[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_for_frame(
    bridge: &mut impl Component,
    wake: &mut mpsc::Receiver<TerminalEvent>,
    text: &str,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Wake));
            if rendered_text(bridge).contains(text) {
                break;
            }
            wake.recv().await.expect("presenter wake");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("frame did not contain {text:?}:\n{}", rendered_text(bridge)));
}

async fn wait_for_revision(service: &QuestionService, id: &str, revision: i64) -> QuestionView {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = service.get(SESSION, id).await.expect("question");
            if view.revision >= revision {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("question commit")
}

fn type_in_dialog(bridge: &mut impl Component, text: &str) {
    for character in text.chars() {
        bridge.handle_event(&AppEvent::Terminal(TerminalEvent::Input(
            zuno_tui::crossterm::event::Event::Key(key(zuno_tui::crossterm::event::KeyCode::Char(
                character,
            ))),
        )));
    }
}

async fn save_draft(
    service: &QuestionService,
    view: &QuestionView,
    command_id: &str,
    draft_answers: QuestionAnswers,
) -> QuestionReceipt {
    service
        .apply(
            &view.origin.session_id,
            &view.id,
            QuestionCommand {
                command_id: command_id.to_owned(),
                expected_revision: view.revision,
                action: QuestionAction::Defer { draft_answers },
            },
        )
        .await
        .expect("save draft")
}

#[tokio::test]
async fn full_ctrl_s_draft_survives_database_reopen_without_becoming_an_answer() {
    const SECRET: &str = "unsubmitted custom terminal choice";
    let directory = tempfile::tempdir().expect("directory");
    let location = zuno_paths::DbLocation::File(directory.path().join("questions.db"));
    let saved = {
        let pool = database_at(&location);
        let service = Arc::new(QuestionService::new(Arc::clone(&pool)));
        let (broker, mut wake) = broker(Arc::clone(&service));
        let opened = broker
            .open(spec(
                QuestionMode::Blocking,
                vec![
                    request("First choice", "First"),
                    request("Terminal choice", "Terminal"),
                ],
            ))
            .await
            .expect("open")
            .question;
        let (shutdown, stopping) = watch::channel(false);
        let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
        let mut bridge = bridge(&broker);
        wait_for_frame(&mut bridge, &mut wake, "First choice").await;
        apply_action(&mut bridge, "dialog.select.submit");
        apply_action(&mut bridge, "dialog.select.next");
        apply_action(&mut bridge, "dialog.select.next");
        apply_action(&mut bridge, "dialog.select.submit");
        type_in_dialog(&mut bridge, SECRET);
        apply_action(&mut bridge, "dialog.question.defer");
        let saved = wait_for_revision(&service, &opened.id, 2).await;
        assert_eq!(saved.state, QuestionState::Pending);
        assert!(saved.answers.is_empty());
        assert_eq!(
            positional_answers(&saved),
            [vec!["First".to_owned()], vec![SECRET.to_owned()]]
        );
        assert!(!saved.is_fully_answered());
        assert!(
            zuno_db::inbox::SessionInbox::new(pool)
                .pending(SESSION)
                .expect("inbox")
                .is_empty()
        );
        drop(bridge);
        shutdown.send(true).expect("disconnect");
        worker.await.expect("stopped");
        saved
    };

    let pool = Arc::new(zuno_db::Pool::open(&location).expect("reopen database"));
    let service = Arc::new(QuestionService::new(Arc::clone(&pool)));
    let (broker, mut wake) = broker(Arc::clone(&service));
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
    let mut bridge = bridge(&broker);
    broker.show_questions(&saved.id).expect("reopen form");
    wait_for_frame(&mut bridge, &mut wake, "First choice").await;
    apply_action(&mut bridge, "dialog.question.next_question");
    assert!(rendered_text(&mut bridge).contains(SECRET));
    assert_eq!(
        service.get(SESSION, &saved.id).await.expect("unchanged"),
        saved
    );
    let empty = service
        .apply(
            SESSION,
            &saved.id,
            QuestionCommand {
                command_id: "empty-after-reopen".to_owned(),
                expected_revision: saved.revision,
                action: QuestionAction::Answer {
                    answers: QuestionAnswers::new(),
                },
            },
        )
        .await
        .expect("empty answer");
    assert_eq!(empty.question.state, QuestionState::Pending);
    assert!(empty.question.answers.is_empty());
    assert_eq!(empty.question.draft_answers, saved.draft_answers);
    assert!(empty.input_id.is_none());
    assert!(
        zuno_db::inbox::SessionInbox::new(pool)
            .pending(SESSION)
            .expect("no model input")
            .is_empty()
    );
    shutdown.send(true).expect("shutdown");
    worker.await.expect("stopped");
}

#[tokio::test]
async fn full_drafts_and_empty_answers_never_resume_required_work_with_or_without_a_goal() {
    use zuno_db::session_execution::SessionExecutionStore;
    use zuno_engine::status::SessionRunRegistry;
    use zuno_types::execution::SessionReadiness;

    for with_goal in [false, true] {
        let pool = database();
        let spill = tempfile::tempdir().expect("goal spill");
        let goals = zuno_goal::GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
            .expect("goals");
        let mut definition = spec(
            QuestionMode::Blocking,
            vec![request("Required choice", "Required")],
        );
        definition.purpose = QuestionPurpose::RequiredInput;
        if with_goal {
            let goal = goals
                .create_goal(SESSION, "continue only after an answer", None)
                .expect("goal");
            definition.origin.goal_id = Some(goal.goal_id);
            definition.expected_goal_revision = Some(goal.revision);
        }
        let runs = SessionRunRegistry::new();
        let guard = runs.begin_turn(SESSION).expect("source turn");
        let service = QuestionService::new(Arc::clone(&pool)).with_runs(runs);
        let opened = service
            .open(definition)
            .await
            .expect("required question")
            .question;
        let execution = SessionExecutionStore::new(Arc::clone(&pool));
        let before = execution.get(SESSION).expect("execution").expect("waiting");
        assert_eq!(
            before.scheduling.as_ref().expect("scheduling").readiness,
            SessionReadiness::WaitingHuman {
                request_id: opened.id.clone()
            }
        );
        let goal_before =
            serde_json::to_value(goals.goal(SESSION).expect("goal")).expect("snapshot");
        let draft_answers = QuestionAnswers::from([(
            opened.questions[0].id.clone(),
            vec!["SECRET_UNSUBMITTED_DRAFT".to_owned()],
        )]);
        let saved = save_draft(
            &service,
            &opened,
            "full-required-draft",
            draft_answers.clone(),
        )
        .await;
        assert_eq!(saved.question.state, QuestionState::Pending);
        assert!(saved.question.answers.is_empty());
        assert_eq!(saved.question.draft_answers, draft_answers);
        assert!(saved.input_id.is_none());
        let empty = service
            .apply(
                SESSION,
                &opened.id,
                QuestionCommand {
                    command_id: "empty-required-answer".to_owned(),
                    expected_revision: saved.question.revision,
                    action: QuestionAction::Answer {
                        answers: QuestionAnswers::new(),
                    },
                },
            )
            .await
            .expect("empty answer");
        assert!(empty.input_id.is_none());
        assert_eq!(
            execution.get(SESSION).expect("unchanged execution"),
            Some(before)
        );
        assert_eq!(
            serde_json::to_value(goals.goal(SESSION).expect("unchanged goal")).expect("goal"),
            goal_before
        );
        assert!(
            guard
                .take_soft_interrupts_at_safe_point()
                .messages
                .is_empty()
        );
        let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&pool));
        assert!(
            inbox
                .pending(SESSION)
                .expect("no draft/status inputs")
                .is_empty()
        );

        let answered = service
            .apply(
                SESSION,
                &opened.id,
                QuestionCommand {
                    command_id: "explicit-required-answer".to_owned(),
                    expected_revision: empty.question.revision,
                    action: QuestionAction::Answer {
                        answers: QuestionAnswers::from([(
                            opened.questions[0].id.clone(),
                            vec!["First".to_owned()],
                        )]),
                    },
                },
            )
            .await
            .expect("explicit answer");
        assert_eq!(answered.question.state, QuestionState::Answered);
        assert!(answered.question.draft_answers.is_empty());
        assert!(answered.input_id.is_some());
        let input = inbox
            .get(SESSION, answered.input_id.as_ref().expect("id"))
            .expect("read")
            .expect("input");
        assert!(
            !input
                .prompt
                .to_string()
                .contains("SECRET_UNSUBMITTED_DRAFT")
        );
        let messages = guard.take_soft_interrupts_at_safe_point();
        assert!(messages.messages.len() <= 1);
        assert!(
            messages
                .messages
                .iter()
                .all(|message| { !message.content.contains("SECRET_UNSUBMITTED_DRAFT") })
        );
        assert_eq!(
            execution
                .get(SESSION)
                .expect("answered execution")
                .expect("state")
                .scheduling
                .expect("scheduling")
                .readiness,
            SessionReadiness::Ready
        );
        if with_goal {
            assert_eq!(
                goals.goal(SESSION).expect("goal").expect("active").status,
                zuno_goal::GoalStatus::Active
            );
        }
    }
}

#[tokio::test]
async fn plan_draft_captures_source_cycle_without_approval_and_stale_supersede_still_works() {
    use zuno_db::session_execution::SessionExecutionStore;
    use zuno_types::execution::{CollaborationMode, TurnExecutionIdentity};

    let pool = database();
    pool.get().expect("connection").execute(
        "INSERT INTO work_plan (session_id,id,goal_id,revision,title,steps,time_created,time_updated)
         VALUES (?1,'plan-source',NULL,1,'Plan one',
           '[{\"id\":\"step\",\"title\":\"Implement\",\"status\":\"in_progress\"}]',1,1)",
        [SESSION],
    ).expect("plan");
    let execution = SessionExecutionStore::new(Arc::clone(&pool));
    let mut state = execution
        .seed(
            SESSION,
            CollaborationMode::Plan,
            Some(TurnExecutionIdentity::new("build", "provider", "model")),
            1,
        )
        .expect("Plan mode");
    state.cycle_id = Some("logical-plan-cycle".to_owned());
    execution.update(state.revision, state).expect("cycle");
    let service = QuestionService::new(Arc::clone(&pool));
    let mut definition = spec(QuestionMode::Deferred, Vec::new());
    definition.purpose = QuestionPurpose::PlanAuthorization;
    definition.origin.turn_id = Some("engine-turn-one".to_owned());
    let opened = service
        .open(definition.clone())
        .await
        .expect("Plan question")
        .question;
    assert_eq!(
        opened
            .plan
            .as_ref()
            .expect("binding")
            .source_cycle_id
            .as_deref(),
        Some("logical-plan-cycle")
    );
    let waiting = execution.get(SESSION).expect("waiting");
    let saved = save_draft(
        &service,
        &opened,
        "save-plan-draft",
        QuestionAnswers::from([(opened.questions[0].id.clone(), vec!["approve".to_owned()])]),
    )
    .await;
    assert_eq!(saved.question.state, QuestionState::Pending);
    assert!(saved.question.decision.is_none());
    assert!(saved.question.authorization.is_none());
    assert!(saved.input_id.is_none());
    assert_eq!(
        execution.get(SESSION).expect("Plan is still waiting"),
        waiting
    );
    assert!(
        zuno_db::inbox::SessionInbox::new(Arc::clone(&pool))
            .pending(SESSION)
            .expect("inbox")
            .is_empty()
    );
    pool.get()
        .expect("connection")
        .execute(
            "UPDATE work_plan SET revision=2,title='Plan two' WHERE session_id=?1",
            [SESSION],
        )
        .expect("new Plan revision");
    definition.origin.turn_id = Some("engine-turn-two".to_owned());
    let replacement = service
        .open(definition)
        .await
        .expect("superseding question")
        .question;
    assert_ne!(replacement.id, opened.id);
    assert_eq!(
        replacement
            .plan
            .as_ref()
            .expect("new binding")
            .plan_revision,
        2
    );
    assert_eq!(
        replacement
            .plan
            .as_ref()
            .expect("new binding")
            .source_cycle_id
            .as_deref(),
        Some("logical-plan-cycle")
    );
    let superseded = service
        .get(SESSION, &opened.id)
        .await
        .expect("old question");
    assert_eq!(superseded.state, QuestionState::Cancelled);
    assert_eq!(
        superseded.authorization,
        Some(zuno_types::question::PlanAuthorizationState::Invalidated)
    );
}

#[tokio::test]
async fn publishing_returns_a_receipt_without_a_presenter_or_inline_answers() {
    let pool = database();
    let service = Arc::new(QuestionService::new(Arc::clone(&pool)));
    let (broker, _wake) = broker(service);
    let receipt = tokio::time::timeout(
        Duration::from_secs(2),
        broker.open(spec(
            QuestionMode::Deferred,
            vec![request("Choose a channel", "Channel")],
        )),
    )
    .await
    .expect("publication cannot wait for the UI")
    .expect("published");
    assert_eq!(receipt.question.state, QuestionState::Pending);
    assert!(receipt.question.answers.is_empty());
    assert!(receipt.input_id.is_none());
    assert!(
        zuno_db::inbox::SessionInbox::new(pool)
            .pending(SESSION)
            .expect("inbox")
            .is_empty()
    );
}

#[tokio::test]
async fn a_full_presentation_queue_does_not_block_publication_or_shutdown() {
    let service = Arc::new(QuestionService::new(database()));
    let (broker, _wake) = broker(Arc::clone(&service));
    for _ in 0..QUESTION_CHANNEL_CAPACITY {
        assert!(
            broker.updates.try_send(PresenterUpdate::List).is_ok(),
            "fill bounded updates"
        );
    }
    let opened = tokio::time::timeout(
        Duration::from_secs(1),
        broker.open(spec(
            QuestionMode::Deferred,
            vec![request("Independent question", "Question")],
        )),
    )
    .await
    .expect("publication is independent of rendering")
    .expect("published");
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
    tokio::task::yield_now().await;
    shutdown.send(true).expect("stop presenter");
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("full presentation queue remains interruptible")
        .expect("worker");
    assert_eq!(
        service
            .get(SESSION, &opened.question.id)
            .await
            .expect("retained")
            .state,
        QuestionState::Pending
    );
}

#[tokio::test]
async fn answers_are_admitted_once_using_stored_ids_and_revision() {
    let pool = database();
    let service = Arc::new(QuestionService::new(Arc::clone(&pool)));
    let (broker, mut wake) = broker(Arc::clone(&service));
    let view = broker
        .open(spec(
            QuestionMode::Blocking,
            vec![
                request("First question", "One"),
                request("Second question", "Two"),
            ],
        ))
        .await
        .expect("published")
        .question;
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
    let mut bridge = bridge(&broker);
    wait_for_frame(&mut bridge, &mut wake, "First question").await;
    apply_action(&mut bridge, "dialog.select.submit");
    apply_action(&mut bridge, "dialog.select.next");
    apply_action(&mut bridge, "dialog.select.submit");
    let answered = wait_for_revision(&service, &view.id, 2).await;
    assert_eq!(answered.state, QuestionState::Answered);
    assert_eq!(
        positional_answers(&answered),
        [vec!["First".to_owned()], vec!["Second".to_owned()]]
    );
    let command = question_command(
        &view,
        QuestionAction::Answer {
            answers: answered.answers.clone(),
        },
    )
    .expect("command");
    let replay = service
        .apply(SESSION, &view.id, command)
        .await
        .expect("duplicate");
    assert!(replay.duplicate);
    assert_eq!(
        zuno_db::inbox::SessionInbox::new(pool)
            .pending(SESSION)
            .expect("inbox")
            .len(),
        1
    );
    shutdown.send(true).expect("shutdown");
    worker.await.expect("presenter stopped");
}

#[tokio::test]
async fn partial_defer_reopens_stored_answers_after_the_surface_disconnects() {
    let service = Arc::new(QuestionService::new(database()));
    let (first_broker, mut wake) = broker(Arc::clone(&service));
    let view = first_broker
        .open(spec(
            QuestionMode::Blocking,
            vec![
                request("Choose first", "One"),
                request("Choose second", "Two"),
            ],
        ))
        .await
        .expect("published")
        .question;
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&first_broker).run(stopping));
    let mut first_bridge = bridge(&first_broker);
    wait_for_frame(&mut first_bridge, &mut wake, "Choose first").await;
    apply_action(&mut first_bridge, "dialog.select.submit");
    apply_action(&mut first_bridge, "dialog.question.defer");
    let deferred = wait_for_revision(&service, &view.id, 2).await;
    assert_eq!(deferred.state, QuestionState::Pending);
    assert_eq!(deferred.mode, QuestionMode::Deferred);
    assert!(
        deferred.answers.is_empty(),
        "Ctrl+S does not confirm even partial form input"
    );
    assert_eq!(deferred.draft_answers[&view.questions[0].id], ["First"]);
    assert_eq!(
        positional_answers(&deferred),
        [vec!["First".to_owned()], vec![]]
    );
    drop(first_bridge);
    shutdown.send(true).expect("disconnect");
    worker.await.expect("stopped");
    drop(first_broker);

    let (recovered, mut wake) = broker(Arc::clone(&service));
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&recovered).run(stopping));
    let mut recovered_bridge = bridge(&recovered);
    recovered.show_questions("list").expect("list handler");
    wait_for_frame(&mut recovered_bridge, &mut wake, "Questions (1 pending)").await;
    apply_action(&mut recovered_bridge, "dialog.select.submit");
    wait_for_frame(
        &mut recovered_bridge,
        &mut wake,
        "Question 2/2 (1 unanswered)",
    )
    .await;
    assert!(rendered_text(&mut recovered_bridge).contains("Choose second"));
    assert_eq!(
        service
            .get(SESSION, &view.id)
            .await
            .expect("request")
            .revision,
        2
    );
    shutdown.send(true).expect("shutdown");
    worker.await.expect("stopped");
}

#[tokio::test]
async fn escape_cancels_without_answers_or_an_approval() {
    let service = Arc::new(QuestionService::new(database()));
    let (broker, mut wake) = broker(Arc::clone(&service));
    let view = broker
        .open(spec(
            QuestionMode::Blocking,
            vec![request("Choose", "Choice")],
        ))
        .await
        .expect("published")
        .question;
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
    let mut bridge = bridge(&broker);
    wait_for_frame(&mut bridge, &mut wake, "Choose").await;
    bridge.handle_action(
        action("session_interrupt"),
        &key(zuno_tui::crossterm::event::KeyCode::Esc),
    );
    let cancelled = wait_for_revision(&service, &view.id, 2).await;
    assert_eq!(cancelled.state, QuestionState::Cancelled);
    assert!(cancelled.answers.is_empty());
    assert!(cancelled.decision.is_none());
    shutdown.send(true).expect("shutdown");
    worker.await.expect("stopped");
}

#[tokio::test]
async fn a_deferred_question_does_not_open_a_modal_until_requested() {
    let service = Arc::new(QuestionService::new(database()));
    let (broker, mut wake) = broker(Arc::clone(&service));
    let view = broker
        .open(spec(
            QuestionMode::Deferred,
            vec![request("Optional detail", "Detail")],
        ))
        .await
        .expect("published")
        .question;
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
    let mut bridge = bridge(&broker);
    wait_for_frame(&mut bridge, &mut wake, "1 pending question(s)").await;
    assert!(!rendered_text(&mut bridge).contains("Optional detail"));
    broker
        .show_questions(&format!("open {}", view.id))
        .expect("reopen handler");
    wait_for_frame(&mut bridge, &mut wake, "Optional detail").await;
    drop(bridge);
    shutdown.send(true).expect("disconnect");
    worker.await.expect("stopped");
    assert_eq!(
        service.get(SESSION, &view.id).await.expect("request").state,
        QuestionState::Pending
    );
}

fn plan_view(review_gate: zuno_review::PlanReviewGate) -> QuestionView {
    QuestionView {
        id: "que_plan".to_owned(),
        origin: QuestionOrigin {
            session_id: SESSION.to_owned(),
            message_id: Some("msg_plan".to_owned()),
            call_id: Some("call_plan".to_owned()),
            turn_id: Some("turn_plan".to_owned()),
            goal_id: None,
        },
        revision: 7,
        mode: QuestionMode::Deferred,
        purpose: QuestionPurpose::PlanAuthorization,
        state: QuestionState::Pending,
        questions: vec![QuestionItem {
            id: "stable-choice".to_owned(),
            question: QuestionRequest::closed(
                "Start the stored Plan?",
                "Start Work",
                vec![
                    zuno_types::question::QuestionOption::new("approve", "Start Work"),
                    zuno_types::question::QuestionOption::new("decline", "Keep planning"),
                ],
            ),
        }],
        answers: BTreeMap::new(),
        draft_answers: BTreeMap::new(),
        plan: Some(PlanQuestionBinding {
            plan_id: "plan_exact".to_owned(),
            plan_revision: 12,
            source_cycle_id: None,
            title: "Stored Plan".to_owned(),
            completed_steps: 1,
            total_steps: 3,
            work_identity: zuno_types::execution::TurnExecutionIdentity::new(
                "build", "test", "model",
            ),
            review_gate: serde_json::to_value(review_gate).expect("gate"),
        }),
        decision: None,
        authorization: None,
        time_created: 1,
        time_updated: 1,
    }
}

#[test]
fn draft_prefill_preserves_blank_overrides_and_other_confirmed_answers() {
    let mut view = plan_view(zuno_review::PlanReviewGate::Unbound);
    view.purpose = QuestionPurpose::Clarification;
    view.plan = None;
    view.questions = ["first", "second", "third"]
        .into_iter()
        .map(|id| QuestionItem {
            id: id.to_owned(),
            question: request("Choose", id),
        })
        .collect();
    view.answers = QuestionAnswers::from([
        ("first".to_owned(), vec!["First".to_owned()]),
        ("second".to_owned(), vec!["Second".to_owned()]),
    ]);
    view.draft_answers = QuestionAnswers::from([
        ("first".to_owned(), Vec::new()),
        ("third".to_owned(), vec!["unsubmitted".to_owned()]),
    ]);
    assert_eq!(
        positional_answers(&view),
        [
            Vec::new(),
            vec!["Second".to_owned()],
            vec!["unsubmitted".to_owned()]
        ]
    );
    assert_eq!(view.answers["first"], ["First"]);
    assert!(!view.is_fully_answered());
}

fn pending_command(broker: &QuestionBroker) -> Option<QuestionCommand> {
    match locked(&broker.command_source)
        .as_mut()
        .expect("no worker")
        .try_recv()
        .ok()?
    {
        PresenterCommand::Apply { command, .. } => Some(command),
        _ => panic!("expected a response command"),
    }
}

#[test]
fn plan_authorization_accepts_only_exact_explicit_labels() {
    for answers in [
        vec![],
        vec![vec![]],
        vec![vec!["yes".to_owned()]],
        vec![vec!["approve".to_owned(), "decline".to_owned()]],
        vec![vec!["approve".to_owned()], vec![]],
    ] {
        assert!(plan_decision(&answers).is_err());
    }
    assert_eq!(
        plan_decision(&[vec!["approve".to_owned()]]),
        Ok(PlanQuestionDecision::Approve)
    );
    assert_eq!(
        plan_decision(&[vec!["decline".to_owned()]]),
        Ok(PlanQuestionDecision::Decline)
    );
}

#[test]
fn draft_approval_requires_the_users_nonempty_risk_reason() {
    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    let broker = Arc::new(QuestionBroker::new(wake));
    let mut bridge = QuestionBridge::new(ViewContext::defaults(), Arc::clone(&broker));
    let view = plan_view(zuno_review::PlanReviewGate::Draft {
        review_id: "review_exact".to_owned(),
        review_revision: 4,
    });
    bridge.open_question(view);
    bridge.resolve(vec![vec!["approve".to_owned()]]);
    assert!(pending_command(&broker).is_none());
    assert!(bridge.active.as_ref().expect("risk prompt").risk_prompt);
    bridge.risk_reason(DialogOutcome::Submitted {
        dialog: PLAN_RISK_DIALOG_ID,
        text: "   ".to_owned(),
    });
    assert!(pending_command(&broker).is_none());
    bridge.risk_reason(DialogOutcome::Submitted {
        dialog: PLAN_RISK_DIALOG_ID,
        text: "I accept the missing Windows smoke for this local prototype".to_owned(),
    });
    let command = pending_command(&broker).expect("explicit approval");
    assert_eq!(command.expected_revision, 7);
    assert_eq!(
        command.action,
        QuestionAction::PlanDecision {
            decision: PlanQuestionDecision::Approve,
            risk_reason: Some(
                "I accept the missing Windows smoke for this local prototype".to_owned()
            ),
        }
    );
}

#[test]
fn deferring_a_complete_plan_choice_never_approves_it() {
    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    let broker = Arc::new(QuestionBroker::new(wake));
    let mut bridge = QuestionBridge::new(ViewContext::defaults(), Arc::clone(&broker));
    bridge.open_question(plan_view(zuno_review::PlanReviewGate::Unbound));
    bridge.defer(vec![vec!["approve".to_owned()]]);
    assert_eq!(
        pending_command(&broker).expect("defer").action,
        QuestionAction::Defer {
            draft_answers: BTreeMap::from([(
                "stable-choice".to_owned(),
                vec!["approve".to_owned()]
            )]),
        }
    );
}

#[test]
fn optional_question_modal_does_not_claim_the_live_turn_is_waiting() {
    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    let broker = Arc::new(QuestionBroker::new(wake.clone()));
    let mut screen = SessionScreen::new(ViewContext::defaults(), wake);
    screen.status_mut().mark_running();
    let mut screen = QuestionScreen::new(screen, Arc::clone(&broker));
    screen.observe_modal(Some(zuno_tui::views::question::DIALOG_ID));
    assert!(screen.inner.status_mut().is_running());
    assert!(screen.inner.status_mut().awaiting_user().is_none());
    broker
        .presentation_blocks_turn
        .store(true, Ordering::Release);
    screen.observe_modal(Some(zuno_tui::views::question::DIALOG_ID));
    assert!(screen.inner.status_mut().awaiting_user().is_some());
}

#[test]
fn approved_work_host_replacement_updates_idle_identity_without_a_synthetic_turn() {
    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    let broker = Arc::new(QuestionBroker::new(wake.clone()));
    let mut screen = SessionScreen::new(ViewContext::defaults(), wake);
    screen.catalog_mut().agent = Some("plan".to_owned());
    screen.status_mut().set_configured_agent("plan");
    let mut screen = QuestionScreen::new(screen, Arc::clone(&broker));
    broker.host_replaced(
        zuno_types::execution::TurnExecutionIdentity::new("build", "provider", "work-model")
            .with_reasoning(Some("high")),
    );
    assert!(
        screen
            .handle_event(&AppEvent::Terminal(TerminalEvent::Wake))
            .redraw
    );
    assert_eq!(screen.inner.catalog_mut().agent.as_deref(), Some("build"));
    assert_eq!(
        screen.inner.catalog_mut().model.as_deref(),
        Some("provider/work-model")
    );
    assert!(!screen.inner.status_mut().is_running());
    assert!(
        screen
            .inner
            .transcript_mut()
            .transcript()
            .messages()
            .is_empty()
    );
}

#[tokio::test]
async fn only_committed_input_wakes_the_session_driver_after_plan_approval() {
    use futures::FutureExt as _;
    use zuno_types::question::PlanAuthorizationState;

    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    let broker = QuestionBroker::new(wake);
    let notifications = broker.work_notifications();
    let mut view = plan_view(zuno_review::PlanReviewGate::Unbound);
    view.state = QuestionState::Answered;
    view.decision = Some(PlanQuestionDecision::Approve);
    view.authorization = Some(PlanAuthorizationState::WaitingForHandoff);
    broker
        .publish(PresenterUpdate::Applied(Box::new(QuestionReceipt {
            question: view.clone(),
            input_id: None,
            duplicate: false,
        })))
        .await;
    assert!(notifications.notified().now_or_never().is_none());
    view.authorization = Some(PlanAuthorizationState::Applied);
    broker
        .publish(PresenterUpdate::Applied(Box::new(QuestionReceipt {
            question: view,
            input_id: Some("committed-work-control".to_owned()),
            duplicate: false,
        })))
        .await;
    assert!(notifications.notified().now_or_never().is_some());
    assert!(notifications.notified().now_or_never().is_none());
}

#[test]
fn response_commands_have_stable_keys_and_never_assume_positional_item_ids() {
    let mut view = plan_view(zuno_review::PlanReviewGate::Unbound);
    view.purpose = QuestionPurpose::Clarification;
    view.plan = None;
    let answers = keyed_answers(&view, vec![vec!["decline".to_owned()]]).expect("answers");
    assert_eq!(
        answers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["stable-choice"]
    );
    let action = QuestionAction::Answer { answers };
    let first = question_command(&view, action.clone()).expect("command");
    assert_eq!(
        first,
        question_command(&view, action.clone()).expect("repeat")
    );
    view.revision += 1;
    assert_ne!(
        first.command_id,
        question_command(&view, action)
            .expect("new revision")
            .command_id
    );
}

#[test]
fn invalid_questions_command_arguments_do_not_enqueue_any_action() {
    let (wake, _events) = zuno_tui::app::terminal_event_channel();
    let broker = QuestionBroker::new(wake);
    assert!(broker.show_questions("open request extra").is_err());
    assert!(
        locked(&broker.command_source)
            .as_mut()
            .expect("queue")
            .try_recv()
            .is_err()
    );
}

#[tokio::test]
async fn native_goal_resume_choice_opens_without_blocking_and_only_explicit_resume_wakes() {
    use zuno_types::execution::{CollaborationMode, TurnExecutionIdentity};
    for resume in [false, true] {
        let pool = database();
        let spill = tempfile::tempdir().expect("spill");
        let goals = zuno_goal::GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
            .expect("goals");
        goals
            .create_goal(SESSION, "Finish the approved change", None)
            .expect("goal");
        goals
            .pause_with_reason(SESSION, zuno_goal::GoalPauseReason::UserInterruption)
            .expect("pause");
        zuno_db::session_execution::SessionExecutionStore::new(Arc::clone(&pool))
            .seed(
                SESSION,
                CollaborationMode::Work,
                Some(TurnExecutionIdentity::new("build", "provider", "model")),
                1,
            )
            .expect("execution");
        let service = Arc::new(QuestionService::new(Arc::clone(&pool)));
        let (broker, mut wake) = broker(Arc::clone(&service));
        let (shutdown, stopping) = watch::channel(false);
        let worker = tokio::spawn(Arc::clone(&broker).run(stopping));
        broker
            .offer_goal_resume(SESSION, None)
            .await
            .expect("offer");
        let view = service.pending(SESSION).await.expect("pending").remove(0);
        assert_eq!(view.purpose, QuestionPurpose::GoalResume);
        let mut bridge = bridge(&broker);
        wait_for_frame(&mut bridge, &mut wake, "Resume paused Goal").await;
        assert!(!broker.presentation_blocks_turn.load(Ordering::Acquire));
        if !resume {
            apply_action(&mut bridge, "dialog.select.next");
        }
        apply_action(&mut bridge, "dialog.select.submit");
        let answered = wait_for_revision(&service, &view.id, 2).await;
        assert_eq!(answered.state, QuestionState::Answered);
        assert_eq!(
            goals.goal(SESSION).expect("goal").expect("present").status,
            if resume {
                zuno_goal::GoalStatus::Active
            } else {
                zuno_goal::GoalStatus::Paused
            }
        );
        let pending = zuno_db::inbox::SessionInbox::new(pool)
            .pending(SESSION)
            .expect("inbox");
        assert_eq!(pending.len(), usize::from(resume));
        assert!(
            pending
                .iter()
                .all(|input| input.prompt["kind"] == "sessionControl")
        );
        shutdown.send(true).expect("stop");
        worker.await.expect("stopped");
    }
}
