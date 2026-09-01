//! Black-box contract for opt-in reasoning on the headless CLI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use serde_json::json;
use zuno_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv, trusted_platform_config};

const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const REASONING: &str = "VISIBLE_PROVIDER_REASONING";
const ANSWER: &str = "VISIBLE_FINAL_ANSWER";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

fn chat_response(reasoning: Option<&str>, text: &str, provenance: &'static str) -> MockResponse {
    let mut chunks = Vec::new();
    if let Some(reasoning) = reasoning {
        chunks.push(json!({
            "id": "chatcmpl-reasoning",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "reasoning_content": reasoning},
                "finish_reason": null
            }]
        }));
    }
    chunks.push(json!({
        "id": "chatcmpl-reasoning",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": null
        }]
    }));
    chunks.push(json!({
        "id": "chatcmpl-reasoning",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    }));
    let mut body = chunks
        .into_iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>();
    body.push_str("data: [DONE]\n\n");
    MockResponse::authored(200, "text/event-stream", body, provenance)
}

fn config(base_url: &str) -> String {
    trusted_platform_config(json!({
        "formatter": false,
        "lsp": false,
        "memory": false,
        "model": "test/test-model",
        "small_model": "test/test-model",
        "provider": {
            "test": {
                "name": "Reasoning fixture",
                "id": "test",
                "env": [],
                "transport": "openai-compatible",
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test model",
                        "attachment": false,
                        "reasoning": true,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2026-08-31",
                        "limit": {"context": 100000, "output": 10000},
                        "cost": {"input": 0, "output": 0},
                        "options": {}
                    }
                },
                "options": {
                    "apiKey": "reasoning-test-key",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    }))
    .to_string()
}

fn variables(env: &ScriptedEnv, config: String) -> BTreeMap<String, String> {
    let mut variables = env.env_vars().into_iter().collect::<BTreeMap<_, _>>();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

async fn run(env: &ScriptedEnv, config: String, extra_args: &[&str]) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .arg("run")
        .args(extra_args)
        .arg("What answer does the fixture provide?")
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, config));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("headless reasoning run stays within its budget")
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
async fn show_reasoning_is_opt_in_and_keeps_the_final_answer_on_stdout() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let scenario = Scenario::new("headless-reasoning")
        .respond(chat_response(
            None,
            "Reasoning test",
            "synthetic title response",
        ))
        .respond(chat_response(
            Some(REASONING),
            ANSWER,
            "synthetic reasoning response",
        ))
        .respond(chat_response(
            None,
            "Reasoning test",
            "synthetic title response",
        ))
        .respond(chat_response(
            Some(REASONING),
            ANSWER,
            "synthetic reasoning response",
        ));
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    let config = config(provider.base_url());

    let hidden = run(&env, config.clone(), &[]).await;
    let shown = run(&env, config, &["--show-reasoning"]).await;
    provider.shutdown().await;

    assert!(hidden.status.success(), "{}", describe(&hidden));
    assert!(
        String::from_utf8_lossy(&hidden.stdout).contains(ANSWER),
        "{}",
        describe(&hidden)
    );
    assert!(
        !String::from_utf8_lossy(&hidden.stderr).contains(REASONING),
        "{}",
        describe(&hidden)
    );

    assert!(shown.status.success(), "{}", describe(&shown));
    let stdout = String::from_utf8_lossy(&shown.stdout);
    let stderr = String::from_utf8_lossy(&shown.stderr);
    assert!(stdout.contains(ANSWER), "{}", describe(&shown));
    assert!(!stdout.contains(REASONING), "{}", describe(&shown));
    assert!(stderr.contains("<<<zuno:reasoning>>>"), "{stderr}");
    assert!(stderr.contains(REASONING), "{stderr}");
    assert!(stderr.contains("<<<zuno:end-reasoning>>>"), "{stderr}");
}

#[tokio::test]
async fn show_reasoning_rejects_json_before_starting_a_provider_request() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let output = run(
        &env,
        "{}".to_owned(),
        &["--show-reasoning", "--format", "json"],
    )
    .await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--show-reasoning cannot be combined"),
        "{stderr}"
    );
}
