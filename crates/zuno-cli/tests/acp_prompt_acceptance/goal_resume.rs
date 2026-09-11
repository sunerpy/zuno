use super::*;
use std::sync::atomic::AtomicBool;
use zuno_types::execution::{
    CollaborationMode, ContinuationToken, SessionExecutionPhase, SessionPauseReason,
    SessionReadiness, SessionScheduling, TurnExecutionIdentity,
};

fn paused_goal(
    client: &PromptClient,
) -> (Arc<zuno_db::Pool>, zuno_goal::GoalStore, zuno_goal::Goal) {
    materialize_acp_fixture_session(client.root.path(), &client.session_id, "test-model", None);
    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(client.root.path())).expect("Goal resume fixture pool"),
    );
    let goals = zuno_goal::GoalStore::from_pool(
        Arc::clone(&pool),
        client.root.path().join("goal-resume-spill"),
    )
    .expect("Goal resume store");
    goals
        .create_goal(
            &client.session_id,
            "Resume CURRENT-IDENTITY only after an explicit choice.",
            None,
        )
        .expect("create fixture Goal");
    let goal = goals
        .pause_with_reason(
            &client.session_id,
            zuno_goal::GoalPauseReason::UserInterruption,
        )
        .expect("pause fixture Goal")
        .expect("paused Goal");
    let executions = zuno_db::session_execution::SessionExecutionStore::new(Arc::clone(&pool));
    let mut state = executions
        .seed(
            &client.session_id,
            CollaborationMode::Work,
            Some(TurnExecutionIdentity::new(
                "orchestrator",
                "test",
                "test-model",
            )),
            zuno_db::message::now_millis(),
        )
        .expect("seed execution state");
    state.phase = SessionExecutionPhase::Paused;
    state.cycle_id = Some("goal-resume-fixture-cycle".to_owned());
    state.scheduling = Some(SessionScheduling {
        readiness: SessionReadiness::Paused {
            reason: SessionPauseReason::User,
        },
        ..SessionScheduling::default()
    });
    executions
        .update(state.revision, state)
        .expect("pause execution state");
    (pool, goals, goal)
}

fn resume_question(client: &mut PromptClient, id: u64) -> Value {
    client.request(
        id,
        "questions/list",
        json!({"sessionId": client.session_id}),
    );
    let response = client.responses(&[id]).remove(0);
    let questions = response["result"]["questions"]
        .as_array()
        .unwrap_or_else(|| panic!("question listing failed: {response}"));
    let matching = questions
        .iter()
        .filter(|question| question["purpose"] == "goal_resume")
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "missing or duplicate Goal resume offer: {response}"
    );
    let question = matching[0].clone();
    let item = &question["questions"][0];
    assert_eq!(item["custom"], false);
    assert_eq!(
        item["options"]
            .as_array()
            .expect("closed resume choices")
            .iter()
            .map(|option| option["label"].clone())
            .collect::<Vec<_>>(),
        vec![json!("Resume goal"), json!("Keep paused")]
    );
    question
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_prompt_offers_resume_but_defer_cancel_and_keep_paused_never_resume() {
    for action in ["defer", "cancel", "keep"] {
        let provider = MockServer::start().await;
        let turns = Arc::new(AtomicUsize::new(0));
        let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
        release.open();
        Mock::given(method("POST"))
            .respond_with(responder)
            .mount(&provider)
            .await;
        let mut client = PromptClient::start(&danger_full_access_config(&provider.uri()));
        let (_pool, goals, paused) = paused_goal(&client);
        client.prompt(
            3,
            if action == "keep" {
                "Resume goal"
            } else {
                "Answer this ordinary query while the previous Goal stays paused."
            },
        );
        let prompted = client.responses(&[3]).remove(0);
        assert_eq!(prompted["result"]["stopReason"], "end_turn", "{prompted}");
        let question = resume_question(&mut client, 4);
        assert_eq!(question["origin"]["goalId"], paused.goal_id);
        assert_eq!(
            question["origin"]["messageId"],
            prompted["result"]["_meta"]["zuno"]["receipt"]["inputId"]
        );
        let item_id = question["questions"][0]["id"]
            .as_str()
            .expect("question item id");
        let selection = if action == "keep" {
            json!({"type":"answer","answers":{item_id:["Keep paused"]}})
        } else {
            json!({"type":action})
        };
        client.request(
            5,
            "questions/respond",
            json!({
                "sessionId": client.session_id,
                "requestId": question["id"],
                "command": {
                    "commandId": format!("goal-resume-{action}"),
                    "expectedRevision": question["revision"],
                    "action": selection,
                },
            }),
        );
        let answered = client.responses(&[5]).remove(0);
        assert!(answered.get("error").is_none(), "{answered}");
        assert!(answered["result"]["inputId"].is_null(), "{answered}");
        let current = goals
            .goal(&client.session_id)
            .expect("read paused Goal")
            .expect("Goal remains");
        assert_eq!(current.status, zuno_goal::GoalStatus::Paused);
        assert_eq!(current.revision, paused.revision);
        assert_eq!(turns.load(Ordering::SeqCst), 1);
    }
}

