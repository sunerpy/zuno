use super::*;
use zuno_types::execution::SessionPauseReason;

async fn paused_client() -> (MockServer, PromptClient, Arc<AtomicUsize>, TurnRelease) {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    execution_gate::pause_session(&client, SessionPauseReason::User);
    client.prompt(3, "Keep this work saved until explicitly resumed.");
    let gated = client.responses(&[3]).remove(0);
    assert_eq!(gated["error"]["code"], -32005, "{gated}");
    (provider, client, turns, release)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_prompt_stays_pending_until_the_native_turn_finishes() {
    let (_provider, mut client, turns, release) = paused_client().await;
    client.prompt(4, "/resume");
    await_turn_requests(&turns, 1).await;
    // With the provider held, even a legal end_turn is an incorrect early
    // response: ACP clients use that response to leave their running state.
    client.assert_no_response(Duration::from_millis(200));
    release.open();
    let mut saw_text = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    let completed = loop {
        let frame = client
            .frames
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("resume completion");
        if frame.get("id") == Some(&json!(4)) {
            break frame;
        }
        if frame["method"] == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            && frame["params"]["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("ACP reply"))
        {
            saw_text = true;
        }
    };
    assert!(
        saw_text,
        "the resume response overtook its committed output"
    );
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    let receipt = &completed["result"]["_meta"]["zuno"]["receipt"];
    assert_eq!(receipt["state"], "completed");
    assert!(receipt["turnId"].is_string());
    assert!(receipt["inputId"].as_str().unwrap().starts_with("ctl_"));
    assert!(receipt["completedAt"].is_number());
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_prompt_remains_cancellable_by_session_stop() {
    let (_provider, mut client, turns, release) = paused_client().await;
    client.prompt(4, "/resume");
    await_turn_requests(&turns, 1).await;
    client.assert_no_response(Duration::from_millis(100));
    let stdin = client.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc":"2.0", "method":"session/cancel",
            "params":{"sessionId":client.session_id},
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    let stopped = client.responses(&[4]).remove(0);
    assert_eq!(stopped["result"]["stopReason"], "cancelled", "{stopped}");
    assert_eq!(
        stopped["result"]["_meta"]["zuno"]["receipt"]["state"],
        "cancelled"
    );
    release.open();
    client.prompt(5, "Answer a new independent request.");
    assert_eq!(
        client.responses(&[5])[0]["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(turns.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn withdrawing_resume_interrupts_only_its_live_control() {
    let (_provider, mut client, turns, release) = paused_client().await;
    client.prompt(4, "/resume");
    await_turn_requests(&turns, 1).await;
    client.assert_no_response(Duration::from_millis(100));
    client.withdraw(4);
    let withdrawn = client.responses(&[4]).remove(0);
    assert_eq!(withdrawn["error"]["code"], -32800, "{withdrawn}");
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(client.root.path())).unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state: Option<String> = pool.get().unwrap().query_row(
            "SELECT r.state FROM session_input_receipt r JOIN session_input i ON i.id=r.input_id
             WHERE i.session_id=?1 AND json_extract(i.prompt,'$.control')='resume_work'
             ORDER BY i.admitted_seq DESC LIMIT 1", [&client.session_id], |row| row.get(0),
        ).optional().unwrap();
        if state.as_deref() == Some("cancelled") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "withdrawn native control never stopped: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    release.open();
    client.prompt(
        5,
        "A later request must not inherit the withdrawn control's cancellation.",
    );
    assert_eq!(
        client.responses(&[5])[0]["result"]["stopReason"],
        "end_turn"
    );
    assert_eq!(turns.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_resume_withdrawal_does_not_cancel_the_other_live_turn() {
    let (_provider, mut client, turns, release) = paused_client().await;
    client.prompt(4, "/resume");
    await_turn_requests(&turns, 1).await;
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(client.root.path())).unwrap());
    let service = zuno_session_control::SessionControlService::new(Arc::clone(&pool));
    let current = service.state(&client.session_id).unwrap().unwrap();
    zuno_db::session_execution::SessionExecutionStore::new(Arc::clone(&pool))
        .set_scheduling(
            &client.session_id,
            current.revision,
            zuno_types::execution::SessionScheduling {
                readiness: zuno_types::execution::SessionReadiness::Paused {
                    reason: SessionPauseReason::User,
                },
                ..Default::default()
            },
            zuno_db::message::now_millis(),
        )
        .unwrap();
    client.prompt(5, "/resume");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let count: i64 = pool
            .get()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM session_input WHERE session_id=?1
             AND json_extract(prompt,'$.control')='resume_work'",
                [&client.session_id],
                |row| row.get(0),
            )
            .unwrap();
        if count == 2 {
            break;
        }
        assert!(Instant::now() < deadline, "queued resume was not admitted");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.assert_no_response(Duration::from_millis(100));
    client.withdraw(5);
    assert_eq!(client.responses(&[5])[0]["error"]["code"], -32800);
    client.assert_no_response(Duration::from_millis(100));
    release.open();
    let first = client.responses(&[4]).remove(0);
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    client.request(6, "session/list", json!({}));
    assert!(client.responses(&[6])[0].get("error").is_none());
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_startup_failure_finishes_the_request_with_a_failed_receipt() {
    let (_provider, mut client, turns, release) = paused_client().await;
    release.open();
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(client.root.path())).unwrap());
    // A persisted identity no longer matching the host is a real, pre-engine
    // failure. It must not leave the accepted resume RPC waiting forever.
    pool.get()
        .unwrap()
        .execute(
            "UPDATE session_execution_state
         SET work_identity=json_set(work_identity,'$.modelId','stale-model'),revision=revision+1
         WHERE session_id=?1",
            [&client.session_id],
        )
        .unwrap();
    client.prompt(4, "/resume");
    let failed = client.responses(&[4]).remove(0);
    assert!(failed.get("error").is_some(), "{failed}");
    assert_eq!(failed["error"]["data"]["admission"], "accepted");
    assert_eq!(failed["error"]["data"]["receipt"]["state"], "failed");
    assert!(failed["error"]["data"]["receipt"]["turnId"].is_null());
    assert_eq!(turns.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_shutdown_retains_the_cancelled_control_without_replay() {
    let (_provider, mut client, turns, release) = paused_client().await;
    client.prompt(4, "/resume");
    await_turn_requests(&turns, 1).await;
    client.assert_no_response(Duration::from_millis(100));
    let root = Arc::clone(&client.root);
    let session_id = client.session_id.clone();
    drop(client.stdin.take());
    tokio::time::sleep(Duration::from_millis(100)).await;
    release.open();
    client.disconnect().await;
    let pool = zuno_db::Pool::open(&acp_database(root.path())).unwrap();
    let receipt: String = pool
        .get()
        .unwrap()
        .query_row(
            "SELECT r.state FROM session_input_receipt r JOIN session_input i ON i.id=r.input_id
         WHERE i.session_id=?1 AND json_extract(i.prompt,'$.control')='resume_work'",
            [&session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        receipt, "cancelled",
        "runtime shutdown must retain its real cancellation"
    );
    assert_eq!(turns.load(Ordering::SeqCst), 1);
    drop(client);
    let mut client = PromptClient::connect(
        &config_with_second_model(&_provider.uri()),
        root,
        Some(&session_id),
    );
    client.request(3, "session/list", json!({}));
    assert!(client.responses(&[3])[0].get("error").is_none());
    assert_eq!(
        turns.load(Ordering::SeqCst),
        1,
        "load must not replay the cancelled control"
    );
}
