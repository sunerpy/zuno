use super::*;

mod goal_resume;

/// Every model response remains gated, including a request restarted by steering.
struct AcceptedTurnResponder {
    turns: Arc<AtomicUsize>,
    gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

struct TurnRelease(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);

impl TurnRelease {
    fn open(&self) {
        let (open, changed) = self.0.as_ref();
        *open.lock().expect("lock fixture response gate") = true;
        changed.notify_all();
    }
}

impl Drop for TurnRelease {
    fn drop(&mut self) {
        self.open();
    }
}

impl AcceptedTurnResponder {
    fn new(turns: Arc<AtomicUsize>) -> (Self, TurnRelease) {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        (
            Self {
                turns,
                gate: Arc::clone(&gate),
            },
            TurnRelease(gate),
        )
    }
}

impl Respond for AcceptedTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        if !body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
        {
            return compatible_text_response("ACP accepted-prompt title");
        }
        self.turns.fetch_add(1, Ordering::SeqCst);
        let (open, changed) = self.gate.as_ref();
        let (open, _timeout) = changed
            .wait_timeout_while(
                open.lock().expect("lock fixture response gate"),
                Duration::from_secs(60),
                |open| !*open,
            )
            .expect("wait for fixture response gate");
        assert!(
            *open,
            "accepted-prompt fixture response gate was not released"
        );
        compatible_text_response("ACP reply")
    }
}

/// Read concurrently so a gated provider cannot hide an early prompt response.
struct PromptClient {
    root: Arc<tempfile::TempDir>,
    child: std::process::Child,
    stdin: Option<ChildStdin>,
    frames: mpsc::Receiver<Value>,
    reader: Option<std::thread::JoinHandle<()>>,
    session_id: String,
}

impl PromptClient {
    fn start(config: &str) -> Self {
        Self::connect(
            config,
            Arc::new(tempfile::tempdir().expect("ACP accepted-prompt root")),
            None,
        )
    }