struct ResumeResponder {
    resumed: Arc<AtomicBool>,
    goal: GoalIdentityTurnResponder,
}

impl Respond for ResumeResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if self.resumed.load(Ordering::SeqCst) {
            self.goal.respond(request)
        } else {
            TextTurnResponder.respond(request)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_resume_after_prompt_completion_drives_the_committed_native_control_once() {
    let provider = MockServer::start().await;
    let resumed = Arc::new(AtomicBool::new(false));
    let goal_responder = GoalIdentityTurnResponder::default();
    Mock::given(method("POST"))
        .respond_with(ResumeResponder {
            resumed: Arc::clone(&resumed),
            goal: goal_responder.clone(),
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&danger_full_access_config(&provider.uri()));
    let (pool, goals, paused) = paused_goal(&client);
    client.prompt(
        3,
        "A completed query must not implicitly resume the paused Goal.",
    );
    let prompted = client.responses(&[3]).remove(0);
    assert_eq!(prompted["result"]["stopReason"], "end_turn", "{prompted}");
    let question = resume_question(&mut client, 4);
    let item_id = question["questions"][0]["id"]
        .as_str()
        .expect("question item id");
    let command = json!({
        "sessionId": client.session_id,
        "requestId": question["id"],
        "command": {
            "commandId": "explicit-goal-resume",
            "expectedRevision": question["revision"],
            "action": {"type":"answer","answers":{item_id:["Resume goal"]}},
        },
    });
    goal_responder.expect_revision(paused.revision + 1);
    resumed.store(true, Ordering::SeqCst);
    client.request(5, "questions/respond", command.clone());
    let response = client.responses(&[5]).remove(0);
    assert!(response.get("error").is_none(), "{response}");
    let control_id = response["result"]["inputId"]
        .as_str()
        .expect("resume admits one native control")
        .to_owned();
    let control = durable_input(client.root.path(), &client.session_id, &control_id);
    assert_eq!(control.prompt["kind"], "sessionControl");
    assert_eq!(control.prompt["control"], "resume_work");
    let receipts = zuno_db::input_receipt::InputReceiptStore::new(Arc::clone(&pool));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let goal = goals
            .goal(&client.session_id)
            .expect("Goal state")
            .expect("Goal");
        let receipt = receipts
            .get(&client.session_id, &control_id)
            .expect("control receipt");
        if goal.status == zuno_goal::GoalStatus::Complete
            && receipt.is_some_and(|receipt| receipt.state.is_terminal())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "committed resume control was not processed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let processed = goal_responder.goal_requests.load(Ordering::SeqCst);
    client.request(6, "questions/respond", command);
    let retry = client.responses(&[6]).remove(0);
    assert_eq!(retry["result"]["duplicate"], true, "{retry}");
    assert_eq!(retry["result"]["inputId"], control_id);
    assert_eq!(
        goal_responder.goal_requests.load(Ordering::SeqCst),
        processed
    );
    let connection = pool.get().expect("history connection");
    let history = zuno_db::message::MessageStore::new(&connection)
        .hydrate_session(&client.session_id)
        .expect("native resume history");
    assert_eq!(
        history
            .iter()
            .filter(|message| message.info.role == zuno_db::message::MessageRole::User)
            .count(),
        1,
        "native resumption manufactured a second user message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn goal_resume_question_after_reconfiguration_uses_the_current_trusted_identity() {
    let provider = MockServer::start().await;
    let resumed = Arc::new(AtomicBool::new(false));
    let goal_responder = GoalIdentityTurnResponder::default();
    Mock::given(method("POST"))
        .respond_with(ResumeResponder {
            resumed: Arc::clone(&resumed),
            goal: goal_responder.clone(),
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&danger_full_access_config(&provider.uri()));
    let (pool, goals, paused) = paused_goal(&client);
    client.prompt(3, "Offer a Goal choice without resuming the paused work.");
    let prompted = client.responses(&[3]).remove(0);
    assert_eq!(prompted["result"]["stopReason"], "end_turn", "{prompted}");
    let question = resume_question(&mut client, 4);

    for (id, config_id, value) in [(5, "agent", "deep"), (6, "model", "test/test-model-2")] {
        client.request(
            id,
            "session/set_config_option",
            json!({
                "sessionId": client.session_id,
                "configId": config_id,
                "value": value,
            }),
        );
        let configured = client.responses(&[id]).remove(0);
        assert!(configured.get("error").is_none(), "{configured}");
        assert!(
            configured["result"]["configOptions"]
                .as_array()
                .expect("configuration options")
                .iter()
                .any(|option| option["id"] == config_id && option["currentValue"] == value),
            "the trusted configuration did not adopt {config_id}={value}: {configured}"
        );
    }
    let current_question = resume_question(&mut client, 7);
    assert_eq!(current_question["id"], question["id"]);
    assert_eq!(current_question["revision"], question["revision"]);
    assert_eq!(
        goals
            .goal(&client.session_id)
            .expect("Goal state")
            .expect("Goal")
            .status,
        zuno_goal::GoalStatus::Paused
    );

    let item_id = current_question["questions"][0]["id"]
        .as_str()
        .expect("question item id");
    goal_responder.expect_revision(paused.revision + 1);
    resumed.store(true, Ordering::SeqCst);
    client.request(
        8,
        "questions/respond",
        json!({
            "sessionId": client.session_id,
            "requestId": current_question["id"],
            "command": {
                "commandId": "resume-after-trusted-reconfiguration",
                "expectedRevision": current_question["revision"],
                "action": {"type":"answer","answers":{item_id:["Resume goal"]}},
            },
        }),
    );
    let response = client.responses(&[8]).remove(0);
    assert!(response.get("error").is_none(), "{response}");
    let control_id = response["result"]["inputId"]
        .as_str()
        .expect("explicit resume admitted its native control");
    let control = durable_input(client.root.path(), &client.session_id, control_id);
    assert_eq!(control.prompt["kind"], "sessionControl");
    assert_eq!(control.prompt["control"], "resume_work");
    let continuation: ContinuationToken =
        serde_json::from_value(control.prompt["continuation"].clone())
            .expect("typed native Work continuation");
    let selected = TurnExecutionIdentity::new("deep", "test", "test-model-2");
    let receipts = zuno_db::input_receipt::InputReceiptStore::new(Arc::clone(&pool));
    let execution = zuno_db::session_execution::SessionExecutionStore::new(Arc::clone(&pool))
        .get(&client.session_id)
        .expect("saved execution state")
        .expect("execution exists");
    let admitted = receipts
        .get(&client.session_id, control_id)
        .expect("native control receipt")
        .expect("receipt exists");
    assert_eq!(
        continuation.identity, selected,
        "the answered native Goal choice retained an identity the current host cannot drive; \
         control_state={:?}, execution={execution:?}, receipt={admitted:?}, response={response}",
        control.state
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let completed = loop {
        let receipt = receipts
            .get(&client.session_id, control_id)
            .expect("native control receipt")
            .expect("receipt remains durable");
        if receipt.state.is_terminal() {
            assert_eq!(
                receipt.state,
                zuno_types::admission::InputReceiptState::Completed,
                "reconfigured native Goal resume did not complete: {receipt:?}"
            );
            break receipt;
        }
        assert!(
            Instant::now() < deadline,
            "reconfigured native Goal resume was never processed: {}",
            client.receipt_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        completed.stop_reason,
        Some(zuno_types::admission::InputStopReason::EndTurn)
    );
    assert_eq!(
        goals
            .goal(&client.session_id)
            .expect("Goal state")
            .expect("Goal")
            .status,
        zuno_goal::GoalStatus::Complete
    );
    assert_eq!(goal_responder.goal_requests.load(Ordering::SeqCst), 2);
    let connection = pool.get().expect("native resume history connection");
    let starts = zuno_db::event_log::read_of_type_after_in(
        &connection,
        &client.session_id,
        "session.turn.started",
        None,
    )
    .expect("native resume turn events");
    let starts = starts
        .iter()
        .filter(|event| event.properties["turnTrigger"] == "user_control")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1, "{starts:?}");
    let start = &starts[0].properties;
    assert_eq!(start["userControl"], "resume_work");
    assert_eq!(start["agent"], selected.agent);
    assert_eq!(start["providerID"], selected.provider_id);
    assert_eq!(start["modelID"], selected.model_id);
    assert_eq!(completed.turn_id.as_deref(), start["turnID"].as_str());
    let history = zuno_db::message::MessageStore::new(&connection)
        .hydrate_session(&client.session_id)
        .expect("native resume history");
    assert_eq!(
        history
            .iter()
            .filter(|message| message.info.role == zuno_db::message::MessageRole::User)
            .count(),
        1,
        "the reconfigured native resume must not manufacture another user message"
    );
}
