//! Production-path coverage for a goal that cannot resume until state is inspected.
//!
//! A call that changed authoritative state and then lost the result leaves a question
//! only that state can answer, so the harness must stop rather than keep working from a
//! guess. These tests drive the real binary: a real `shell` call whose child-process
//! guard failed reaches the real dispatcher, the real durable tool record, and the real
//! Goal accounting.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout, Stdio};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};
use zuno_testkit::{
    DbChoice, MockProvider, MockResponse, Scenario, ScriptedEnv, trusted_platform_config,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// The command whose reported exit status decides nothing about what it changed.
///
/// `exit 125` plus the guard's own diagnostic in the captured output is how
/// `zuno_process::GuardExit::from_reported_run` recognises that the guard's machinery
/// broke rather than that the command chose that code. Simulating it from the command
/// itself is what makes an uncertain outcome reproducible: a real guard failure needs a
/// host whose `pidfd_open` is refused, and that is not something a test can arrange.
const GUARD_FAILURE_COMMAND: &str =
    "printf 'child-process guard failed: pidfd_open: Permission denied\\n'; exit 125";

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
        "synthetic uncertain-outcome marker; provider wire compatibility is covered elsewhere",
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

fn provider_config(base_url: &str) -> String {
    trusted_platform_config(json!({
        "formatter": false,
        "lsp": false,
        "model": "test/test-model",
        // The uncertain outcome has to be the command's, not the sandbox's: a confined
        // backend that refuses to start the command never reaches the guard at all.
        "sandbox": {"mode": "danger-full-access"},
        "permission": {"mode": "allow_all"},
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
                        "options": {
                            "apiKey": "goal-uncertain-probe",
                            "baseURL": format!("{base_url}/v1")
                        }
                    }
                },
                "options": {
                    "apiKey": "goal-uncertain-probe",
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

/// The one pending uncertain tool record the session holds, as the surfaces read it.
fn pending_uncertain(connection: &Connection) -> Vec<Value> {
    let mut statement = connection
        .prepare(
            "SELECT data FROM part \
             WHERE json_extract(data, '$.state.outcome') = 'uncertain' \
             ORDER BY time_created ASC, id ASC",
        )
        .expect("prepare the uncertain tool-record query");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("read uncertain tool records")
        .map(|data| {
            serde_json::from_str::<Value>(&data.expect("uncertain part data"))
                .expect("uncertain part data is JSON")
        })
        .collect()
}

/// One line-delimited JSON-RPC round trip, returning the result and the updates.
///
/// A local copy rather than a shared helper: this file owns one ACP conversation, and
/// the point of driving it here is that the durable state it reads was produced by the
/// two production runs above rather than seeded by the test.
fn acp_request(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let frame: Value = serde_json::from_str(&line).expect("ACP frame JSON");
        if frame.get("id") == Some(&json!(id)) {
            // The frame is returned whole rather than asserted on: resuming a goal hands
            // it back to the continuation driver, and how that driver's own turn ends is
            // a different question from whether the resume retired the obligation.
            return (frame.clone(), updates);
        }
        if frame.get("method").and_then(Value::as_str) == Some("session/update") {
            updates.push(frame["params"]["update"].clone());
        }
    }
}

/// The JSON a native `/goal` prompt publishes as its command output.
///
/// The first chunk that parses, not the concatenation of every chunk: a prompt that
/// resumes the goal publishes the command output and then whatever the resumed turn
/// itself says, and only the first of those is this command's answer.
fn goal_command_output(updates: &[Value]) -> Value {
    updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .find_map(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| {
            panic!("a native /goal command must publish its JSON output: {updates:#?}")
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_side_effect_pauses_the_goal_and_survives_a_pause_that_was_never_written() {
    let scenario = Scenario::new("durable-uncertain-outcome")
        .respond(text_response("Uncertain outcome probe"))
        .respond(tool_response(
            "create-goal",
            "goal_propose",
            json!({"objective": "publish the release and confirm it landed"}),
        ))
        .respond(tool_response(
            "guard-failure",
            "shell",
            json!({"command": GUARD_FAILURE_COMMAND}),
        ))
        .respond(text_response("STOPPED-TO-INSPECT"));
    let provider = MockProvider::start(vec![scenario])
        .await
        .expect("bind loopback provider");
    let env = ScriptedEnv::new()
        .expect("isolated environment")
        .with_db(DbChoice::TempFile);
    let variables = variables(&env, provider.base_url());
    let database = PathBuf::from(
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
            "publish the release",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables.clone());
    command.kill_on_drop(true);
    let output = tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the first run must finish inside its budget")
        .expect("launch production CLI");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "production CLI failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The whole point of the pause: the goal stops after the call it cannot account
    // for, so nothing further is requested from the provider on its behalf.
    assert_eq!(
        provider.captured_count().await,
        4,
        "title, goal creation, the guard failure, and the model's own reply — and nothing after:\n{stdout}"
    );

    let connection = Connection::open(&database).expect("open the session database");
    let records = pending_uncertain(&connection);
    assert_eq!(records.len(), 1, "exactly one call owes an inspection");
    let uncertain = &records[0]["state"]["uncertain"];
    assert_eq!(uncertain["tool"], "shell");
    assert_eq!(uncertain["callID"], "guard-failure");
    assert_eq!(uncertain["cause"], "lost_outcome");
    // A guard that broke around the command never learned which paths it reached, so an
    // empty list is the honest answer rather than a missing one.
    assert_eq!(uncertain["appliedPaths"], json!([]));
    assert!(
        uncertain["observedAtMs"].as_i64().is_some_and(|at| at > 0),
        "the record must say when the outcome was observed: {uncertain}"
    );
    assert!(
        uncertain.get("reconciledAtMs").is_none(),
        "nobody inspected anything yet: {uncertain}"
    );

    let (status, pause): (String, Option<String>) = connection
        .query_row(
            "SELECT goal.status, goal_pause.reason FROM goal \
             LEFT JOIN goal_pause ON goal_pause.session_id = goal.session_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read the goal and its pause");
    assert_eq!(status, "paused");
    assert_eq!(
        pause.as_deref(),
        Some("uncertain_side_effect"),
        "an uncertain outcome must pause for inspection, not record ordinary progress"
    );
    let retries: i64 = connection
        .query_row("SELECT count(*) FROM goal_retry", [], |row| row.get(0))
        .expect("count durable retry plans");
    assert_eq!(
        retries, 0,
        "a call that may already have had its effect is never scheduled for replay"
    );

    let session_id: String = connection
        .query_row("SELECT id FROM session LIMIT 1", [], |row| row.get(0))
        .expect("read the session id");

    // Now the crash window. A process that died between persisting the tool record and
    // recording the pause leaves the obligation durable and the goal still active, and
    // the obligation is what has to win: the pause is recomputed from the record rather
    // than remembered from the turn that produced it.
    connection
        .execute("DELETE FROM goal_pause", [])
        .expect("erase the pause the turn recorded");
    connection
        .execute("UPDATE goal SET status = 'active'", [])
        .expect("restore the goal the crashed process left behind");
    drop(connection);

    let resumed = Scenario::new("uncertain-outcome-survives-restart")
        .respond(text_response("STILL-WAITING-ON-AUTHORITATIVE-STATE"));
    let restarted = MockProvider::start(vec![resumed])
        .await
        .expect("bind the second loopback provider");
    let mut variables = variables;
    variables.insert(
        "ZUNO_CONFIG_CONTENT".to_owned(),
        provider_config(restarted.base_url()),
    );

    let mut command = tokio::process::Command::new(binary());
    command
        .args([
            "run",
            "--format",
            "json",
            "--model",
            "test/test-model",
            "--session",
            &session_id,
            "what is the state of the release?",
        ])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables.clone());
    command.kill_on_drop(true);
    let output = tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the restarted run must finish inside its budget")
        .expect("relaunch production CLI");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "restarted CLI failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        restarted.captured_count().await,
        1,
        "the asked question is answered, and the goal is not driven a single step further:\n{stdout}"
    );

    let connection = Connection::open(&database).expect("reopen the session database");
    let (status, pause): (String, Option<String>) = connection
        .query_row(
            "SELECT goal.status, goal_pause.reason FROM goal \
             LEFT JOIN goal_pause ON goal_pause.session_id = goal.session_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read the goal and its pause after the restart");
    assert_eq!(status, "paused");
    assert_eq!(
        pause.as_deref(),
        Some("uncertain_side_effect"),
        "the durable record, not the lost pause, is what decides whether the goal may run"
    );
    let records = pending_uncertain(&connection);
    assert_eq!(records.len(), 1);
    assert!(
        records[0]["state"]["uncertain"]
            .get("reconciledAtMs")
            .is_none(),
        "a turn is not an inspection: only an explicit recovery action retires the call"
    );
    // The pause is only actionable if a human can see which call to inspect, and only
    // ends when they say they inspected it. Both of those are surfaces, so they are read
    // through one — and through durable state neither the test nor the surface authored.
    // One permanent provider failure, because the resumed turn has to end somewhere and
    // this test is not about where. What matters is that the turn happens at all: before
    // reconciliation the guard refused to start one, so a captured request is the proof
    // that the obligation was retired rather than merely reported.
    let recovered = Scenario::new("uncertain-outcome-recovery").respond(MockResponse::authored(
        400,
        "application/json",
        br#"{"error":{"message":"the resumed turn is out of scope here","type":"invalid_request_error"}}"#.to_vec(),
        "a deterministic permanent failure cannot be recorded from a live provider",
    ));
    let recovery_provider = MockProvider::start(vec![recovered])
        .await
        .expect("bind the recovery loopback provider");
    variables.insert(
        "ZUNO_CONFIG_CONTENT".to_owned(),
        provider_config(recovery_provider.base_url()),
    );

    let working_dir = env.working_dir().to_owned();
    let mut child = std::process::Command::new(binary())
        .arg("acp")
        .current_dir(env.working_dir())
        .env_clear()
        .envs(&variables)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start zuno acp against the same durable session");
    let mut acp_stdin = child.stdin.take().expect("ACP stdin");
    let mut acp_stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    let inspection = tokio::task::spawn_blocking(move || {
        acp_request(
            &mut acp_stdin,
            &mut acp_stdout,
            1,
            "initialize",
            json!({"protocolVersion": 1}),
        );
        acp_request(
            &mut acp_stdin,
            &mut acp_stdout,
            2,
            "session/load",
            json!({"sessionId": &session_id, "cwd": working_dir, "mcpServers": []}),
        );
        let (_, shown) = acp_request(
            &mut acp_stdin,
            &mut acp_stdout,
            3,
            "session/prompt",
            json!({
                "sessionId": &session_id,
                "prompt": [{"type": "text", "text": "/goal show"}]
            }),
        );
        let (_, resumed) = acp_request(
            &mut acp_stdin,
            &mut acp_stdout,
            4,
            "session/prompt",
            json!({
                "sessionId": &session_id,
                "prompt": [{"type": "text", "text": "/goal resume"}]
            }),
        );
        (goal_command_output(&shown), goal_command_output(&resumed))
    })
    .await
    .expect("drive the ACP recovery conversation");
    let _ = child.kill();
    let _ = child.wait();
    let (shown, resumed) = inspection;

    assert_eq!(shown["pause"]["reason"], "uncertain_side_effect");
    let pending = shown["pendingUncertainCalls"]
        .as_array()
        .expect("the status surface must name the calls that owe an inspection");
    assert_eq!(pending.len(), 1, "{shown}");
    assert_eq!(pending[0]["tool"], "shell");
    assert_eq!(pending[0]["callID"], "guard-failure");
    assert_eq!(pending[0]["cause"], "lost_outcome");
    assert_eq!(pending[0]["appliedPaths"], json!([]));

    assert_eq!(resumed["status"], "active");
    let reconciled = resumed["reconciledUncertainCalls"]
        .as_array()
        .expect("resuming must report what it retired");
    assert_eq!(reconciled.len(), 1, "{resumed}");
    assert_eq!(reconciled[0]["callID"], "guard-failure");
    assert_eq!(
        recovery_provider.captured_count().await,
        1,
        "an inspected goal runs again; before the resume the guard refused to start a turn"
    );

    let connection = Connection::open(&database).expect("reopen the session database");
    let records = pending_uncertain(&connection);
    assert_eq!(
        records.len(),
        1,
        "reconciliation records that an inspection happened; it never erases the evidence"
    );
    assert!(
        records[0]["state"]["uncertain"]["reconciledAtMs"]
            .as_i64()
            .is_some_and(|at| at > 0),
        "the explicit recovery action is what retires the obligation: {}",
        records[0]["state"]["uncertain"]
    );
}
