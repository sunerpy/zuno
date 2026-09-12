use super::*;
use zuno_types::execution::{
    CollaborationMode, SessionPauseReason, SessionReadiness, SessionScheduling,
    TurnExecutionIdentity,
};

fn pause_session(client: &PromptClient, reason: SessionPauseReason) {
    materialize_acp_fixture_session(client.root.path(), &client.session_id, "test-model", None);
    let pool = Arc::new(zuno_db::Pool::open(&acp_database(client.root.path())).unwrap());
    let control = zuno_session_control::SessionControlService::new(Arc::clone(&pool));
    let now = zuno_db::message::now_millis();
    control
        .record_continuation(
            &client.session_id,
            "explicit-pause-fixture",
            TurnExecutionIdentity::new("orchestrator", "test", "test-model"),
            CollaborationMode::Work,
            None,
            None,
            None,
            now,
        )
        .unwrap();
    let store = zuno_db::session_execution::SessionExecutionStore::new(pool);
    let state = control.state(&client.session_id).unwrap().unwrap();
    store
        .set_scheduling(
            &client.session_id,
            state.revision,
            SessionScheduling {
                readiness: SessionReadiness::Paused { reason },
                ..Default::default()
            },
            now + 1,
        )
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_cancel_then_new_prompt_runs_without_a_resume_command() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    client.prompt(3, "Stop this ordinary task.");
    await_turn_requests(&turns, 1).await;
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
    let stopped = client.responses(&[3]).remove(0);
    assert_eq!(stopped["result"]["stopReason"], "cancelled", "{stopped}");
    release.open();
    client.prompt(
        4,
        "Answer this independent new request without resuming old work.",
    );
    let next = client.responses(&[4]).remove(0);
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");
    assert_ne!(
        next["result"]["_meta"]["zuno"]["receipt"]["turnId"],
        stopped["result"]["_meta"]["zuno"]["receipt"]["turnId"],
    );
    assert_eq!(turns.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gated_input_survives_reconnect_and_explicit_resume_applies_it_once() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    release.open();
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let config = config_with_second_model(&provider.uri());
    let mut client = PromptClient::start(&config);
    pause_session(&client, SessionPauseReason::User);
    let text = "Apply the retained choice: option A.";
    let meta = json!({"zuno":{"messageId":"retained-choice"}});
    client.prompt_with_meta(3, text, meta.clone());
    let deferred = client.responses(&[3]).remove(0);
    assert_eq!(deferred["error"]["code"], -32005, "{deferred}");
    let data = &deferred["error"]["data"];
    assert_eq!(data["admission"], "accepted");
    assert_eq!(data["reason"], "executionGated");
    assert_eq!(data["receipt"]["state"], "recorded");
    assert_eq!(data["receipt"]["executionGate"]["reason"], "user");
    assert_eq!(data["receipt"]["executionGate"]["recovery"], "resume_work");
    assert!(data["receipt"]["appliedAt"].is_null());
    assert!(data["receipt"]["completedAt"].is_null());
    assert_eq!(turns.load(Ordering::SeqCst), 0);
    assert_eq!(client.admitted_count(text), 1);
    let input_id = data["inputId"].clone();
    let root = Arc::clone(&client.root);
    let session_id = client.session_id.clone();
    client.disconnect().await;
    drop(client);
    let mut client = PromptClient::connect(&config, root, Some(&session_id));
    client.prompt_with_meta(3, text, meta.clone());
    let repeated = client.responses(&[3]).remove(0);
    assert_eq!(repeated["error"]["data"], *data, "{repeated}");
    assert_eq!(turns.load(Ordering::SeqCst), 0);
    client.prompt(4, "/resume");
    let resumed = client.responses(&[4]).remove(0);
    assert!(resumed.get("error").is_none(), "{resumed}");
    await_turn_requests(&turns, 1).await;
    client.prompt_with_meta(5, text, meta);
    let completed = client.responses(&[5]).remove(0);
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    let receipt = &completed["result"]["_meta"]["zuno"]["receipt"];
    assert_eq!(receipt["inputId"], input_id);
    assert_eq!(receipt["state"], "completed");
    assert!(receipt["executionGate"].is_null());
    assert!(receipt["appliedAt"].is_number());
    assert_eq!(client.admitted_count(text), 1);
    assert_eq!(turns.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentication_gate_has_specific_recovery_and_resume_cannot_waive_it() {
    let provider = MockServer::start().await;
    let turns = Arc::new(AtomicUsize::new(0));
    let (responder, release) = AcceptedTurnResponder::new(Arc::clone(&turns));
    release.open();
    Mock::given(method("POST"))
        .respond_with(responder)
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config_with_second_model(&provider.uri()));
    pause_session(&client, SessionPauseReason::Authentication);
    client.prompt(3, "This request must wait for authentication.");
    let gated = client.responses(&[3]).remove(0);
    assert_eq!(gated["error"]["code"], -32005, "{gated}");
    assert_eq!(
        gated["error"]["data"]["receipt"]["executionGate"]["recovery"],
        "reauthenticate"
    );
    client.prompt(4, "/resume");
    let refused = client.responses(&[4]).remove(0);
    assert!(refused.get("error").is_some(), "{refused}");
    assert_eq!(turns.load(Ordering::SeqCst), 0);
}
