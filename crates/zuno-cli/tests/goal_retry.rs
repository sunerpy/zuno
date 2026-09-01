//! Production-path coverage for durable goal recovery after provider failure.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};
use zuno_testkit::{
    DbChoice, MockProvider, MockResponse, Scenario, ScriptedEnv, trusted_platform_config,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(30);

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

fn text_response(text: &str) -> MockResponse {
    let chunk = json!({
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": text},
            "finish_reason": null
        }]
    });
    let finish = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    MockResponse::authored(
        200,
        "text/event-stream",
        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "synthetic goal recovery marker; provider wire compatibility is covered elsewhere",
    )
}

fn tool_response(call_id: &str, name: &str, arguments: Value) -> MockResponse {
    let arguments = serde_json::to_string(&arguments).expect("serialize tool arguments");
    let call = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "function": {"name": name, "arguments": arguments}
                }]
            },
            "finish_reason": null
        }]
    });
    let finish = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    MockResponse::authored(
        200,
        "text/event-stream",
        format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "synthetic goal tool call; tool-loop framing is covered by provider cassette tests",
    )
}

fn transient_failure() -> MockResponse {
    MockResponse::authored(
        503,
        "application/json",
        br#"{"error":{"message":"temporary upstream outage","type":"server_error"}}"#.to_vec(),
        "deterministic retry exhaustion cannot be recorded from a live provider",
    )
}

fn provider_config(base_url: &str) -> String {
    trusted_platform_config(json!({
        "formatter": false,
        "lsp": false,
        "model": "test/test-model",
        "goal": {
            "retry": {
                "initial_delay_ms": 1,
                "max_delay_ms": 1,
                "jitter_percent": 0,
                "poll_interval_ms": 1
            }
        },
        "provider": {
            "test": {
                "name": "test",
                "id": "test",
                "env": [],
                "transport": "openai-compatible",
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "test-model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": {"context": 100000, "output": 10000},
                        "cost": {"input": 0, "output": 0},
                        "options": {}
                    }
                },
                "options": {
                    "apiKey": "goal-retry-probe",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    }))
    .to_string()
}

fn variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = env.env_vars().into_iter().collect::<BTreeMap<_, _>>();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), provider_config(base_url)),
    ]);
    variables
}

#[tokio::test]
async fn active_goal_survives_request_retry_exhaustion_and_completes_automatically() {
    let scenario = Scenario::new("durable-goal-retry")
        .respond(text_response("Goal retry probe"))
        .respond(tool_response(
            "create-goal",
            "goal_propose",
            json!({
                "objective": "finish after a transient provider outage"
            }),
        ))
        .respond(transient_failure())
        .respond(transient_failure())
        .respond(transient_failure())
        .respond(tool_response(
            "complete-plan",
            "plan_update",
            json!({
                "expected_revision": 1,
                "title": "Complete durable recovery probe",
                "steps": [
                    {"id":"investigate","title":"Inspect the recovery state","status":"completed"},
                    {"id":"execute","title":"Run the recovery attempt","status":"completed"},
                    {"id":"integrate","title":"Reconcile the durable state","status":"completed"},
                    {"id":"verify","title":"Verify automatic completion","status":"completed"}
                ]
            }),
        ))
        .respond(tool_response(
            "complete-goal",
            "goal_update",
            json!({
                // The failed turn records its usage after goal creation, advancing
                // the authoritative goal from revision 1 to revision 2.
                "expected_revision": 2,
                "status": "complete"
            }),
        ))
        .respond(text_response("GOAL-RECOVERED"));
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("bind loopback provider");
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let variables = variables(&env, provider.base_url());
    let goal_database = PathBuf::from(
        variables
            .get("ZUNO_DB")
            .expect("scripted file database path")
            .as_str(),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args([
            "run",
            "--format",
            "json",
            "--model",
            "test/test-model",
            "complete the durable recovery probe",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables);
    command.kill_on_drop(true);
    let output = match tokio::time::timeout(RUN_TIMEOUT, command.output()).await {
        Ok(output) => output.expect("launch production CLI"),
        Err(error) => {
            let request_sequence = provider
                .captured()
                .await
                .into_iter()
                .map(|request| {
                    format!(
                        "{} {} scenario={:?} served_index={:?}",
                        request.method, request.path, request.scenario, request.served_index
                    )
                })
                .collect::<Vec<_>>();
            panic!(
                "goal recovery must finish inside its budget: {error}; captured requests: {request_sequence:#?}"
            );
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "production CLI failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("active goal retry 1 is scheduled"),
        "{stdout}"
    );
    assert!(stdout.contains("GOAL-RECOVERED"), "{stdout}");
    assert_eq!(
        provider.captured_count().await,
        8,
        "title, goal creation, three request attempts, plan completion, goal completion, and final answer"
    );

    let retry_request = provider
        .captured()
        .await
        .get(5)
        .and_then(|request| request.json())
        .expect("automatic retry request is captured JSON");
    let retry_request = retry_request.to_string();
    assert!(
        retry_request.contains("recovery attempt 1")
            && retry_request.contains("before repeating an action with side effects"),
        "the retry request must carry the durable side-effect audit:\n{retry_request}"
    );

    let connection = Connection::open(&goal_database).expect("open the completed goal database");
    let status: String = connection
        .query_row("SELECT status FROM goal", [], |row| row.get(0))
        .expect("read final goal status");
    let retries: i64 = connection
        .query_row("SELECT count(*) FROM goal_retry", [], |row| row.get(0))
        .expect("count remaining retry plans");
    assert_eq!(status, "complete");
    assert_eq!(retries, 0, "completion must clear the durable retry plan");
}
