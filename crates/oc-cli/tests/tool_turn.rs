//! The proof the binary can execute a turn in which the model calls a tool.
//!
//! Todo 44 tested that the registry assembles the right tool set and wave 6 tested
//! that `run_turn` drives a tool loop, and both stayed green while `run` passed the
//! dispatcher an empty tool vector. Neither test could see that, because neither
//! went through a production entry point. These do: they launch the real
//! `opencode-rust run` binary against a cassette-backed provider and assert on what
//! the binary put on the wire and what it did to the filesystem.
//!
//! # Why two tests and not one
//!
//! [`tool_turn_offers_the_assembled_registry_and_continues_after_a_tool_result`]
//! replays `openai-chat/drives-a-tool-loop-end-to-end` byte for byte. Its recorded
//! call names `get_weather`, a tool this runtime does not have, so it proves the
//! two properties that do not need a matching implementation: the request carries
//! the assembled registry, and an unknown call still produces a tool result that
//! the loop sends back. Those are recorded provider bytes, so nothing about the
//! wire format is this repository's opinion.
//!
//! [`tool_turn_executes_a_real_tool_and_the_side_effect_lands_on_disk`] needs the
//! model to call a tool that exists, and no recording of this runtime's own tool
//! names can exist yet. It therefore rewrites the recorded stream's tool name and
//! arguments and declares the result authored, which is exactly the accounting
//! `oc-testkit` exists to force: the framing, chunk boundaries, finish reason and
//! usage frame are still the recorded ones, and only the two values that name a
//! tool are this repository's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use oc_testkit::mock_provider::{MockResponse, ResponseOrigin};
use oc_testkit::{CassettePlayer, MockProvider, Scenario, ScriptedEnv};

/// The recorded conversation both tests build on.
const CASSETTE: &str = "openai-chat/drives-a-tool-loop-end-to-end";

/// The recorded tool-free text completion that answers the prelude's title request.
///
/// The same recording todo 88's frozen harness uses for the same purpose
/// (`crates/oc-testkit/src/perf/workload.rs:93`), for the same reason: every run now
/// opens with exactly one tool-free request, because a new session generates its title
/// before its first real turn. That is what makes the harness's
/// `completed_tool_turns(captured) = (captured - PRELUDE_REQUESTS) / RESPONSES_PER_TURN`
/// come out at 1 for one tool turn instead of 0.
const TITLE_CASSETTE: &str = "openai-chat/streams-text";

/// `PRELUDE_REQUESTS` and `RESPONSES_PER_TURN` from the frozen harness, in that order.
///
/// Duplicated here rather than imported because `oc-testkit`'s copies are
/// crate-private and deliberately frozen — importing them would let a future edit to
/// one silently retune this assertion, which is the massaged pass three earlier agents
/// declined to produce. If these ever disagree with `perf/workload.rs:18-19`, the port
/// has changed its request shape and the perf gate is no longer measuring what it
/// measured from real 1.18.12 traffic.
const FROZEN_PRELUDE_REQUESTS: usize = 1;
const FROZEN_RESPONSES_PER_TURN: usize = 2;

/// Total provider requests one tool turn must produce for the frozen gate to score it.
const REQUESTS_FOR_ONE_TOOL_TURN: usize = FROZEN_PRELUDE_REQUESTS + FROZEN_RESPONSES_PER_TURN;

/// The tool the recording calls, which this runtime deliberately does not have.
const RECORDED_TOOL: &str = "get_weather";

/// Tools the assembled registry must offer a non-GPT model.
///
/// Not the whole list on purpose: `edit` and `write` are model-conditional and
/// `websearch` is provider-conditional, so pinning every id here would make this
/// test fail for a reason that has nothing to do with the wiring it exists to
/// check. These four are unconditional for every model and provider.
const REQUIRED_TOOLS: [&str; 4] = ["bash", "read", "glob", "grep"];

/// Wall-clock budget for one cassette-backed run. Everything it talks to is
/// loopback or the local filesystem, so exceeding this is a hang, not slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// What the executed `write` call must leave on disk.
const WRITTEN_CONTENT: &str = "the tool ran\n";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-rust"))
}

