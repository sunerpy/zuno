use super::*;

use serde_json::json;
use zuno_db::inbox::{InputDelivery, NewSessionInput, SessionInbox, SubmissionState};
use zuno_db::session_execution::SessionExecutionStore;
use zuno_types::execution::{
    CollaborationMode, InputTriggerKind, SessionPauseReason, SessionReadiness, SessionScheduling,
    TurnExecutionIdentity,
};

const SESSION: &str = "ses_tui_deferred";

fn database(
    readiness: SessionReadiness,
) -> (Arc<zuno_db::Pool>, SessionInbox, SessionExecutionStore) {
    let pool = Arc::new(zuno_db::Pool::open(&zuno_paths::DbLocation::Memory).expect("database"));
    let mut connection = pool.get().expect("connection");
    zuno_db::migration::apply(&mut connection).expect("schema");
    connection
        .execute_batch(
            "INSERT INTO project (id,worktree,time_created,time_updated,sandboxes)
             VALUES ('project','/workspace',1,1,'[]');
             INSERT INTO session
               (id,project_id,slug,directory,title,version,time_created,time_updated)
             VALUES ('ses_tui_deferred','project','tui','/workspace','TUI','test',1,1);",
        )
        .expect("session");
    drop(connection);
    let store = SessionExecutionStore::new(Arc::clone(&pool));
    let mut state = store
        .seed(
            SESSION,
            CollaborationMode::Work,
            Some(TurnExecutionIdentity::new("build", "test", "model")),
            2,
        )
        .expect("execution");
    state.cycle_id = Some("original-cycle".to_owned());
    state.phase = match readiness {
        SessionReadiness::Ready => zuno_types::execution::SessionExecutionPhase::Idle,
        SessionReadiness::WaitingHuman { .. } | SessionReadiness::WaitingExternal { .. } => {
            zuno_types::execution::SessionExecutionPhase::Waiting
        }
        SessionReadiness::Paused { .. } => zuno_types::execution::SessionExecutionPhase::Paused,
        SessionReadiness::Completed => zuno_types::execution::SessionExecutionPhase::Completed,
    };
    state.scheduling = Some(SessionScheduling {
        readiness,
        progress_fingerprint: Some("same-evidence".to_owned()),
        unchanged_progress_count: 3,
    });
    store.update(state.revision, state).expect("scheduling");
    (Arc::clone(&pool), SessionInbox::new(pool), store)
}

fn admit_callback(
    inbox: &SessionInbox,
    id: &str,
    cycle_id: Option<&str>,
) -> zuno_db::inbox::SessionInput {
    inbox
        .admit(
            NewSessionInput::new(
                id,
                SESSION,
                json!({
                    "kind": "backgroundExecutionReport",
                    "executionID": "bg_result",
                    "status": "completed",
                    "text": "the original command finished",
                }),
                InputDelivery::Queue,
                10,
            )
            .with_trigger_kind(InputTriggerKind::Automatic)
            .with_cycle_id(cycle_id),
        )
        .expect("callback evidence")
}

#[test]
fn rejected_callbacks_remain_queued_without_changing_pause_or_progress() {
    let (_pool, inbox, store) = database(SessionReadiness::Paused {
        reason: SessionPauseReason::NoProgress,
    });
    let before = store.get(SESSION).expect("state");
    let callback = admit_callback(&inbox, "callback", Some("original-cycle"));

    for _ in 0..3 {
        assert!(
            next_admissible_prompt(&inbox, SESSION)
                .expect("admission")
                .is_none()
        );
        assert_eq!(
            inbox.get(SESSION, "callback").expect("row"),
            Some(callback.clone())
        );
        assert_eq!(store.get(SESSION).expect("state"), before);
    }
}

#[test]
fn rejected_callback_does_not_hide_later_user_input_or_resume_the_session() {
    let (_pool, inbox, store) = database(SessionReadiness::Paused {
        reason: SessionPauseReason::User,
    });
    let before = store.get(SESSION).expect("state");
    let callback = admit_callback(&inbox, "callback", Some("original-cycle"));
    inbox
        .admit(NewSessionInput::new(
            "status-query",
            SESSION,
            json!({
                "kind": "tuiPrompt",
                "submission": {"kind": "text", "data": "what is paused?"},
                "origin": "tui_keybinding"
            }),
            InputDelivery::Queue,
            11,
        ))
        .expect("user query");
    let (input, submission) = next_admissible_prompt(&inbox, SESSION)
        .expect("admission")
        .expect("the user query can run");
    assert_eq!(input.id, "status-query");
    assert_eq!(
        submission,
        PromptSubmission::Text("what is paused?".to_owned())
    );
    assert_eq!(
        inbox.get(SESSION, "callback").expect("callback"),
        Some(callback)
    );
    assert_eq!(
        store.get(SESSION).expect("query preserves the pause"),
        before
    );
}

#[test]
fn legacy_and_other_cycle_callbacks_cannot_start_work_from_the_tui() {
    for origin in [None, Some("another-cycle")] {
        let (_pool, inbox, _store) = database(SessionReadiness::Ready);
        let callback = admit_callback(&inbox, "callback", origin);
        assert!(
            next_admissible_prompt(&inbox, SESSION)
                .expect("admission")
                .is_none()
        );
        assert_eq!(
            inbox.get(SESSION, "callback").expect("callback"),
            Some(callback)
        );
    }
    let (_pool, inbox, _store) = database(SessionReadiness::Ready);
    admit_callback(&inbox, "current", Some("original-cycle"));
    assert_eq!(
        next_admissible_prompt(&inbox, SESSION)
            .expect("admission")
            .expect("current cycle")
            .0
            .id,
        "current"
    );
}