    fn connect(config: &str, root: Arc<tempfile::TempDir>, existing_session: Option<&str>) -> Self {
        let mut child = isolated_command_with_config(root.path(), config)
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(acp_stderr())
            .spawn()
            .expect("start accepted-prompt ACP process");
        let mut stdin = child.stdin.take().expect("ACP stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));
        request(
            &mut stdin,
            &mut stdout,
            1,
            "initialize",
            json!({"protocolVersion": 1}),
        );
        let mut params = json!({"cwd": root.path(), "mcpServers": []});
        if let Some(session_id) = existing_session {
            params["sessionId"] = json!(session_id);
        }
        let created = request(
            &mut stdin,
            &mut stdout,
            2,
            if existing_session.is_some() {
                "session/load"
            } else {
                "session/new"
            },
            params,
        );
        let session_id = existing_session
            .or_else(|| created["sessionId"].as_str())
            .expect("session id")
            .to_owned();
        let (sender, frames) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in stdout.lines() {
                let line = line.expect("read ACP frame");
                let frame = serde_json::from_str(&line).expect("parse ACP frame");
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            root,
            child,
            stdin: Some(stdin),
            frames,
            reader: Some(reader),
            session_id,
        }
    }

    fn prompt(&mut self, id: u64, text: &str) {
        self.prompt_with_meta(id, text, json!({}));
    }

    fn prompt_with_meta(&mut self, id: u64, text: &str, meta: Value) {
        send_request(
            self.stdin.as_mut().expect("open ACP stdin"),
            id,
            "session/prompt",
            json!({
                "sessionId": &self.session_id,
                "prompt": [{"type": "text", "text": text}],
                "_meta": meta,
            }),
        );
    }

    fn request(&mut self, id: u64, method: &str, params: Value) {
        send_request(
            self.stdin.as_mut().expect("open ACP stdin"),
            id,
            method,
            params,
        );
    }

    fn withdraw(&mut self, id: u64) {
        let stdin = self.stdin.as_mut().expect("open ACP stdin");
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc": "2.0",
                "method": "$/cancel_request",
                "params": {"requestId": id},
            })
        )
        .expect("withdraw accepted prompt");
        stdin.flush().expect("flush prompt withdrawal");
    }

    async fn disconnect(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .expect("poll disconnected ACP process")
            {
                assert!(
                    status.success(),
                    "disconnected ACP process failed: {status}"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "disconnect did not settle the native driver"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn admitted_count(&self, text: &str) -> i64 {
        let connection =
            zuno_db::open::open(&acp_database(self.root.path())).expect("open fixture database");
        connection
            .query_row(
                "SELECT COUNT(*) FROM session_input \
                 WHERE session_id = ?1 AND json_extract(prompt, '$.text') = ?2",
                [&self.session_id, text],
                |row| row.get(0),
            )
            .expect("count fixture prompt admissions")
    }

    async fn admitted_prompt(&self, text: &str) -> zuno_db::inbox::SessionInput {
        admitted_prompt(self.root.path(), &self.session_id, text).await
    }

    fn assert_no_response(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.frames.recv_timeout(remaining) {
                Ok(frame) => assert!(
                    frame.get("id").is_none(),
                    "accepted prompt responded before its processing completed: {frame}"
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => return,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("ACP disconnected while accepted prompts were unfinished")
                }
            }
        }
    }

    fn responses(&self, ids: &[u64]) -> Vec<Value> {
        let mut responses = vec![Value::Null; ids.len()];
        let deadline = Instant::now() + Duration::from_secs(15);
        while responses.iter().any(Value::is_null) {
            let frame = self
                .frames
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!(
                        "accepted prompts {ids:?} did not settle ({error}); durable receipts: {}",
                        self.receipt_snapshot()
                    )
                });
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                let index = ids
                    .iter()
                    .position(|expected| *expected == id)
                    .unwrap_or_else(|| panic!("unexpected ACP response: {frame}"));
                assert!(
                    responses[index].is_null(),
                    "duplicate ACP response: {frame}"
                );
                responses[index] = frame;
            } else {
                assert_eq!(frame["method"], "session/update");
            }
        }
        responses
    }

    fn receipt_snapshot(&self) -> String {
        let snapshot = || -> rusqlite::Result<String> {
            let connection = rusqlite::Connection::open_with_flags(
                self.root.path().join("zuno-acp.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            connection.busy_timeout(Duration::from_millis(50))?;
            let mut statement = connection.prepare(
                "SELECT json_object(
                    'inputId',i.id,'inputState',i.state,'receiptState',r.state,
                    'turnId',r.turn_id,'stopReason',r.stop_reason,'error',r.error)
                 FROM session_input i
                 LEFT JOIN session_input_receipt r ON r.input_id=i.id
                 WHERE i.session_id=?1 ORDER BY i.admitted_seq",
            )?;
            let rows = statement
                .query_map([&self.session_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(format!("[{}]", rows.join(",")))
        };
        snapshot().unwrap_or_else(|error| format!("snapshot unavailable: {error}"))
    }
}

impl Drop for PromptClient {
    fn drop(&mut self) {
        drop(self.stdin.take());
        // Each fixture owns this process. Reap it even when a regression panics.
        let _killed = self.child.kill();
        let _waited = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _joined = reader.join();
        }
    }
}

pub(super) async fn admitted_prompt(
    root: &std::path::Path,
    session_id: &str,
    text: &str,
) -> zuno_db::inbox::SessionInput {
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(root)).expect("open fixture inbox"));
    let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&pool));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let input_id = pool
            .get()
            .expect("open fixture input connection")
            .query_row(
                "SELECT id FROM session_input \
                 WHERE session_id = ?1 AND json_extract(prompt, '$.text') = ?2 \
                 ORDER BY admitted_seq DESC LIMIT 1",
                [session_id, text],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .expect("find accepted fixture input");
        if let Some(input_id) = input_id {
            return inbox
                .get(session_id, &input_id)
                .expect("read accepted fixture input")
                .expect("accepted fixture input exists");
        }
        assert!(
            Instant::now() < deadline,
            "prompt was not admitted while the existing turn was active"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_accepted_concurrent_prompt_waits_for_its_processing_result() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    client.prompt(3, "Start the accepted-prompt regression turn.");
    await_turn_requests(&turns, 1).await;
    client.prompt(4, "Also verify the accepted concurrent input.");
    let accepted = client
        .admitted_prompt("Also verify the accepted concurrent input.")
        .await;
    assert_eq!(accepted.delivery, zuno_db::inbox::InputDelivery::Steer);
    client.assert_no_response(Duration::from_millis(150));

    release.open();
    for response in client.responses(&[3, 4]) {
        assert!(
            response.get("error").is_none(),
            "durable acceptance was returned as an error: {response}"
        );
        assert_eq!(response["result"]["stopReason"], "end_turn");
    }
    let settled = durable_input(client.root.path(), &client.session_id, &accepted.id);
    assert_eq!(settled.state, zuno_db::inbox::SubmissionState::Consumed);
    assert!(
        provider
            .received_requests()
            .await
            .expect("fixture provider requests")
            .iter()
            .any(|request| {
                String::from_utf8_lossy(&request.body)
                    .contains("Also verify the accepted concurrent input.")
            }),
        "a successful accepted prompt never reached the model"
    );
}

struct GatedGoalResponder {
    gate: AcceptedTurnResponder,
    goal: GoalCompletionTurnResponder,
}

impl Respond for GatedGoalResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let _gated = self.gate.respond(request);
        self.goal.respond(request)
    }
}