/// A config naming one OpenAI-compatible provider pointed at the mock.
///
/// It deliberately does **not** set `permission`. The turn must be governed by the
/// ruleset `agent list` prints, so an `"*": "allow"` override here would hide a
/// regression in which the real rules never reach the dispatcher.
fn provider_config(base_url: &str) -> String {
    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "npm": "@ai-sdk/openai-compatible",
                "api": format!("{base_url}/v1"),
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test Model",
                        "attachment": false,
                        "reasoning": false,
                        "temperature": false,
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": { "context": 100_000, "output": 10_000 },
                        "cost": { "input": 0, "output": 0 },
                        "options": {}
                    }
                },
                "options": {
                    "apiKey": "test-key",
                    "baseURL": format!("{base_url}/v1")
                }
            }
        }
    })
    .to_string()
}

fn variables(env: &ScriptedEnv, base_url: &str) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("OPENCODE_PURE".to_owned(), "1".to_owned()),
        ("OPENCODE_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        // No `OPENCODE_MODELS_PATH`: the config below fully specifies `test/test-model`,
        // so a catalog is not needed to resolve it. Injecting a fixture here is what hid
        // todo 108 — the binary could not start without one — through five waves.
        (
            "OPENCODE_DISABLE_MODELS_FETCH".to_owned(),
            "true".to_owned(),
        ),
        (
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            provider_config(base_url),
        ),
    ]);
    variables
}

/// Launch the real binary and wait for it, bounded.
///
/// `tokio::process` rather than `std::process` because the mock provider's server
/// runs on this test's runtime: a synchronous wait would stop driving it, the
/// response would never be written, and the run would hang rather than fail.
async fn run_prompt(env: &ScriptedEnv, base_url: &str, prompt: &str) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", prompt])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, base_url));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the run must finish inside its budget")
        .expect("launch opencode-rust run")
}

/// Every `tools[].function.name` the binary advertised in a captured request.
fn advertised_tools(body: &serde_json::Value) -> Vec<String> {
    body.get("tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/function/name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The frozen harness's own arithmetic, reproduced so a failure names the number.
const fn completed_tool_turns(captured_requests: usize) -> usize {
    captured_requests.saturating_sub(FROZEN_PRELUDE_REQUESTS) / FROZEN_RESPONSES_PER_TURN
}

fn has_tool_result(body: &serde_json::Value) -> bool {
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
            })
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_turn_offers_the_assembled_registry_and_continues_after_a_tool_result() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let scenario = Scenario::new("recorded-tool-loop")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded text completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded tool loop loads");
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");
    assert!(
        provider.authored_scenarios().is_empty(),
        "this test must replay recorded provider bytes only"
    );

    let output = run_prompt(&env, provider.base_url(), "What is the weather in Paris?").await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        captured.len(),
        REQUESTS_FOR_ONE_TOOL_TURN,
        "one tool turn must be one tool-free prelude request plus two turn requests; \
         the frozen gate scores {} completed turn(s) from {} captured",
        completed_tool_turns(captured.len()),
        captured.len()
    );
    assert_eq!(
        completed_tool_turns(captured.len()),
        1,
        "the frozen perf harness would still score this turn as incomplete"
    );

    let prelude = captured[0].json().expect("the prelude request is JSON");
    assert!(
        advertised_tools(&prelude).is_empty(),
        "the prelude request offered tools, but the title agent denies every one of \
         them:\nbody:\n{prelude:#}"
    );
    assert!(
        !has_tool_result(&prelude),
        "the prelude request is the first thing on the wire and cannot carry a tool \
         result:\n{prelude:#}"
    );

    let first = captured[1].json().expect("the first turn request is JSON");
    let offered = advertised_tools(&first);
    for required in REQUIRED_TOOLS {
        assert!(
            offered.iter().any(|name| name == required),
            "the request advertised {offered:?}, which does not include `{required}`; \
             the assembled registry did not reach the dispatcher\nbody:\n{first:#}"
        );
    }

    let second = captured[2].json().expect("the second turn request is JSON");
    assert!(
        has_tool_result(&second),
        "the second request carries no tool result:\n{second:#}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(RECORDED_TOOL),
        "the unknown recorded tool was not reported on stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Rewrite the recorded first response so the model calls `write` instead.
///
/// Every frame is parsed, edited and re-serialized rather than string-patched: the
/// recording streams the arguments across five frames, and a textual splice leaves
/// malformed JSON that the provider rejects before the loop is reached. The frame
/// sequence, the finish reason and the usage frame stay as recorded; the tool's
/// name and its arguments are the only edited values, which is what `reason`
/// declares to [`MockProvider::authored_scenarios`].
fn write_tool_call_response(recorded: &str, target: &Path) -> MockResponse {
    let arguments = serde_json::json!({
        "filePath": target.to_string_lossy(),
        "content": WRITTEN_CONTENT,
        "intent": "prove the tool executes",
    })
    .to_string();
    let mut rewritten = String::new();
    for frame in recorded.split("\n\n") {
        let Some(payload) = frame.strip_prefix("data: ") else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            rewritten.push_str("data: [DONE]\n\n");
            continue;
        }
        let mut chunk: serde_json::Value =
            serde_json::from_str(payload).expect("every recorded frame is JSON");
        let fragment = has_tool_call_fragment(&chunk);
        let named = chunk
            .pointer("/choices/0/delta/tool_calls/0/function/name")
            .is_some();
        if named {
            let call = chunk
                .pointer_mut("/choices/0/delta/tool_calls/0")
                .expect("the frame that names a function has the call");
            call["function"]["name"] = serde_json::Value::String("write".to_owned());
            call["function"]["arguments"] = serde_json::Value::String(arguments.clone());
        } else if fragment {
            continue;
        }
        rewritten.push_str("data: ");
        rewritten.push_str(&serde_json::to_string(&chunk).expect("the frame re-serializes"));
        rewritten.push_str("\n\n");
    }
    MockResponse::authored(
        200,
        "text/event-stream; charset=utf-8",
        rewritten,
        "no recording of a model calling this runtime's own tool names can exist \
         before this runtime has ever been driven by one; the frame sequence, \
         finish reason and usage frame are the recorded ones",
    )
}