#[test]
fn completed_host_control_is_not_promoted_repeatedly() {
    let (_pool, inbox, _store) = database(SessionReadiness::Paused {
        reason: SessionPauseReason::NoProgress,
    });
    inbox
        .admit(NewSessionInput::new(
            "resume-command",
            SESSION,
            serde_json::to_value(PersistedTuiInput::TuiPrompt {
                submission: PromptSubmission::Host(HostCommand::Resume(String::new())),
                origin: PromptOrigin::TuiKeybinding,
            })
            .expect("command"),
            InputDelivery::Queue,
            10,
        ))
        .expect("queue");
    inbox
        .promote_id(SESSION, "resume-command")
        .expect("promote");
    settle_host_input(&inbox, SESSION, "resume-command", &Ok(())).expect("consumed");
    assert_eq!(
        inbox
            .get(SESSION, "resume-command")
            .expect("row")
            .expect("retained")
            .state,
        SubmissionState::Consumed
    );
    assert!(
        next_admissible_prompt(&inbox, SESSION)
            .expect("admission")
            .is_none()
    );
}

#[test]
fn explicit_resume_reuses_the_exact_committed_control_without_a_plan_or_goal() {
    let (pool, inbox, store) = database(SessionReadiness::Paused {
        reason: SessionPauseReason::NoProgress,
    });
    let spill = tempfile::tempdir().expect("goal projection root");
    let _goals = zuno_goal::GoalStore::from_pool(Arc::clone(&pool), spill.path().to_owned())
        .expect("initialize Goal tables without creating a Goal");
    let service = zuno_session_control::SessionControlService::new(pool);
    let paused = store.get(SESSION).expect("state").expect("paused");
    let resumed = service
        .resume_session(SESSION, paused.revision, 20)
        .expect("explicit resume");
    let continuation = serde_json::from_value(resumed.input.prompt["continuation"].clone())
        .expect("stored continuation");
    let mut control = CommittedWorkControl {
        input: resumed.input.clone(),
        continuation,
    };
    assert!(control.continuation.plan_id.is_none());
    assert!(validate_committed_work(&control, &resumed.state).is_ok());
    assert_eq!(
        inbox.pending(SESSION).expect("one control").as_slice(),
        std::slice::from_ref(&resumed.input)
    );

    control.continuation.identity.agent = "plan".to_owned();
    assert!(validate_committed_work(&control, &resumed.state).is_err());
    control.continuation = resumed.state.continuation.clone().expect("original token");
    control.input.cycle_id = Some("different-cycle".to_owned());
    assert!(validate_committed_work(&control, &resumed.state).is_err());
    control.input = resumed.input;
    control.input.trigger_kind = InputTriggerKind::Automatic;
    assert!(validate_committed_work(&control, &resumed.state).is_err());
    assert_eq!(
        store.get(SESSION).expect("unchanged authority"),
        Some(resumed.state)
    );
    assert_eq!(
        inbox.pending(SESSION).expect("no replacement input").len(),
        1
    );
}

#[test]
fn native_questions_and_resume_do_not_mark_an_idle_screen_as_running() {
    for (text, command) in [
        ("/questions list", HostCommand::Questions("list".to_owned())),
        ("/resume", HostCommand::Resume(String::new())),
    ] {
        let (wake, _events) = zuno_tui::app::terminal_event_channel();
        let (sink, mut prompts) = mpsc::channel(2);
        let mut screen = SessionScreen::new(ViewContext::defaults(), wake).with_prompt_sink(sink);
        screen.submit_prompt(text);
        assert_eq!(
            prompts.try_recv().expect("native command").prompt.payload,
            PromptSubmission::Host(command)
        );
        assert!(!screen.status_mut().is_running());
        assert!(!screen.transcript_mut().transcript().is_running());
    }
}

#[tokio::test]
async fn questions_router_works_while_turns_are_busy_and_preserves_other_envelopes() {
    let (wake, _wakes) = zuno_tui::app::terminal_event_channel();
    let questions = Arc::new(QuestionBroker::new(wake.clone()));
    let (source, incoming) = mpsc::channel(4);
    let (outgoing, mut forwarded) = mpsc::channel(4);
    let (events, _received) = event_channel();
    let (shutdown, stopping) = watch::channel(false);
    let worker = tokio::spawn(route_question_commands(
        incoming,
        outgoing,
        questions,
        QueuedInputProjection::new(Vec::new()),
        wake,
        events,
        stopping,
    ));
    source
        .send(TargetedPromptSubmission::root_with(PromptEnvelope::new(
            PromptSubmission::Host(HostCommand::Questions("list".to_owned())),
            PromptDelivery::Queue,
            PromptOrigin::TuiKeybinding,
        )))
        .await
        .expect("questions while running");
    let next = TargetedPromptSubmission::session_with(
        "child",
        PromptEnvelope::new(
            PromptSubmission::Text("retain this child input".to_owned()),
            PromptDelivery::Steer,
            PromptOrigin::TuiChild,
        )
        .with_expected_turn(Some("child-turn".to_owned()))
        .with_request_id("submission-identity".to_owned()),
    );
    source.send(next.clone()).await.expect("child input");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), forwarded.recv())
            .await
            .expect("responsive routing")
            .expect("forwarded"),
        next
    );
    assert!(
        forwarded.try_recv().is_err(),
        "/questions must not become model input"
    );
    shutdown.send(true).expect("shutdown");
    worker.await.expect("router stopped");
}