fn start_autonomous_goal(client: &mut PromptClient) {
    materialize_acp_fixture_session(client.root.path(), &client.session_id, "test-model", None);
    client.request(3, "session/close", json!({"sessionId": client.session_id}));
    assert!(client.responses(&[3])[0].get("error").is_none());

    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(client.root.path())).expect("open Goal fixture store"),
    );
    zuno_goal::GoalStore::from_pool(
        Arc::clone(&pool),
        client.root.path().join("goal-objective-spill"),
    )
    .expect("open fixture Goal store")
    .create_goal(
        &client.session_id,
        "Complete the autonomous accepted-prompt regression.",
        None,
    )
    .expect("seed autonomous Goal");
    seed_assistant_only_compaction_tail(
        &pool.get().expect("open fixture history"),
        &client.session_id,
    );
    client.request(
        4,
        "session/load",
        json!({
            "sessionId": client.session_id,
            "cwd": client.root.path(),
            "mcpServers": [],
        }),
    );
    assert!(client.responses(&[4])[0].get("error").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_accepted_prompt_waits_for_an_autonomous_goal_turn() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (gate, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(GatedGoalResponder {
            gate,
            goal: GoalCompletionTurnResponder::default(),
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&danger_full_access_config(&provider.uri()));
    start_autonomous_goal(&mut client);
    await_turn_requests(&turns, 1).await;
    client.prompt(5, "Apply this input to the autonomous Goal turn.");
    let accepted = client
        .admitted_prompt("Apply this input to the autonomous Goal turn.")
        .await;
    client.assert_no_response(Duration::from_millis(150));
    release.open();

    let response = client.responses(&[5]).remove(0);
    assert!(
        response.get("error").is_none(),
        "autonomous turn acceptance was returned as an error: {response}"
    );
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let settled = durable_input(client.root.path(), &client.session_id, &accepted.id);
    assert_eq!(settled.state, zuno_db::inbox::SubmissionState::Consumed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_live_native_turn_remains_observable_after_its_original_prompt_waiter_is_withdrawn() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (gate, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(GatedGoalResponder {
            gate,
            goal: GoalCompletionTurnResponder::default(),
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&danger_full_access_config(&provider.uri()));
    start_autonomous_goal(&mut client);
    await_turn_requests(&turns, 1).await;
    let text = "Observe this applied input through the live native Goal turn.";
    let meta = json!({"zuno": {"messageId": "live-native-observer"}});
    client.prompt_with_meta(5, text, meta.clone());
    let input = client.admitted_prompt(text).await;
    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(client.root.path())).expect("live observer pool"),
    );
    let receipts = zuno_db::input_receipt::InputReceiptStore::new(pool);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if receipts
            .get(&client.session_id, &input.id)
            .expect("live input receipt")
            .is_some_and(|receipt| {
                receipt.state == zuno_types::admission::InputReceiptState::Applied
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "steered input did not reach provider application"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.withdraw(5);
    assert_eq!(client.responses(&[5])[0]["error"]["code"], -32800);
    client.prompt_with_meta(6, text, meta);
    client.request(7, "session/list", json!({}));
    assert!(client.responses(&[7])[0].get("error").is_none());
    client.assert_no_response(Duration::from_millis(150));
    release.open();
    let observed = client.responses(&[6]).remove(0);
    assert_eq!(observed["result"]["stopReason"], "end_turn", "{observed}");
    assert_eq!(
        observed["result"]["_meta"]["zuno"]["receipt"]["inputId"],
        input.id
    );
    assert_eq!(client.admitted_count(text), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_message_id_retries_share_one_input_and_conflicting_payloads_are_rejected() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    let text = "Retry this exact client message without accepting it twice.";
    let meta = json!({"zuno": {"messageId": "client-accepted-1"}});
    client.prompt_with_meta(3, text, meta.clone());
    await_turn_requests(&turns, 1).await;
    client.prompt_with_meta(4, text, meta.clone());
    // A read-only request is a dispatch barrier while both prompts remain open.
    client.request(5, "session/list", json!({}));
    assert!(client.responses(&[5])[0].get("error").is_none());
    assert_eq!(client.admitted_count(text), 1);
    client.assert_no_response(Duration::from_millis(100));

    let conflict = "A different payload must not reuse that client message id.";
    client.prompt_with_meta(6, conflict, meta.clone());
    let rejected = client.responses(&[6]).remove(0);
    assert!(
        rejected.get("error").is_some(),
        "a conflicting client message id was accepted: {rejected}"
    );
    assert_eq!(client.admitted_count(text), 1);
    assert_eq!(client.admitted_count(conflict), 0);

    release.open();
    for response in client.responses(&[3, 4]) {
        assert!(
            response.get("error").is_none(),
            "a retry of an accepted input failed: {response}"
        );
        assert_eq!(response["result"]["stopReason"], "end_turn");
    }
    let completed_turn_requests = turns.load(Ordering::SeqCst);
    client.prompt_with_meta(7, text, meta);
    let retried = client.responses(&[7]).remove(0);
    assert_eq!(retried["result"]["stopReason"], "end_turn");
    assert_eq!(client.admitted_count(text), 1);
    assert_eq!(
        turns.load(Ordering::SeqCst),
        completed_turn_requests,
        "retrying a completed receipt started another model request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_identical_prompt_text_without_message_id_is_admitted_separately() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    let text = "Intentional repeated text is a new submission.";
    client.prompt(3, text);
    await_turn_requests(&turns, 1).await;
    client.prompt(4, text);
    client.request(5, "session/list", json!({}));
    assert!(client.responses(&[5])[0].get("error").is_none());
    assert_eq!(client.admitted_count(text), 2);
    client.assert_no_response(Duration::from_millis(100));
    release.open();
    for response in client.responses(&[3, 4]) {
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_withdrawal_racing_promotion_preserves_the_owner_and_durable_outcome() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    client.prompt(3, "Keep the owning turn running during withdrawal.");
    await_turn_requests(&turns, 1).await;
    let text = "Withdraw this accepted input before it reaches the model.";
    client.prompt(4, text);
    let accepted = client.admitted_prompt(text).await;
    client.withdraw(4);
    let withdrawn = client.responses(&[4]).remove(0);
    assert_eq!(withdrawn["error"]["code"], -32800, "{withdrawn}");
    release.open();
    let owner = client.responses(&[3]).remove(0);
    assert_eq!(owner["result"]["stopReason"], "end_turn", "{owner}");
    let settled = durable_input(client.root.path(), &client.session_id, &accepted.id);
    let reached_model = provider
        .received_requests()
        .await
        .expect("fixture provider requests")
        .iter()
        .any(|request| String::from_utf8_lossy(&request.body).contains(text));
    match settled.state {
        zuno_db::inbox::SubmissionState::Cancelled => {
            assert!(
                !reached_model,
                "a cancelled durable input reached the model"
            );
        }
        zuno_db::inbox::SubmissionState::Consumed => {
            assert!(
                reached_model,
                "an input that won promotion lost its model-visible history"
            );
        }
        state => panic!("withdrawal left a nonterminal input behind: {state:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_withdrawing_a_message_id_retry_does_not_withdraw_the_original_input() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    let text = "The original accepted input owns its work.";
    let meta = json!({"zuno": {"messageId": "client-retried-withdrawal"}});
    client.prompt_with_meta(3, text, meta.clone());
    await_turn_requests(&turns, 1).await;
    client.prompt_with_meta(4, text, meta);
    client.request(5, "session/list", json!({}));
    assert!(client.responses(&[5])[0].get("error").is_none());
    assert_eq!(client.admitted_count(text), 1);
    client.withdraw(4);
    let withdrawn = client.responses(&[4]).remove(0);
    assert_eq!(withdrawn["error"]["code"], -32800, "{withdrawn}");
    release.open();
    let owner = client.responses(&[3]).remove(0);
    assert_eq!(owner["result"]["stopReason"], "end_turn", "{owner}");
    assert_eq!(client.admitted_count(text), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_cancelled_processing_remains_cancelled_when_its_message_id_is_retried() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    let text = "Cancel this native turn and retain its accepted receipt.";
    let meta = json!({"zuno": {"messageId": "cancelled-native-turn"}});
    client.prompt_with_meta(3, text, meta.clone());
    await_turn_requests(&turns, 1).await;
    client.withdraw(3);
    let cancelled = client.responses(&[3]).remove(0);
    assert_eq!(cancelled["error"]["code"], -32800, "{cancelled}");
    release.open();
    client.prompt_with_meta(4, text, meta);
    let retried = client.responses(&[4]).remove(0);
    assert_eq!(retried["result"]["stopReason"], "cancelled", "{retried}");
    assert_eq!(
        retried["result"]["_meta"]["zuno"]["receipt"]["state"],
        "cancelled"
    );
    assert_eq!(client.admitted_count(text), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_disconnect_preserves_admission_and_reconnect_does_not_repeat_applied_input() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let config = config_with_second_model(&provider.uri());
    let mut first = PromptClient::start(&config);
    let text = "Preserve this accepted input when its client disconnects.";
    let meta = json!({"zuno": {"messageId": "disconnect-retry"}});
    first.prompt_with_meta(3, text, meta.clone());
    await_turn_requests(&turns, 1).await;
    let input = first.admitted_prompt(text).await;
    first.disconnect().await;
    release.open();
    let recorded = durable_input(first.root.path(), &first.session_id, &input.id);
    assert_eq!(
        recorded.state,
        zuno_db::inbox::SubmissionState::Consumed,
        "disconnect was treated as withdrawal of the durable input"
    );
    let mut reconnected =
        PromptClient::connect(&config, Arc::clone(&first.root), Some(&first.session_id));
    reconnected.prompt_with_meta(3, text, meta);
    let response = reconnected.responses(&[3]).remove(0);
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert_eq!(
        response["result"]["_meta"]["zuno"]["receipt"]["inputId"],
        input.id
    );
    assert_eq!(reconnected.admitted_count(text), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_cold_applied_receipt_reports_observation_unavailable_without_replay_or_mutation() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let config = config_with_second_model(&provider.uri());
    let mut first = PromptClient::start(&config);
    let text = "Keep one admission across a host crash without replaying uncertain work.";
    let meta = json!({"zuno": {"messageId": "crashed-owner-retry"}});
    first.prompt_with_meta(3, text, meta.clone());
    await_turn_requests(&turns, 1).await;
    let input = first.admitted_prompt(text).await;
    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(first.root.path())).expect("crash fixture pool"),
    );
    let receipts = zuno_db::input_receipt::InputReceiptStore::new(pool);
    let applied = receipts
        .get(&first.session_id, &input.id)
        .expect("applied receipt")
        .expect("receipt exists");
    assert_eq!(
        applied.state,
        zuno_types::admission::InputReceiptState::Applied
    );
    assert!(applied.turn_id.is_some());

    // This is a confirmed process death, not EOF or an observer withdrawal.
    first.child.kill().expect("crash this fixture's ACP host");
    first.child.wait().expect("confirm the old host exited");
    drop(first.stdin.take());
    release.open();

    let mut reconnected =
        PromptClient::connect(&config, Arc::clone(&first.root), Some(&first.session_id));
    reconnected.prompt_with_meta(3, text, meta);
    let response = reconnected.responses(&[3]).remove(0);
    assert_eq!(response["error"]["code"], -32004, "{response}");
    assert_eq!(
        response["error"]["data"]["reason"],
        "executionObservationUnavailable"
    );
    assert_eq!(response["error"]["data"]["admission"], "accepted");
    assert_eq!(response["error"]["data"]["recoveryRequired"], true);
    let receipt = &response["error"]["data"]["receipt"];
    assert_eq!(receipt["inputId"], input.id, "{response}");
    assert_eq!(receipt["state"], "applied", "{response}");
    assert_eq!(
        receipts
            .get(&first.session_id, &input.id)
            .expect("unchanged receipt"),
        Some(applied),
        "observer unavailability changed the durable execution outcome"
    );
    assert_eq!(reconnected.admitted_count(text), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 1);
    assert_eq!(
        durable_input(first.root.path(), &first.session_id, &input.id).state,
        zuno_db::inbox::SubmissionState::Consumed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_unattached_applied_fixture_returns_accepted_observation_error_without_model_work() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    materialize_acp_fixture_session(client.root.path(), &client.session_id, "test-model", None);
    client.request(3, "session/close", json!({"sessionId": client.session_id}));
    assert!(client.responses(&[3])[0].get("error").is_none());

    // A pre-existing durable snapshot whose owner is unavailable to this adapter.
    // This fixture makes no assertion that the execution died or completed.
    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(client.root.path())).expect("unattached fixture pool"),
    );
    let inbox = zuno_db::inbox::SessionInbox::new(Arc::clone(&pool));
    let receipts = zuno_db::input_receipt::InputReceiptStore::new(Arc::clone(&pool));
    let input_id = "msg_unattached_applied_fixture";
    let text = "Existing applied input must keep its unknown execution outcome.";
    let now = zuno_db::message::now_millis();
    inbox
        .admit(
            zuno_db::inbox::NewSessionInput::new(
                input_id,
                &client.session_id,
                json!({
                    "kind":"acpPrompt", "text":text,
                    "content":[{"type":"text","text":text}],
                }),
                zuno_db::inbox::InputDelivery::Steer,
                now,
            )
            .with_source_key("client-message:unattached-applied-fixture"),
        )
        .expect("seed existing admission");
    inbox
        .promote_id(&client.session_id, input_id)
        .expect("seed prior promotion")
        .expect("promoted input");
    {
        let connection = pool.get().expect("fixture history");
        put_durable_message(
            &connection,
            &client.session_id,
            input_id,
            "user",
            now,
            json!({"agent":"orchestrator","model":{"providerID":"test","modelID":"test-model"}}),
        );
        put_durable_part(
            &connection,
            &client.session_id,
            input_id,
            "prt_unattached_applied_fixture",
            now,
            json!({"type":"text","text":text}),
        );
    }
    inbox
        .mark_consumed(&client.session_id, input_id)
        .expect("seed recorded input")
        .expect("consumed input");
    receipts
        .mark_applied(
            &client.session_id,
            &[input_id.to_owned()],
            "turn_unattached_fixture",
            now,
        )
        .expect("seed pre-existing applied receipt");
    let before = receipts
        .get(&client.session_id, input_id)
        .expect("fixture receipt")
        .expect("receipt exists");
    client.request(
        4,
        "session/load",
        json!({"sessionId":client.session_id,"cwd":client.root.path(),"mcpServers":[]}),
    );
    assert!(client.responses(&[4])[0].get("error").is_none());
    client.prompt_with_meta(
        5,
        text,
        json!({"zuno":{"messageId":"unattached-applied-fixture"}}),
    );
    let response = client.responses(&[5]).remove(0);
    assert_eq!(response["error"]["code"], -32004, "{response}");
    assert_eq!(response["error"]["data"]["admission"], "accepted");
    assert_eq!(
        response["error"]["data"]["reason"],
        "executionObservationUnavailable"
    );
    assert_eq!(
        response["error"]["data"]["receipt"],
        serde_json::to_value(&before).expect("original receipt")
    );
    assert_eq!(
        receipts
            .get(&client.session_id, input_id)
            .expect("unchanged receipt"),
        Some(before)
    );
    assert_eq!(client.admitted_count(text), 1);
    assert!(
        provider
            .received_requests()
            .await
            .expect("provider requests")
            .is_empty(),
        "an unavailable observation started new inference"
    );
}

struct TerminalResponder {
    gate: AcceptedTurnResponder,
    finish: Option<&'static str>,
}

impl Respond for TerminalResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let gated = self.gate.respond(request);
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        if !body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
        {
            return gated;
        }
        let Some(reason) = self.finish else {
            return ResponseTemplate::new(400).set_body_json(json!({
                "error": {
                    "message": "Fixture permanently rejected this inference.",
                    "type": "invalid_request_error",
                    "code": "invalid_prompt",
                }
            }));
        };
        let chunk = json!({
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "Fixture terminal reply"},
                "finish_reason": null,
            }]
        });
        let finish = json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11},
        });
        ResponseTemplate::new(200).set_body_raw(
            format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_accepted_prompts_return_the_actual_processing_stop_reason() {
    for (finish, expected) in [("length", "max_tokens"), ("content_filter", "refusal")] {
        let provider = MockServer::start().await;
        let turns = Arc::new(AtomicUsize::new(0));
        let (gate, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
        Mock::given(method("POST"))
            .respond_with(TerminalResponder {
                gate,
                finish: Some(finish),
            })
            .mount(&provider)
            .await;
        let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
        client.prompt(3, "Report the provider's actual terminal reason.");
        await_turn_requests(&turns, 1).await;
        client.prompt(4, "Apply the same terminal reason to this accepted input.");
        client
            .admitted_prompt("Apply the same terminal reason to this accepted input.")
            .await;
        client.assert_no_response(Duration::from_millis(100));
        release.open();
        for response in client.responses(&[3, 4]) {
            assert_eq!(response["result"]["stopReason"], expected, "{response}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_processing_failure_is_reported_with_durable_acceptance() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (gate, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(TerminalResponder { gate, finish: None })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    client.prompt(3, "Attempt the permanently failing inference.");
    await_turn_requests(&turns, 1).await;
    client.prompt(
        4,
        "Keep this accepted input identifiable when inference fails.",
    );
    let accepted = client
        .admitted_prompt("Keep this accepted input identifiable when inference fails.")
        .await;
    client.assert_no_response(Duration::from_millis(100));
    release.open();
    let responses = client.responses(&[3, 4]);
    for response in &responses {
        assert!(response.get("error").is_some(), "{response}");
        assert_ne!(response["error"]["code"], -32001, "{response}");
    }
    assert_eq!(responses[1]["error"]["data"]["inputId"], accepted.id);
    assert_eq!(
        responses[1]["error"]["data"]["admittedSequence"],
        accepted.admitted_sequence
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_failure_before_provider_application_settles_the_accepted_receipt() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).expect("fixture config");
    config["provider"]["test"]["models"]["test-model"]["limit"] =
        json!({"context": 64, "output": 16});
    let mut client = PromptClient::start(&config.to_string());
    client.prompt_with_meta(
        3,
        "This accepted query cannot fit alongside the native tool catalogue.",
        json!({"zuno": {"messageId": "before-provider-failure"}}),
    );
    let response = client.responses(&[3]).remove(0);
    assert!(response.get("error").is_some(), "{response}");
    assert_ne!(response["error"]["code"], -32001);
    let receipt = &response["error"]["data"]["receipt"];
    assert_eq!(
        receipt["clientMessageId"], "before-provider-failure",
        "{response}"
    );
    assert_eq!(receipt["state"], "failed", "{response}");
    assert!(receipt["appliedAt"].is_null(), "{response}");
    assert!(
        provider
            .received_requests()
            .await
            .expect("fixture provider requests")
            .iter()
            .all(|request| {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("provider request JSON");
                !body["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty())
            }),
        "the rejected context budget still reached model inference"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_invalid_message_ids_are_rejected_before_durable_admission() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    for (id, message_id) in (3..).zip([
        json!(""),
        json!(" "),
        json!(42),
        json!({}),
        json!("x".repeat(257)),
    ]) {
        client.prompt_with_meta(
            id,
            "A malformed client message id must not be accepted.",
            json!({"zuno": {"messageId": message_id}}),
        );
        let rejected = client.responses(&[id]).remove(0);
        assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
        assert_eq!(
            durable_session_configuration(client.root.path(), &client.session_id),
            None,
            "invalid message id materialized the reserved session"
        );
    }
    assert!(
        provider
            .received_requests()
            .await
            .expect("fixture provider requests")
            .is_empty(),
        "invalid client identity reached the model"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_client_message_ids_are_scoped_to_the_session() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    let meta = json!({"zuno": {"messageId": "same-client-id"}});
    client.prompt_with_meta(3, "The first session accepts this client id.", meta.clone());
    let first = client.responses(&[3]).remove(0);
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    client.request(
        4,
        "session/new",
        json!({"cwd": client.root.path(), "mcpServers": []}),
    );
    let created = client.responses(&[4]).remove(0);
    client.session_id = created["result"]["sessionId"]
        .as_str()
        .expect("second session id")
        .to_owned();
    client.prompt_with_meta(5, "The second session accepts a different payload.", meta);
    let second = client.responses(&[5]).remove(0);
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");
    let first_receipt = &first["result"]["_meta"]["zuno"]["receipt"];
    let second_receipt = &second["result"]["_meta"]["zuno"]["receipt"];
    assert_eq!(first_receipt["clientMessageId"], "same-client-id");
    assert_eq!(second_receipt["clientMessageId"], "same-client-id");
    assert_ne!(first_receipt["inputId"], second_receipt["inputId"]);
    assert_ne!(first_receipt["sessionId"], second_receipt["sessionId"]);
}

struct QueuedTurnResponder {
    first: TerminalResponder,
    second: TerminalResponder,
}

impl Respond for QueuedTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if String::from_utf8_lossy(&request.body).contains("QUEUED-RECEIPT-INPUT") {
            self.second.respond(request)
        } else {
            self.first.respond(request)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_queued_receipt_waits_for_its_own_turn_and_does_not_resteer_on_retry() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (first_gate, first_release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    let (second_gate, second_release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(QueuedTurnResponder {
            first: TerminalResponder {
                gate: first_gate,
                finish: Some("length"),
            },
            second: TerminalResponder {
                gate: second_gate,
                finish: Some("stop"),
            },
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    client.prompt(3, "The first turn must report its own token limit.");
    await_turn_requests(&turns, 1).await;

    // Reconstruct an already accepted handoff with no process-local steer signal.
    // Its retry must observe that row instead of injecting it into the first turn.
    let text = "QUEUED-RECEIPT-INPUT belongs to the following native turn.";
    let pool = Arc::new(
        zuno_db::Pool::open(&acp_database(client.root.path())).expect("open queued fixture inbox"),
    );
    zuno_db::inbox::SessionInbox::new(pool)
        .admit(
            zuno_db::inbox::NewSessionInput::new(
                "msg_queued_receipt_fixture",
                &client.session_id,
                json!({
                    "kind": "acpPrompt",
                    "text": text,
                    "content": [{"type": "text", "text": text}],
                }),
                zuno_db::inbox::InputDelivery::Steer,
                zuno_db::message::now_millis(),
            )
            .with_source_key("client-message:queued-receipt-fixture"),
        )
        .expect("seed accepted FIFO handoff");
    client.prompt_with_meta(
        4,
        text,
        json!({"zuno": {"messageId": "queued-receipt-fixture"}}),
    );
    client.request(5, "session/list", json!({}));
    assert!(client.responses(&[5])[0].get("error").is_none());
    assert_eq!(client.admitted_count(text), 1);
    client.assert_no_response(Duration::from_millis(100));
    client.prompt_with_meta(
        6,
        text,
        json!({"zuno": {"messageId": "queued-receipt-fixture"}}),
    );
    client.request(7, "session/list", json!({}));
    assert!(client.responses(&[7])[0].get("error").is_none());
    client.withdraw(6);
    assert_eq!(client.responses(&[6])[0]["error"]["code"], -32800);
    assert_eq!(
        durable_input(
            client.root.path(),
            &client.session_id,
            "msg_queued_receipt_fixture",
        )
        .state,
        zuno_db::inbox::SubmissionState::Steering,
        "withdrawing an observer retired the original pending input"
    );

    first_release.open();
    let first = client.responses(&[3]).remove(0);
    assert_eq!(first["result"]["stopReason"], "max_tokens", "{first}");
    await_turn_requests(&turns, 2).await;
    client.assert_no_response(Duration::from_millis(100));
    second_release.open();
    let second = client.responses(&[4]).remove(0);
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");
    let first_receipt = &first["result"]["_meta"]["zuno"]["receipt"];
    let second_receipt = &second["result"]["_meta"]["zuno"]["receipt"];
    assert_ne!(first_receipt["turnId"], second_receipt["turnId"]);
    assert_eq!(second_receipt["inputId"], "msg_queued_receipt_fixture");
    assert_eq!(turns.load(Ordering::SeqCst), 2);
}
