//! Production-path proof for durable OpenAI Responses session affinity.
//!
//! The test launches the real CLI against a loopback gateway. Its first request
//! is the isolated title prelude; the next two are one foreground tool loop. The
//! captured JSON therefore proves both halves of the contract: lifecycle-owned
//! requests carry no affinity, while every continuation of one durable session
//! carries the same non-model-visible identity.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde_json::{Value, json};
use zuno_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv};

const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const FINAL_MARKER: &str = "ZUNO_SESSION_AFFINITY_OK";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

fn responses(events: Vec<Value>, provenance: &'static str) -> MockResponse {
    let mut body = String::new();
    for event in events {
        let kind = event["type"].as_str().expect("Responses event type");
        body.push_str("event: ");
        body.push_str(kind);
        body.push_str("\ndata: ");
        body.push_str(&serde_json::to_string(&event).expect("serialize Responses event"));
        body.push_str("\n\n");
    }
    MockResponse::authored(200, "text/event-stream", body, provenance)
}

fn text_response(text: &str, provenance: &'static str) -> MockResponse {
    responses(
        vec![
            json!({"type":"response.output_text.delta","delta":text}),
            json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":3,"output_tokens":2}}
            }),
        ],
        provenance,
    )
}

fn read_tool_response(path: &Path) -> MockResponse {
    let arguments = json!({"filePath": path.to_string_lossy()}).to_string();
    responses(
        vec![
            json!({
                "type":"response.output_item.added",
                "item":{
                    "id":"item_read",
                    "type":"function_call",
                    "call_id":"call_read",
                    "name":"read",
                    "arguments":""
                }
            }),
            json!({
                "type":"response.function_call_arguments.delta",
                "item_id":"item_read",
                "delta":arguments
            }),
            json!({
                "type":"response.output_item.done",
                "item":{
                    "id":"item_read",
                    "type":"function_call",
                    "call_id":"call_read",
                    "name":"read",
                    "arguments":arguments
                }
            }),
            json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":5,"output_tokens":3}}
            }),
        ],
        "synthetic read call; OpenAI Responses framing is covered by provider cassette tests",
    )
}

fn provider_config(base_url: &str) -> String {
    json!({
        "formatter": false,
        "lsp": false,
        "model": "test/test-model",
        "small_model": "test/test-model",
        "memory": false,
        "permission": {"mode":"allow_all"},
        "provider": {
            "test": {
                "name": "Session affinity gateway",
                "id": "test",
                "env": [],
                "transport": "openai",
                "surface": "responses",
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2026-08-27",
                        "limit": {"context":100000,"output":10000},
                        "cost": {"input":0,"output":0},
                        "options": {}
                    }
                },
                "options": {"baseURL": format!("{base_url}/v1")}
            }
        }
    })
    .to_string()
}

fn variables(env: &ScriptedEnv, config: String) -> BTreeMap<String, String> {
    let mut variables = env.env_vars().into_iter().collect::<BTreeMap<_, _>>();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        (
            "ZUNO_AUTH_CONTENT".to_owned(),
            json!({"test":{"type":"api","key":"test-key"}}).to_string(),
        ),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

async fn run_prompt(env: &ScriptedEnv, config: String) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args([
            "run",
            "--agent",
            "build",
            "--model",
            "test/test-model",
            "read the fixture and finish",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, config));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the affinity run must finish inside its budget")
        .expect("launch zuno run")
}

fn describe(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_session_keeps_affinity_across_a_real_responses_tool_loop() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let fixture = env.working_dir().join("affinity-fixture.txt");
    std::fs::write(&fixture, "durable affinity fixture\n").expect("write read fixture");
    let scenario = Scenario::new("session-affinity")
        .respond(text_response(
            "Session affinity test",
            "synthetic title; title framing is not under test",
        ))
        .respond(read_tool_response(&fixture))
        .respond(text_response(
            FINAL_MARKER,
            "synthetic final answer; continuation routing is under test",
        ));
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let config = provider_config(provider.base_url());

    let output = run_prompt(&env, config).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(output.status.success(), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(FINAL_MARKER),
        "{}",
        describe(&output)
    );
    assert_eq!(
        captured.len(),
        3,
        "expected title, tool call, and continuation requests\n{}",
        describe(&output)
    );

    let bodies = captured
        .iter()
        .map(|request| request.json().expect("captured JSON body"))
        .collect::<Vec<_>>();
    assert!(
        bodies[0].get("metadata").is_none(),
        "title generation inherited foreground affinity: {}",
        bodies[0]
    );

    let first_id = bodies[1]["metadata"]["zuno_session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("foreground request carries no affinity: {bodies:#?}"));
    let second_id = bodies[2]["metadata"]["zuno_session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("tool continuation carries no affinity: {bodies:#?}"));
    assert!(first_id.starts_with("ses_"), "{first_id}");
    assert_eq!(
        first_id, second_id,
        "one durable session changed affinity across its tool continuation"
    );
    assert!(
        bodies[2]["input"].as_array().is_some_and(|items| items
            .iter()
            .any(|item| item["type"] == "function_call_output")),
        "the third request was not a real tool continuation: {}",
        bodies[2]
    );
    for body in &bodies[1..] {
        let model_visible = json!({
            "instructions": body.get("instructions"),
            "input": body.get("input")
        })
        .to_string();
        assert!(
            !model_visible.contains(first_id),
            "affinity leaked into model-visible content: {model_visible}"
        );
    }
}
