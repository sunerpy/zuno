//! Process-level proof that the main ACP path uses provider config and retains errors.
use super::*;

struct RetryFailureResponder {
    main_requests: Arc<AtomicUsize>,
    delay_replacement: bool,
}

impl Respond for RetryFailureResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider JSON");
        if !body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
        {
            return compatible_text_response("Synthetic title");
        }
        let attempt = self.main_requests.fetch_add(1, Ordering::SeqCst) + 1;
        let response = ResponseTemplate::new(503)
            .insert_header("x-request-id", "wire-request-retry-fixture")
            .set_body_json(json!({"error": {
                "message": "Synthetic unavailable; api_key=acp-probe",
                "code": "reasoning_replay_account_unavailable",
                "request_id": "wire-request-retry-fixture"
            }}));
        if self.delay_replacement && attempt > 1 {
            response.set_delay(Duration::from_secs(20))
        } else {
            response
        }
    }
}

fn config(base: &str, attempts: u32) -> String {
    let mut config: Value = serde_json::from_str(&config_with_second_model(base)).unwrap();
    config["default_agent"] = json!("build");
    config["provider"]["test"]["retry"] = json!({
        "max_attempts": attempts, "recovery_window_ms": 2000,
        "initial_delay_ms": 1, "max_delay_ms": 1, "jitter_percent": 0
    });
    config.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_provider_retry_uses_configured_single_attempt() {
    let provider = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(RetryFailureResponder {
            main_requests: attempts.clone(),
            delay_replacement: false,
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config(&provider.uri(), 1));
    client.prompt(10, "Inspect the synthetic retry fixture.");
    let response = client.responses(&[10]).remove(0);
    assert!(response.get("error").is_some(), "{response}");
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "main resolver must honor max_attempts=1"
    );
    let connection = rusqlite::Connection::open(client.root.path().join("zuno-acp.db")).unwrap();
    let maxima: Vec<u32> = connection
        .prepare(
            "SELECT json_extract(data,'$.maxAttempts') FROM event
         WHERE aggregate_id=?1 AND type='session.provider.attempt.1'
         AND json_extract(data,'$.status')='started'",
        )
        .unwrap()
        .query_map([&client.session_id], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(maxima, vec![1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_provider_retry_deadline_preserves_safe_http_failure_and_receipt() {
    let provider = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(RetryFailureResponder {
            main_requests: attempts.clone(),
            delay_replacement: true,
        })
        .mount(&provider)
        .await;
    let mut client = PromptClient::start(&config(&provider.uri(), 2));
    client.prompt(10, "Inspect the synthetic deadline fixture.");
    let response = client.responses(&[10]).remove(0);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let data = &response["error"]["data"];
    assert_eq!(data["admission"], "accepted", "{response}");
    assert_eq!(data["receipt"]["state"], "failed", "{response}");
    assert!(data["receipt"]["appliedAt"].is_number(), "{response}");
    let receipt_error = data["receipt"]["error"].as_str().expect("receipt error");
    for marker in [
        "provider retry deadline",
        "HTTP 503",
        "reasoning_replay_account_unavailable",
        "wire-request-retry-fixture",
    ] {
        assert!(receipt_error.contains(marker), "{receipt_error}");
    }
    assert!(!response.to_string().contains("acp-probe"), "{response}");
    let connection = rusqlite::Connection::open(client.root.path().join("zuno-acp.db")).unwrap();
    let recorded: String = connection
        .query_row(
            "SELECT data FROM event WHERE aggregate_id=?1 AND type='session.provider.request.1'
         AND json_extract(data,'$.errorKind')='provider_retry_deadline'",
            [&client.session_id],
            |row| row.get(0),
        )
        .unwrap();
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    assert_eq!(recorded["lastProviderFailure"]["status"], 503);
    assert_eq!(
        recorded["lastProviderFailure"]["requestID"],
        "wire-request-retry-fixture"
    );
    assert!(!recorded.to_string().contains("acp-probe"));
}
