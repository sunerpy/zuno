use super::*;

fn cancel(client: &mut PromptClient, meta: Value) {
    let stdin = client.stdin.as_mut().expect("open ACP stdin");
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": client.session_id, "_meta": meta},
        })
    )
    .expect("write session cancellation");
    stdin.flush().expect("flush session cancellation");
}

fn live_turn_id(client: &PromptClient) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frame = client
            .frames
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("live turn metadata arrives");
        assert!(
            frame.get("id").is_none(),
            "unexpected prompt response: {frame}"
        );
        assert_eq!(frame["method"], "session/update");
        if let Some(id) = frame["params"]["update"]["_meta"]["zuno"]["turnId"].as_str() {
            return id.to_owned();
        }
    }
}

/// The original turn completed normally. Its delayed, explicitly identified
/// cancellation must not stop the independent turn already using the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_exact_cancel_for_t1_cannot_interrupt_t2() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    release.open();
    client.prompt(3, "Finish turn one before a later cancellation arrives.");
    let first = client.responses(&[3]).remove(0);
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let t1 = first["result"]["_meta"]["zuno"]["receipt"]["turnId"]
        .as_str()
        .expect("the completed receipt identifies T1");
    *release.0.0.lock().expect("close provider gate") = false;
    let before = turns.load(Ordering::SeqCst);
    client.prompt(
        4,
        "Turn two must survive a delayed cancellation of turn one.",
    );
    await_turn_requests(&turns, before + 1).await;
    cancel(&mut client, json!({"zuno": {"expectedTurnId": t1}}));
    // A transport barrier plus the gated provider makes accidental interruption
    // observable without racing a normal T2 completion.
    client.request(5, "session/list", json!({}));
    assert!(client.responses(&[5])[0].get("error").is_none());
    client.assert_no_response(Duration::from_millis(150));
    release.open();
    let second = client.responses(&[4]).remove(0);
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");
    assert_ne!(second["result"]["_meta"]["zuno"]["receipt"]["turnId"], t1);
    assert_eq!(turns.load(Ordering::SeqCst), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_exact_cancel_uses_published_turn_metadata_and_retains_the_receipt() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    let text = "Cancel exactly the turn exposed by protocol metadata.";
    let meta = json!({"zuno": {"messageId": "exact-cancellation"}});
    client.prompt_with_meta(3, text, meta.clone());
    await_turn_requests(&turns, 1).await;
    let target = live_turn_id(&client);
    // Malformed exact metadata must fail closed, never fall back to legacy.
    for invalid in [json!(null), json!(""), json!("   "), json!(42)] {
        cancel(&mut client, json!({"zuno": {"expectedTurnId": invalid}}));
    }
    client.request(4, "session/list", json!({}));
    assert!(client.responses(&[4])[0].get("error").is_none());
    client.assert_no_response(Duration::from_millis(100));
    cancel(&mut client, json!({"zuno": {"expectedTurnId": target}}));
    let stopped = client.responses(&[3]).remove(0);
    assert_eq!(stopped["result"]["stopReason"], "cancelled", "{stopped}");
    assert_eq!(
        stopped["result"]["_meta"]["zuno"]["receipt"]["turnId"],
        target
    );
    release.open();
    client.prompt_with_meta(5, text, meta);
    let retried = client.responses(&[5]).remove(0);
    assert_eq!(retried["result"], stopped["result"]);
    assert_eq!(client.admitted_count(text), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_idle_legacy_cancel_and_old_request_withdrawal_do_not_cancel_new_input() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    release.open();
    client.prompt(3, "Finish the original request.");
    assert_eq!(
        client.responses(&[3])[0]["result"]["stopReason"],
        "end_turn"
    );
    cancel(&mut client, json!({}));
    *release.0.0.lock().expect("close provider gate") = false;
    let before = turns.load(Ordering::SeqCst);
    client.prompt(4, "The next input is independent of the completed request.");
    await_turn_requests(&turns, before + 1).await;
    client.withdraw(3);
    client.request(5, "session/list", json!({}));
    assert!(client.responses(&[5])[0].get("error").is_none());
    client.assert_no_response(Duration::from_millis(100));
    release.open();
    let next = client.responses(&[4]).remove(0);
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    assert_eq!(turns.load(Ordering::SeqCst), before + 1);
}

/// Session-only cancellation carries no evidence of which earlier turn a client
/// had in mind. It targets the live turn at dispatch, even after a previous turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_legacy_cancel_honestly_targets_the_current_turn() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    release.open();
    client.prompt(3, "Finish the earlier turn.");
    assert_eq!(
        client.responses(&[3])[0]["result"]["stopReason"],
        "end_turn"
    );
    *release.0.0.lock().expect("close provider gate") = false;
    let before = turns.load(Ordering::SeqCst);
    client.prompt(4, "The legacy cancel can only identify this live turn.");
    await_turn_requests(&turns, before + 1).await;
    let current = live_turn_id(&client);
    cancel(&mut client, json!({}));
    let stopped = client.responses(&[4]).remove(0);
    assert_eq!(stopped["result"]["stopReason"], "cancelled", "{stopped}");
    assert_eq!(
        stopped["result"]["_meta"]["zuno"]["receipt"]["turnId"],
        current
    );
    release.open();
}
