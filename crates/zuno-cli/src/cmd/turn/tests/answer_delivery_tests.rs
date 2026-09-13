use super::*;

#[tokio::test]
async fn durable_answer_without_live_notification_precedes_first_resumed_request() {
    let (_dir, mut host, provider, _work) = mock_provider_host(
        "build",
        vec![
            final_response("Initial answer."),
            final_response("Applied the choice."),
        ],
    )
    .await;
    drive_user(&mut host, "Inspect this fixture.").await;
    // A separately reconstructed native service has no live run registry. Its
    // answer commits successfully while the original host is idle.
    let questions = QuestionService::new(Arc::clone(&host.database));
    let question = questions
        .open(QuestionSpec {
            origin: QuestionOrigin {
                session_id: host.session_id.clone(),
                message_id: None,
                call_id: None,
                turn_id: None,
                goal_id: None,
            },
            mode: QuestionMode::Deferred,
            purpose: QuestionPurpose::Clarification,
            questions: vec![
                QuestionPrompt::new("Which fixture?", "Fixture", Vec::new()).into_request(),
            ],
            expected_goal_revision: None,
            plan: None,
        })
        .await
        .unwrap()
        .question;
    let receipt = questions
        .apply(
            &host.session_id,
            &question.id,
            QuestionCommand {
                command_id: "answer-from-reconnected-client".to_owned(),
                expected_revision: question.revision,
                action: QuestionAction::Answer {
                    answers: [(
                        question.questions[0].id.clone(),
                        vec!["selected-fixture-42".to_owned()],
                    )]
                    .into(),
                },
            },
        )
        .await
        .unwrap();
    let answer_id = receipt.input_id.unwrap();
    assert_eq!(
        host.inbox
            .get(&host.session_id, &answer_id)
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Queued
    );
    let resumed = host
        .session_control
        .resume_session(
            &host.session_id,
            execution(&host).revision,
            zuno_db::message::now_millis(),
        )
        .unwrap();
    let control = host
        .inbox
        .promote_id(&host.session_id, &resumed.input.id)
        .unwrap()
        .unwrap();
    let guard = host.runs.begin_turn(host.session_id.clone()).unwrap();
    let (sender, receiver) = zuno_engine::r#loop::event_channel();
    let (result, _) = tokio::join!(
        host.drive_promoted_start_work_with_guard(
            &control.id,
            resumed.state.continuation.unwrap(),
            &guard,
            sender,
        ),
        collect_turn_events(receiver),
    );
    result.unwrap();
    let actual = {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "no empty resume request before the answer"
        );
        serde_json::to_string(&requests[1].messages).unwrap()
    };
    assert!(
        actual.contains("selected-fixture-42"),
        "the first resume request must carry the already accepted answer"
    );
    let applied = zuno_db::input_receipt::get_in(&host.connection, &host.session_id, &answer_id)
        .unwrap()
        .unwrap();
    assert!(
        applied.applied_at.is_some(),
        "recorded or answered is not applied"
    );
    let count: i64 = host
        .connection
        .query_row(
            "SELECT count(*) FROM message WHERE id=?1",
            [&answer_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    drop(guard);
    host.shutdown().await.unwrap();
}