/// Whether a frame carries an arguments-only `tool_calls` fragment.
fn has_tool_call_fragment(chunk: &serde_json::Value) -> bool {
    chunk.pointer("/choices/0/delta/tool_calls").is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_turn_executes_a_real_tool_and_the_side_effect_lands_on_disk() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let target = env.project().join("written-by-the-tool.txt");
    let player = CassettePlayer::from_oracle(CASSETTE).expect("the recorded tool loop loads");
    let mut interactions = player.cassette().http_interactions();
    let first = interactions.next().expect("the tool-call interaction");
    let second = interactions.next().expect("the continuation interaction");
    let recorded_first = String::from_utf8(
        first
            .response
            .decoded_body(CASSETTE, 1)
            .expect("the recorded body decodes"),
    )
    .expect("the recorded body is UTF-8");

    let scenario = Scenario::new("write-tool-loop")
        .on_path("/v1/chat/completions")
        .from_oracle_cassette(TITLE_CASSETTE)
        .expect("the recorded text completion loads")
        .respond(write_tool_call_response(&recorded_first, &target))
        .respond(
            MockResponse::from_recorded(CASSETTE, 2, second).expect("the continuation decodes"),
        );
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback");

    let output = run_prompt(&env, provider.base_url(), "Write the file.").await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        output.status.success(),
        "run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        captured.len(),
        REQUESTS_FOR_ONE_TOOL_TURN,
        "the turn did not continue after the tool result"
    );
    assert_eq!(
        std::fs::read_to_string(&target).ok().as_deref(),
        Some(WRITTEN_CONTENT),
        "the `write` tool did not run; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        captured[2]
            .json()
            .is_some_and(|body| has_tool_result(&body)),
        "the executed tool's result was not sent back to the model"
    );
    assert!(
        matches!(
            captured[1].served_origin.as_ref(),
            Some(ResponseOrigin::Authored { .. })
        ),
        "the rewritten first response must be reported as authored"
    );
}

#[test]
fn tui_refuses_a_non_terminal_invocation_and_names_the_headless_surface() {
    // The one property of the boot path that is assertable without a TTY, and the
    // one that matters most: entering raw mode on a pipe would write escape
    // sequences into whatever is reading it with no way to type the exit key.
    let env = ScriptedEnv::new().expect("isolated environment");
    let output = std::process::Command::new(binary())
        .arg("tui")
        .current_dir(env.working_dir())
        .env_clear()
        .envs(env.env_vars())
        .output()
        .expect("launch opencode-rust tui");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("terminal"), "{stderr}");
    assert!(stderr.contains("run"), "{stderr}");
}
